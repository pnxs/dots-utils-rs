//! Self-inspection: this test binary is itself an ELF executable built
//! against dots-rs, so registering types and then running the inspector
//! on `current_exe()` exercises the whole chain — derive emission,
//! linkme section layout, PIE relocations, descriptor/kind parsing —
//! without building a fixture binary.
//!
//! Test builds run without LTO, so culling is coarse: types *other*
//! than the ones registered here may legitimately appear in the slices.
//! The assertions therefore only check presence, never absence.

#![cfg(target_os = "linux")]

use dots_rs::GlobalRegistration;

// In its own module because the derive's `dots!` ctor plumbing
// (`__dots_ctor_*` macro + re-export) collides with itself at a test
// binary's crate root.
mod model {
    use dots_rs::{DotsEnum, DotsStruct};

    #[derive(DotsEnum, Default, Debug, Clone, Copy, PartialEq, Eq)]
    #[dots(name = "InspectColor")]
    pub enum InspectColor {
        #[default]
        #[dots(tag = 1)]
        Red,
        #[dots(tag = 2, value = 7)]
        Green,
    }

    #[derive(DotsStruct, Default, Debug, Clone)]
    #[dots(name = "InspectSub", substruct_only)]
    pub struct InspectSub {
        #[dots(tag = 1)]
        pub level: Option<i32>,
    }

    #[derive(DotsStruct, Default, Debug, Clone)]
    #[dots(name = "InspectPing", cached)]
    pub struct InspectPing {
        #[dots(tag = 1, key)]
        pub id: u32,
        #[dots(tag = 2)]
        pub note: Option<String>,
        #[dots(tag = 3)]
        pub color: Option<InspectColor>,
        #[dots(tag = 4)]
        pub labels: Option<Vec<String>>,
        #[dots(tag = 5)]
        pub sub: Option<InspectSub>,
    }

    #[derive(DotsStruct, Default, Debug, Clone)]
    #[dots(name = "InspectPong")]
    pub struct InspectPong {
        #[dots(tag = 1)]
        pub value: Option<u64>,
    }
}
use model::{InspectPing, InspectPong};

fn inspect_self() -> dots_inspect::Report {
    let exe = std::env::current_exe().expect("own path");
    let data = std::fs::read(exe).expect("read own binary");
    dots_inspect::inspect(&data).expect("inspect own binary")
}

#[test]
fn finds_registered_types_in_own_binary() {
    InspectPing::register_as_subscribed();
    InspectPong::register_as_published();

    let report = inspect_self();
    assert!(report.has_subscribed_section);
    assert!(report.has_published_section);

    let ping = report
        .subscribed
        .iter()
        .find(|t| t.name == "InspectPing")
        .expect("InspectPing should be listed as subscribed");
    assert!(ping.flags.is_cached());
    assert_eq!(ping.size, size_of::<InspectPing>() as u64);
    assert_eq!(ping.align, align_of::<InspectPing>() as u64);

    let pong = report
        .published
        .iter()
        .find(|t| t.name == "InspectPong")
        .expect("InspectPong should be listed as published");
    assert_eq!(pong.properties.len(), 1);
    assert_eq!(pong.flags.bits(), 0);
}

#[test]
fn renders_types_as_dots_idl() {
    InspectPing::register_as_subscribed();

    let report = inspect_self();
    let ping = report
        .subscribed
        .iter()
        .find(|t| t.name == "InspectPing")
        .expect("InspectPing should be listed as subscribed");
    assert_eq!(
        ping.to_idl(),
        "struct InspectPing [cached] {\n\
         \x20   1: [key] uint32 id;\n\
         \x20   2: string note;\n\
         \x20   3: InspectColor color;\n\
         \x20   4: vector<string> labels;\n\
         \x20   5: InspectSub sub;\n\
         }\n"
    );

    // Property types pull in the substruct and the enum with full
    // definitions. In this uncalled build InspectSub's own (never
    // invoked) registration statics survive the lack of culling, so it
    // may show up as registered rather than referenced — accept both.
    let sub = report
        .referenced_structs
        .iter()
        .chain(&report.subscribed)
        .chain(&report.published)
        .find(|t| t.name == "InspectSub")
        .expect("InspectSub should be found");
    assert_eq!(
        sub.to_idl(),
        "struct InspectSub [substruct_only] {\n\
         \x20   1: int32 level;\n\
         }\n"
    );

    let color = report
        .referenced_enums
        .iter()
        .find(|e| e.name == "InspectColor")
        .expect("InspectColor should be referenced");
    // Red's value is the `tag - 1` default and stays implicit; Green's
    // explicit `value = 7` must be spelled out.
    assert_eq!(
        color.to_idl(),
        "enum InspectColor {\n\
         \x20   1: Red,\n\
         \x20   2: Green = 7\n\
         }\n"
    );
}

#[test]
fn rejects_non_object_input() {
    assert!(dots_inspect::inspect(b"definitely not an ELF").is_err());
}
