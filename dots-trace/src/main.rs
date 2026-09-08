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
//!   `any<Type>[N bytes]` blob). Generic `instance_ref` and typed
//!   `instance_ref<T>` fields render as `Target["key",42]`, including
//!   in vectors and expanded `any` objects. Reference targets do not
//!   need to be registered or cached to display their keys.
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
use dots_rs::DotsDescriptorRequest;
use dots_rs::Timepoint;
use dots_rs::{App, ColorSchema, DynamicStruct, Operation, Registry, StructDisplay};

const CLIENT_NAME: &str = "dots-trace";

fn format_payload(
    registry: &Registry,
    value: &DynamicStruct,
    colors: Option<&ColorSchema>,
) -> String {
    // dots-rs 0.1.6 resolves typed instance references from the field
    // descriptor, including when nested in vectors or `any` objects.
    match colors {
        Some(cs) => StructDisplay::colored(registry, value, cs).to_string(),
        None => StructDisplay::new(registry, value).to_string(),
    }
}

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
    dots_rs::init_tracing("");

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
        "DotsClient".into(),
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
        let payload = format_payload(&registry, event.updated(), colors);

        println!(
            "{ts_color}{timestamp}{off} {op_color}{op}{off} {type_color}{type_name}{off} {payload}"
        );
    });

    eprintln!("subscribed to every known type; press Ctrl-C to exit.");
    app.run_until_signal().await?;
    eprintln!("exited.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dots_rs::{StructDescriptorData, StructPropertyData};

    fn decode(registry: &Registry, name: &str, kind: &str, payload: &[u8]) -> DynamicStruct {
        let descriptor = registry
            .build_dynamic_struct(&StructDescriptorData {
                name: Some(name.into()),
                properties: Some(vec![StructPropertyData {
                    name: Some("subject".into()),
                    tag: Some(1),
                    is_key: Some(false),
                    type_name: Some(kind.into()),
                    type_id: None,
                }]),
                ..Default::default()
            })
            .unwrap();
        let descriptor = std::sync::Arc::new(descriptor);
        registry.register_struct_dynamic(descriptor.clone());
        DynamicStruct::decode(descriptor, payload).unwrap()
    }

    #[test]
    fn displays_instance_refs_without_target_descriptor() {
        // Wire fixtures: {1: ["Target", ["eth0", 42]]} for a generic
        // reference, and {1: ["eth0", 42]} for a typed reference.
        for (kind, payload) in [
            (
                "instance_ref",
                &b"\xa1\x01\x82\x66Target\x82\x64eth0\x18\x2a"[..],
            ),
            ("instance_ref<Target>", &b"\xa1\x01\x82\x64eth0\x18\x2a"[..]),
        ] {
            let registry = Registry::new();
            let value = decode(&registry, "Notice", kind, payload);
            assert_eq!(
                format_payload(&registry, &value, None),
                r#"Notice{ subject: Target["eth0",42] }"#,
            );
            let colored = format_payload(&registry, &value, Some(&ColorSchema::TRACE));
            assert!(colored.contains("\x1b["));
            // Strip SGR codes to verify that terminal output retains
            // the same target name and decoded key as plain output.
            let plain: String = colored
                .split('\x1b')
                .enumerate()
                .map(|(i, part)| {
                    if i == 0 {
                        part
                    } else {
                        part.split_once('m').unwrap().1
                    }
                })
                .collect();
            assert_eq!(plain, format_payload(&registry, &value, None));
        }
    }

    #[test]
    fn displays_reference_vectors_inside_any() {
        for (kind, payload) in [
            (
                "vector<instance_ref>",
                &b"\xa1\x01\x82\x82\x66Target\x81\x64eth0\x82\x66Target\x80"[..],
            ),
            (
                "vector<instance_ref<Target>>",
                &b"\xa1\x01\x82\x81\x64eth0\x80"[..],
            ),
        ] {
            let registry = Registry::new();
            decode(&registry, "Notice", kind, payload);
            // {1: ["Notice", bytes(payload)]}: an `any` envelope.
            let mut encoder = dots_rs::minicbor::Encoder::new(Vec::new());
            encoder
                .map(1)
                .unwrap()
                .u8(1)
                .unwrap()
                .array(2)
                .unwrap()
                .str("Notice")
                .unwrap()
                .bytes(payload)
                .unwrap();
            let value = decode(&registry, "Envelope", "any", &encoder.into_writer());
            assert_eq!(
                format_payload(&registry, &value, None),
                r#"Envelope{ subject: Notice{ subject: [Target["eth0"], Target[]] } }"#,
            );
        }
    }
}
