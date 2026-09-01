//! CLI front-end: `dots-inspect <binary>...` prints the DOTS types each
//! binary publishes and subscribes to, straight from its ELF sections.
//! With `--detail` the full type definitions are printed in `.dots` IDL
//! syntax.

use std::process::ExitCode;

use dots_inspect::{Report, inspect};

fn main() -> ExitCode {
    let mut detail = false;
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-d" | "--detail" => detail = true,
            "-h" | "--help" => {
                usage();
                return ExitCode::from(2);
            }
            _ => paths.push(arg),
        }
    }
    if paths.is_empty() {
        usage();
        return ExitCode::from(2);
    }

    let show_path = paths.len() > 1;
    let mut failed = false;
    for path in &paths {
        if show_path {
            println!("{path}:");
        }
        match std::fs::read(path).map_err(|e| e.to_string()) {
            Ok(data) => match inspect(&data) {
                Ok(report) => print_report(&report, detail),
                Err(e) => {
                    eprintln!("dots-inspect: {path}: {e}");
                    failed = true;
                }
            },
            Err(e) => {
                eprintln!("dots-inspect: {path}: {e}");
                failed = true;
            }
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn usage() {
    eprintln!("usage: dots-inspect [-d|--detail] <binary>...");
    eprintln!();
    eprintln!("Prints the DOTS types a dots-rs binary publishes and subscribes");
    eprintln!("to, read statically from its linkme ELF sections.");
    eprintln!();
    eprintln!("  -d, --detail   also print each type definition in .dots IDL");
    eprintln!("                 syntax, plus referenced substructs and enums");
}

fn print_report(report: &Report, detail: bool) {
    if !report.has_published_section
        && !report.has_subscribed_section
        && !report.has_subscribed_filtered_section
    {
        println!("no dots-rs registration sections — not a dots-rs binary?");
        return;
    }
    if detail {
        print_detail(report);
    } else {
        print_names("published", &report.published);
        print_names("subscribed", &report.subscribed);
        print_names("subscribed (filtered)", &report.subscribed_filtered);
    }
}

fn print_names(label: &str, types: &[dots_inspect::TypeInfo]) {
    if types.is_empty() {
        println!("{label}: none");
        return;
    }
    println!("{label} ({}):", types.len());
    for t in types {
        println!("  {}", t.name);
    }
}

fn print_detail(report: &Report) {
    let groups = [
        ("published", &report.published),
        ("subscribed", &report.subscribed),
        ("subscribed (filtered)", &report.subscribed_filtered),
    ];
    for (label, types) in groups {
        if types.is_empty() {
            println!("{label}: none");
            println!();
            continue;
        }
        println!("{label} ({}):", types.len());
        println!();
        for t in types {
            print!("{}", t.to_idl());
            println!();
        }
    }

    // Types pulled in as property types of the registered ones — part
    // of the schema, but not registered themselves.
    let mut referenced: Vec<(&str, String)> = report
        .referenced_structs
        .iter()
        .map(|t| (t.name.as_str(), t.to_idl()))
        .chain(
            report
                .referenced_enums
                .iter()
                .map(|e| (e.name.as_str(), e.to_idl())),
        )
        .collect();
    referenced.sort_by_key(|(name, _)| *name);
    if !referenced.is_empty() {
        println!("referenced types ({}):", referenced.len());
        println!();
        for (_, idl) in referenced {
            print!("{idl}");
            println!();
        }
    }
}
