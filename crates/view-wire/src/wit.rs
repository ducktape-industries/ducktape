//! The one literal shared by guest and host binding generators.
/// Passes the canonical Ice WIT literal to a local callback macro.
///
/// ```
/// macro_rules! inspect { ($wit:literal) => { const TEXT: &str = $wit; }; }
/// view_wire::with_view_wit!(inspect);
/// assert_eq!(TEXT, view_wire::WIT);
/// ```
#[macro_export]
macro_rules! with_view_wit {
    ($callback:ident) => {
        $callback!(
            r#"package ducktape:view@0.1.0;

world view {
    // The guest's panic hook hands the host its message before the abort
    // that follows: a trapped instance can never be entered again, so the
    // message has to leave first.
    import panicked: func(message: string);

    export init: func(macos: bool);
    export tick: func(events: list<u8>) -> list<u8>;
    export snapshot: func() -> result<list<u8>, string>;
    export restore: func(state: list<u8>, macos: bool) -> result<_, string>;
}
"#
        );
    };
}

macro_rules! declare_wit {
    ($wit:literal) => {
        pub const WIT: &str = $wit;
    };
}
crate::with_view_wit!(declare_wit);
