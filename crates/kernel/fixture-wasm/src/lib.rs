//! The kernel fixture programs as wasm32 bytes, compiled from crates/kernel/fixtures by this
//! crate's build script whenever a test suite that loads them builds.
macro_rules! fixtures {
    ($($name:ident => $file:literal),* $(,)?) => {
        $(pub const $name: &[u8] = include_bytes!(concat!(env!("FIXTURE_WASM"), "/", $file, ".wasm"));)*
    };
}

fixtures! {
    ACCESS => "fixture_access",
    AWAITER => "fixture_awaiter",
    AWAITS => "fixture_awaits",
    BLOBS => "fixture_blobs",
    EGRESS => "fixture_egress",
    HALO => "fixture_halo",
    IDENTITY => "fixture_identity",
    ORACLE => "fixture_oracle",
    PROBE => "fixture_probe",
    PROGRAMS => "fixture_programs",
    RELAY => "fixture_relay",
    SCHEDULER => "fixture_scheduler",
    VALSET => "fixture_valset",
}
