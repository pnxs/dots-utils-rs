//! DOTS trace tool.
//!
//! Connects to a broker, asks for every type's descriptor, and prints
//! every transmission that arrives — both internal DOTS lifecycle
//! traffic (DotsClient, DotsMember, DotsCacheInfo, …) and any user
//! types the broker knows about. Demonstrates the dynamic-client API
//! end to end:
//!
//! - [`App::new`] for the default endpoint (`tcp://127.0.0.1:11235`),
//!   overridable via the `DOTS_ENDPOINT` env var,
//! - [`App::publish`] of `DotsDescriptorRequest` to ask the broker to
//!   stream every cached descriptor,
//! - [`App::subscribe_all_types`] for one handler that fires for
//!   every transmission of every learned type,
//! - [`StructDisplay`] for human-readable payload output that expands
//!   `any` fields inline (the contained object is decoded against the
//!   registry and printed in place, rather than shown as an opaque
//!   `any<Type>[N bytes]` blob).
//!
//! Output is colored (dots-cpp's trace schema: CREATE green, UPDATE
//! yellow, REMOVE red, type names blue, values by kind) when stdout is
//! a terminal; piping the output or setting `NO_COLOR` disables it.
//!
//! ```text
//! ./dotsd                                              # in one terminal
//! cargo run --bin dots-trace                           # default tcp://127.0.0.1:11235
//! DOTS_ENDPOINT=uds:///tmp/dotsd.sock cargo run --bin dots-trace
//! ```
//!
//! Override the log level via `RUST_LOG`, e.g.
//! `RUST_LOG=dots_rs_transport=debug cargo run --bin dots-trace`.

use std::collections::BTreeSet;
use std::io::IsTerminal;

use chrono::{DateTime, Local, SecondsFormat};
use dots_rs_core::Timepoint;
use dots_rs_model::DotsDescriptorRequest;
use dots_rs_transport::{App, ColorSchema, Operation, StructDisplay};

const CLIENT_NAME: &str = "dots-trace";

/// Render a timepoint as local-time RFC 3339 with millisecond
/// precision, e.g. `2026-07-05T18:53:05.296+03:00`.
///
/// Falls back to the raw seconds value for timepoints outside
/// chrono's representable range (±262,000 years).
fn format_timepoint(tp: Timepoint) -> String {
    DateTime::from_timestamp_millis((tp.as_secs_f64() * 1000.0).round() as i64)
        .map(|utc| {
            utc.with_timezone(&Local)
                .to_rfc3339_opts(SecondsFormat::Millis, false)
        })
        .unwrap_or_else(|| format!("{}s", tp.as_secs_f64()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dots_rs_transport::init_tracing("");

    let app = App::new(CLIENT_NAME).await?;

    // Ask the broker to stream every cached descriptor — this is what
    // turns dots-trace into a *dynamic* client. Without it, we'd only
    // see types whose descriptors arrived during preload.
    app.publish(&DotsDescriptorRequest::default());

    // The shared registry lets the handler expand `any` fields inline:
    // the contained object is decoded and printed in place rather than
    // shown as an opaque `any<Type>[N bytes]` blob.
    let registry = app.registry().clone();

    // dots-cpp's trace color schema when stdout is a terminal; honor
    // the NO_COLOR convention (set and non-empty disables colors).
    let colors: Option<&'static ColorSchema> = (std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
    .then_some(&ColorSchema::TRACE);

    let exclude_types: BTreeSet<String> = BTreeSet::from([
        "DotsCacheInfo".into(),
        "StructDescriptorData".into(),
        "EnumDescriptorData".into(),
        "DotsClient".into()
    ]);

    // One handler for every type — the composite helper auto-installs
    // a dynamic subscription per descriptor (now and as new ones land).
    let _all = app.subscribe_all_types(move |event| {
        let type_name = &event.transmitted.descriptor.name;

        if exclude_types.contains(type_name) {
            return;
        }

        // 2026-07-05T18:53:05.296+03:00 UPDATE LcdCustomChar <slot:0 rows:[0, 0, 1, 2, 20, 8, 0, 0] >
        let timestamp = event
            .header
            .sent_time
            .map(format_timepoint)
            .unwrap_or_else(|| "<no sent_time>".into());
        let op = match event.operation {
            Operation::Create => "CREATE",
            Operation::Update => "UPDATE",
            Operation::Remove => "REMOVE",
        };

        let off = colors.map_or("", |c| c.all_off);
        let ts_color = colors.map_or("", |c| c.timestamp);
        let type_color = colors.map_or("", |c| c.type_name);
        let op_color = colors.map_or("", |c| match event.operation {
            Operation::Create => c.create,
            Operation::Update => c.update,
            Operation::Remove => c.remove,
        });
        let payload = match colors {
            Some(cs) => StructDisplay::colored(&registry, event.updated(), cs).to_string(),
            None => StructDisplay::new(&registry, event.updated()).to_string(),
        };

        println!(
            "{ts_color}{timestamp}{off} {op_color}{op}{off} {type_color}{type_name}{off} {payload}"
        );
    });

    eprintln!("subscribed to every known type; press Ctrl-C to exit.");
    app.run_until_signal().await?;
    eprintln!("exited.");
    Ok(())
}
