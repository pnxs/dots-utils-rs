//! Terminal UI for inspecting the DOTS broker.
//!
//! Connects to dotsd, subscribes to [`DotsClient`] and
//! [`DotsClientStatistics`], and renders a live table of the connected
//! guests and their per-client write/read counters.
//!
//! The broker republishes statistics every 5s (configurable on the
//! dotsd side via `--stats-interval`), so the table is paced by the
//! broker's snapshot cadence — we just redraw on a fixed local tick to
//! pick up whichever updates the dispatcher has integrated into the
//! containers since the last frame.
//!
//! Endpoint: same as every other dots-rust client — `DOTS_ENDPOINT`
//! env var, falling back to `tcp://127.0.0.1:11235`. Quit with `q` or
//! `Ctrl-C`.

use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Entry;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use tokio::time::{MissedTickBehavior, interval};

use dots_rs_model::{DotsClient, DotsClientStatistics, DotsConnectionState};
use dots_rs_transport::{App, Container};

const CLIENT_NAME: &str = "dots-tui";
const TICK: Duration = Duration::from_millis(100);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect first — surfacing connect errors to the terminal in the
    // normal scrollback is friendlier than catching them after we've
    // already switched to the alternate screen.
    let app = App::new(CLIENT_NAME).await?;
    let clients = app.container::<DotsClient>();
    let stats = app.container::<DotsClientStatistics>();
    let client = app.client();

    // Types screen plumbing — populate as descriptors flow in, then
    // mutate per-type instance sets / op state from every dispatched
    // event. Two subscriptions: `subscribe_new_struct_type` so a type
    // appears in the table the moment its descriptor is known
    // (instances = 0 until traffic shows up); `subscribe_all_types`
    // for the actual event firehose.
    let types: TypeRegistry = Arc::new(Mutex::new(HashMap::new()));
    let types_for_new = types.clone();
    let _new_type_sub = app.subscribe_new_struct_type(move |desc| {
        let mut r = types_for_new.lock().expect("types registry poisoned");
        r.entry(desc.name.clone()).or_insert_with(|| TypeStats {
            cached: desc.flags.is_cached(),
            internal: desc.flags.is_internal(),
            instances: HashSet::new(),
            last_op: None,
        });
    });
    let types_for_events = types.clone();
    let _all_types_sub = app.subscribe_all_types(move |event| {
        let Some(type_name) = event.header.type_name.as_deref() else {
            return;
        };
        let key = event.updated().key_bytes();
        let is_remove = event.header.remove_obj == Some(true);
        let from_cache = event.header.from_cache.is_some();

        let mut r = types_for_events.lock().expect("types registry poisoned");
        let s = r.entry(type_name.to_string()).or_insert_with(|| TypeStats {
            cached: event.updated().descriptor.flags.is_cached(),
            internal: event.updated().descriptor.flags.is_internal(),
            instances: HashSet::new(),
            last_op: None,
        });
        let op = if is_remove {
            s.instances.remove(&key);
            Op::Remove
        } else if s.instances.contains(&key) {
            Op::Update
        } else {
            s.instances.insert(key);
            Op::Publish
        };
        // Cache replay shouldn't flash the activity light — it's
        // historical traffic, not a live event. The instance set
        // still gets populated above so counts reflect the cache
        // state once preload completes.
        if !from_cache {
            s.last_op = Some((op, Instant::now()));
        }
    });

    let driver = tokio::spawn(async move { app.run().await });

    let mut terminal = setup_terminal()?;
    let ui_result = run_ui(&mut terminal, &clients, &stats, &types).await;
    restore_terminal(&mut terminal)?;

    client.exit();
    let _ = driver.await;

    ui_result
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

async fn run_ui(
    terminal: &mut Term,
    clients: &Container<DotsClient>,
    stats: &Container<DotsClientStatistics>,
    types: &TypeRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = EventStream::new();
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut rates = RateTracker::default();
    let mut view = View::Clients;

    loop {
        draw(terminal, view, clients, stats, types, &mut rates)?;

        tokio::select! {
            _ = ticker.tick() => {}
            maybe = events.next() => {
                match maybe {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) || ctrl_c {
                            return Ok(());
                        }
                        match key.code {
                            KeyCode::Tab | KeyCode::BackTab => view = view.toggle(),
                            KeyCode::Char('1') => view = View::Clients,
                            KeyCode::Char('2') => view = View::Types,
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(Box::new(e)),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn draw(
    terminal: &mut Term,
    view: View,
    clients: &Container<DotsClient>,
    stats: &Container<DotsClientStatistics>,
    types: &TypeRegistry,
    rates: &mut RateTracker,
) -> io::Result<()> {
    let client_rows = snapshot_rows(clients, stats, rates);
    let type_rows = match view {
        View::Types => snapshot_types(types),
        View::Clients => Vec::new(),
    };

    terminal.draw(|frame| {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        frame.render_widget(Paragraph::new(title_bar(view, &client_rows, &type_rows)), chunks[0]);

        match view {
            View::Clients => frame.render_widget(table(&client_rows), chunks[1]),
            View::Types => frame.render_widget(types_table(&type_rows), chunks[1]),
        }

        let footer = Line::from(vec![
            keycap(" Tab "),
            Span::styled(" switch view  ", Style::default().fg(Color::DarkGray)),
            keycap(" 1 "),
            Span::styled(" clients  ", Style::default().fg(Color::DarkGray)),
            keycap(" 2 "),
            Span::styled(" types  ", Style::default().fg(Color::DarkGray)),
            keycap(" q "),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    })?;
    Ok(())
}

fn keycap(label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default().fg(Color::Black).bg(Color::DarkGray),
    )
}

fn title_bar(view: View, client_rows: &[RowData], type_rows: &[TypeRow]) -> Line<'static> {
    let tab = |label: &str, active: bool| {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!(" {label} "),
                Style::default().fg(Color::Gray).bg(Color::Reset),
            )
        }
    };
    let mut spans = vec![
        Span::styled(
            " dots-tui ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        tab("1 clients", matches!(view, View::Clients)),
        Span::raw(" "),
        tab("2 types", matches!(view, View::Types)),
        Span::raw("  "),
    ];
    match view {
        View::Clients => {
            let active = client_rows
                .iter()
                .filter(|r| r.state.is_some_and(is_active))
                .count();
            spans.push(Span::styled(
                format!("{active}"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("/", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                format!("{}", client_rows.len()),
                Style::default().fg(Color::White),
            ));
            spans.push(Span::styled(
                " clients connected",
                Style::default().fg(Color::DarkGray),
            ));
        }
        View::Types => {
            let user_types: usize = type_rows.iter().filter(|t| !t.internal).count();
            let total_instances: usize = type_rows
                .iter()
                .filter(|t| !t.internal)
                .map(|t| t.instance_count)
                .sum();
            spans.push(Span::styled(
                format!("{user_types}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                " user types, ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                format!("{total_instances}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                " cached instances",
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    Line::from(spans)
}

const HEADERS: &[&str] = &[
    "ID",
    "Name",
    "State",
    "Sent B",
    "Sent Pkt",
    "Tx/s",
    "Recv B",
    "Recv Pkt",
    "Rx/s",
    "Queued B",
    "Peak Q B",
    "Peak Q Frm",
    "Wakeups",
];

const COLUMNS: &[Constraint] = &[
    Constraint::Length(6),
    Constraint::Min(12),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(8),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(8),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(11),
    Constraint::Length(10),
];

fn table(rows: &[RowData]) -> Table<'_> {
    let header = Row::new(HEADERS.iter().copied().map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }))
    .height(1);

    let body: Vec<Row> = rows.iter().map(render_row).collect();

    Table::new(body, COLUMNS)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " clients ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .column_spacing(1)
}

fn render_row(r: &RowData) -> Row<'static> {
    let active = r.state.is_some_and(is_active);
    let dim_closed = !active && !r.overflow;

    // Per-row base style: dim closed rows, otherwise default. Per-cell
    // styles below add color on top; closed rows still get to keep
    // their hues but the DIM modifier softens them via the terminal.
    let row_style = if dim_closed {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };

    Row::new([
        Cell::from(Span::styled(r.id.to_string(), Style::default().fg(Color::Cyan))),
        name_cell(&r.name),
        Cell::from(state_span(r.state)),
        Cell::from(Span::styled(
            fmt_bytes(r.sent_bytes),
            Style::default().fg(Color::LightBlue),
        )),
        Cell::from(Span::styled(
            fmt_u64(r.sent_pkts),
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::DIM),
        )),
        Cell::from(Span::styled(
            fmt_pps(r.sent_pps),
            rate_style(r.sent_pps, Color::LightBlue),
        )),
        Cell::from(Span::styled(
            fmt_bytes(r.recv_bytes),
            Style::default().fg(Color::LightGreen),
        )),
        Cell::from(Span::styled(
            fmt_u64(r.recv_pkts),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::DIM),
        )),
        Cell::from(Span::styled(
            fmt_pps(r.recv_pps),
            rate_style(r.recv_pps, Color::LightGreen),
        )),
        Cell::from(Span::styled(
            fmt_bytes(r.queued_bytes),
            pressure_style(r.queued_bytes),
        )),
        Cell::from(Span::styled(
            fmt_bytes(r.peak_queued_bytes),
            pressure_style(r.peak_queued_bytes).add_modifier(Modifier::DIM),
        )),
        Cell::from(Span::styled(
            fmt_u64(r.peak_queued_frames.into()),
            Style::default().fg(Color::Gray),
        )),
        Cell::from(Span::styled(
            fmt_u64(r.drainer_wakeups),
            Style::default().fg(Color::Gray),
        )),
    ])
    .style(row_style)
}

fn name_cell(name: &Option<String>) -> Cell<'static> {
    match name {
        Some(s) if !s.is_empty() => Cell::from(Span::styled(
            s.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Some(_) => Cell::from(Span::styled(
            "<empty>",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        )),
        None => Cell::from(Span::styled(
            "<no name>",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        )),
    }
}

fn state_span(state: Option<DotsConnectionState>) -> Span<'static> {
    let (label, color, bold) = match state {
        Some(DotsConnectionState::Connected) => ("connected", Color::Green, true),
        Some(DotsConnectionState::EarlySubscribe) => ("early", Color::Yellow, false),
        Some(DotsConnectionState::Connecting) => ("connecting", Color::Yellow, false),
        Some(DotsConnectionState::Suspended) => ("suspended", Color::Magenta, false),
        Some(DotsConnectionState::Closed) => ("closed", Color::DarkGray, false),
        None => ("—", Color::DarkGray, false),
    };
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(label, style)
}


/// Colour ramp for backlog bytes. Thresholds are conservative — the
/// transport's per-guest queue starts mattering well before 1 MiB, but
/// anything > 0 is worth a hint of yellow so the operator notices.
fn pressure_style(bytes: u64) -> Style {
    if bytes == 0 {
        Style::default().fg(Color::DarkGray)
    } else if bytes < 64 * 1024 {
        Style::default().fg(Color::Yellow)
    } else if bytes < 1024 * 1024 {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD)
    }
}

struct RowData {
    id: u32,
    /// Preserved as `Option` (rather than collapsed to `""`) so the
    /// UI can distinguish three states diagnostically: a real name,
    /// an empty string that the connecting client supplied, and a
    /// missing-from-publish `None`. dotsd's transition handler always
    /// includes `name` when the guest sent a `client_name` on
    /// `DotsMsgConnect`, so `<no name>` on a fresh row means the
    /// guest connected without supplying one.
    name: Option<String>,
    state: Option<DotsConnectionState>,
    sent_bytes: u64,
    sent_pkts: u64,
    sent_pps: f64,
    recv_bytes: u64,
    recv_pkts: u64,
    recv_pps: f64,
    queued_bytes: u64,
    peak_queued_bytes: u64,
    peak_queued_frames: u32,
    drainer_wakeups: u64,
    overflow: bool,
}

fn snapshot_rows(
    clients: &Container<DotsClient>,
    stats: &Container<DotsClientStatistics>,
    rates: &mut RateTracker,
) -> Vec<RowData> {
    let mut clients_snap: Vec<DotsClient> = Vec::new();
    clients.for_each(|_, c, _| clients_snap.push(c.clone()));
    let mut stats_snap: HashMap<u32, DotsClientStatistics> = HashMap::new();
    stats.for_each(|_, s, _| {
        if let Some(id) = s.client_id {
            stats_snap.insert(id, s.clone());
        }
    });

    let mut alive: HashSet<u32> = HashSet::with_capacity(clients_snap.len());
    let mut rows: Vec<RowData> = clients_snap
        .into_iter()
        .filter_map(|c| {
            let id = c.id?;
            alive.insert(id);
            let stat = stats_snap.get(&id);
            // `DotsClientStatistics` is broker-centric: its `sent`
            // counts broker→guest traffic, `received` counts
            // guest→broker. The TUI presents the *guest* view, so we
            // swap: tx columns read from `received`, rx columns from
            // `sent`. See `GuestStats` doc in host.rs and dotsd's
            // mapping at crates/dotsd/src/main.rs:142.
            let sent_pkts = stat
                .and_then(|s| s.received.as_ref().and_then(|d| d.packages))
                .unwrap_or(0);
            let recv_pkts = stat
                .and_then(|s| s.sent.as_ref().and_then(|d| d.packages))
                .unwrap_or(0);
            let (sent_pps, recv_pps) = rates.observe(id, sent_pkts, recv_pkts);
            Some(RowData {
                id,
                name: c.name,
                state: c.connection_state,
                sent_bytes: stat
                    .and_then(|s| s.received.as_ref().and_then(|d| d.bytes))
                    .unwrap_or(0),
                sent_pkts,
                sent_pps,
                recv_bytes: stat
                    .and_then(|s| s.sent.as_ref().and_then(|d| d.bytes))
                    .unwrap_or(0),
                recv_pkts,
                recv_pps,
                queued_bytes: stat.and_then(|s| s.current_queued_bytes).unwrap_or(0),
                peak_queued_bytes: stat.and_then(|s| s.peak_queued_bytes).unwrap_or(0),
                peak_queued_frames: stat.and_then(|s| s.peak_queued_frames).unwrap_or(0),
                drainer_wakeups: stat.and_then(|s| s.drainer_wakeups).unwrap_or(0),
                overflow: stat
                    .and_then(|s| s.overflow_disconnected)
                    .unwrap_or(false),
            })
        })
        .collect();
    rates.forget(&alive);
    rows.sort_by_key(|r| r.id);
    rows
}

/// Per-client packet-rate tracker. The broker republishes
/// `DotsClientStatistics` only every ~5s (default `--stats-interval`),
/// while the UI ticks at ~10 Hz — so we hold the previously-computed
/// rate steady between snapshots and recompute only when one of the
/// cumulative counters actually advances. Without this gating, the
/// displayed rate would flicker to zero for most ticks and to a
/// 5-second-averaged spike on each broker publish.
#[derive(Default)]
struct RateTracker {
    states: HashMap<u32, RateState>,
}

struct RateState {
    last_at: Instant,
    last_sent_pkts: u64,
    last_recv_pkts: u64,
    sent_pps: f64,
    recv_pps: f64,
}

impl RateTracker {
    fn observe(&mut self, id: u32, sent_pkts: u64, recv_pkts: u64) -> (f64, f64) {
        let now = Instant::now();
        match self.states.entry(id) {
            Entry::Vacant(v) => {
                v.insert(RateState {
                    last_at: now,
                    last_sent_pkts: sent_pkts,
                    last_recv_pkts: recv_pkts,
                    sent_pps: 0.0,
                    recv_pps: 0.0,
                });
                (0.0, 0.0)
            }
            Entry::Occupied(mut o) => {
                let s = o.get_mut();
                if sent_pkts != s.last_sent_pkts || recv_pkts != s.last_recv_pkts {
                    // `max(1ms)` guards against a degenerate `dt = 0`
                    // if the broker double-publishes within the same
                    // tick. saturating_sub guards against a counter
                    // reset (e.g. broker restart while we stayed up).
                    let dt = now.duration_since(s.last_at).as_secs_f64().max(1e-3);
                    s.sent_pps = sent_pkts.saturating_sub(s.last_sent_pkts) as f64 / dt;
                    s.recv_pps = recv_pkts.saturating_sub(s.last_recv_pkts) as f64 / dt;
                    s.last_sent_pkts = sent_pkts;
                    s.last_recv_pkts = recv_pkts;
                    s.last_at = now;
                }
                (s.sent_pps, s.recv_pps)
            }
        }
    }

    fn forget(&mut self, alive: &HashSet<u32>) {
        self.states.retain(|id, _| alive.contains(id));
    }
}

fn is_active(state: DotsConnectionState) -> bool {
    !matches!(state, DotsConnectionState::Closed)
}

fn fmt_u64(n: u64) -> String {
    if n == 0 {
        "0".to_string()
    } else {
        // Group thousands with underscores for readability.
        let s = n.to_string();
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len() + s.len() / 3);
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 && (bytes.len() - i) % 3 == 0 {
                out.push('_');
            }
            out.push(*b as char);
        }
        out
    }
}

fn fmt_pps(pps: f64) -> String {
    if pps < 0.05 {
        String::new()
    } else if pps < 10.0 {
        format!("{pps:.1}/s")
    } else if pps < 1000.0 {
        format!("{pps:.0}/s")
    } else if pps < 1_000_000.0 {
        format!("{:.1}k/s", pps / 1000.0)
    } else {
        format!("{:.1}M/s", pps / 1_000_000.0)
    }
}

fn rate_style(pps: f64, hue: Color) -> Style {
    if pps < 0.05 {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(hue).add_modifier(Modifier::BOLD)
    }
}

fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

// ===== Types view =====

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Clients,
    Types,
}

impl View {
    fn toggle(self) -> Self {
        match self {
            View::Clients => View::Types,
            View::Types => View::Clients,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Publish,
    Update,
    Remove,
}

struct TypeStats {
    cached: bool,
    internal: bool,
    /// Encoded key bytes of every instance currently in cache. Counted
    /// in via [`Op::Publish`] / [`Op::Update`] and out via
    /// [`Op::Remove`]. For non-cached types this set still tracks
    /// "keys seen at least once" but its size is meaningless to the
    /// user — see `instance_count` in `TypeRow` for the rendering
    /// rule.
    instances: HashSet<Vec<u8>>,
    last_op: Option<(Op, Instant)>,
}

type TypeRegistry = Arc<Mutex<HashMap<String, TypeStats>>>;

struct TypeRow {
    name: String,
    cached: bool,
    internal: bool,
    instance_count: usize,
    last_op: Option<(Op, Instant)>,
}

fn snapshot_types(registry: &TypeRegistry) -> Vec<TypeRow> {
    let r = registry.lock().expect("types registry poisoned");
    let mut rows: Vec<TypeRow> = r
        .iter()
        .map(|(name, s)| TypeRow {
            name: name.clone(),
            cached: s.cached,
            internal: s.internal,
            instance_count: s.instances.len(),
            last_op: s.last_op,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

const TYPE_HEADERS: &[&str] = &["Type", "Cached", "Instances", "Activity"];

const TYPE_COLUMNS: &[Constraint] = &[
    Constraint::Min(20),
    Constraint::Length(7),
    Constraint::Length(11),
    Constraint::Length(10),
];

fn types_table(rows: &[TypeRow]) -> Table<'_> {
    let header = Row::new(TYPE_HEADERS.iter().copied().map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }))
    .height(1);

    let body: Vec<Row> = rows
        .iter()
        .filter(|r| !r.internal)
        .map(render_type_row)
        .collect();

    Table::new(body, TYPE_COLUMNS)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " user types ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .column_spacing(1)
}

fn render_type_row(r: &TypeRow) -> Row<'static> {
    let cached_cell = if r.cached {
        Cell::from(Span::styled(
            "cached",
            Style::default().fg(Color::Green),
        ))
    } else {
        Cell::from(Span::styled(
            "event",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))
    };
    let instance_cell = if r.cached {
        Cell::from(Span::styled(
            r.instance_count.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Cell::from(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        ))
    };
    Row::new([
        Cell::from(Span::styled(
            r.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        cached_cell,
        instance_cell,
        Cell::from(activity_span(r.last_op)),
    ])
}

/// Map the most-recent op into a coloured glyph, fading as the event
/// recedes into the past. Decay window picked to align with the UI's
/// ~100 ms tick: bright for 250 ms, lit-but-not-bold for another
/// 600 ms, then back to a dim gray dot once the event is "stale".
fn activity_span(last_op: Option<(Op, Instant)>) -> Span<'static> {
    let Some((op, when)) = last_op else {
        return Span::styled("·", Style::default().fg(Color::DarkGray));
    };
    let age = when.elapsed();
    let color = match op {
        Op::Publish => Color::Green,
        Op::Update => Color::Yellow,
        Op::Remove => Color::Red,
    };
    if age < Duration::from_millis(250) {
        Span::styled(
            "●",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    } else if age < Duration::from_millis(850) {
        Span::styled("●", Style::default().fg(color))
    } else {
        Span::styled("·", Style::default().fg(Color::DarkGray))
    }
}
