//! Every lane a plane binds by name is a lane some module declares.
//!
//! A `LaneSource::Declared(LaneKey { module_id, name })` is a WAIT, never a
//! default: `lane_table` holds the plane until the committed table names that
//! key, and reports `lane_absent` on a forever-retry cadence while it does not.
//! So a key that no `lanes.json` declares does not fail a build, fail a test, or
//! fail a boot — the plane simply never comes up, on every node, and the only
//! evidence is a warning in a ring that scrolls.
//!
//! That is not hypothetical. The node bound `chat/voice` for Pages presence for
//! as long as the lane existed; when presence moved to its own `chat/presence`
//! lane, the binder and the declaration had to move together, and nothing but
//! this test would have said so if they had not.
//!
//! It PARSES the source. The scanner this replaces read text: it cut each file
//! at its first `#[cfg(test)]` and called the rest test code, so a key bound by
//! a production `const` below any test-only item was never checked at all — a
//! planted `chat/nope` under `presence_plane.rs`'s own test constant passed it
//! green. The `#[cfg(test)]` judgement is [`source_lint`]'s now, shared with
//! `crates/topology/tests/wasm_embed.rs`, which had the identical bug; this
//! file walks what the binary actually compiles and matches `LaneKey` struct
//! expressions by AST, so a key split across lines, nested in a call, or
//! sitting beside a brace in a string literal reads the same as any other.
//!
//! It lives in the node binary's tests for the same reason
//! `tracing_plane_lint.rs` does: the two halves it compares are in different
//! trees — planes in `bin/node/src`, declarations in `crates/modules` — and
//! node-bin is where they meet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use syn::visit::Visit;

/// A lane key, as source spells it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    module_id: String,
    name: String,
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.module_id, self.name)
    }
}

/// What one file's shipped code binds: the keys it names, and how many `LaneKey`
/// sites named something this cannot read.
#[derive(Default)]
struct Bound {
    keys: Vec<Key>,
    /// A key assembled from anything but two string literals. Reported, never
    /// skipped: a plane knows what it IS, so a key it cannot state literally is
    /// either not a bind or not checkable, and both deserve a look. Silently
    /// passing what it cannot read is the failure this lint was rewritten to
    /// stop making.
    unreadable: usize,
}

impl<'ast> Visit<'ast> for Bound {
    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        let names_a_lane_key = expr
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "LaneKey");
        if names_a_lane_key {
            match (
                literal_field(expr, "module_id"),
                literal_field(expr, "name"),
            ) {
                (Some(module_id), Some(name)) => self.keys.push(Key { module_id, name }),
                _ => self.unreadable += 1,
            }
        }
        syn::visit::visit_expr_struct(self, expr);
    }
}

/// The string a named field is initialized with, when it is a plain literal.
/// `LitStr::value` has already resolved escapes and raw strings, so the
/// spelling in the source does not matter.
fn literal_field(expr: &syn::ExprStruct, field: &str) -> Option<String> {
    let initialized = expr.fields.iter().find(|value| match &value.member {
        syn::Member::Named(ident) => ident == field,
        syn::Member::Unnamed(_) => false,
    })?;
    match &initialized.expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(literal),
            ..
        }) => Some(literal.value()),
        _ => None,
    }
}

/// Every lane key the SHIPPED half of one source binds.
fn bound_keys(file: &syn::File) -> Bound {
    let mut bound = Bound::default();
    bound.visit_file(&source_lint::production_only(file));
    bound
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve the repo root")
}

/// Every lane any module in the tree declares, read from the FILES that are
/// the declarations — the same files `crates/noded/build.rs` stages into a
/// founding set, so this reads what a network would actually commit.
fn declared() -> BTreeSet<Key> {
    let modules = repo_root().join("crates/modules");
    let mut lanes = BTreeSet::new();
    for area in std::fs::read_dir(&modules).expect("read crates/modules") {
        let area = area.expect("dir entry").path();
        if !area.is_dir() {
            continue;
        }
        for module in std::fs::read_dir(&area).expect("read a module area") {
            let module = module.expect("dir entry").path();
            let declaration = module.join("lanes.json");
            if !declaration.is_file() {
                continue;
            }
            let module_id = module
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a module directory name")
                .to_owned();
            let decls: Vec<modules::LaneDecl> = serde_json::from_str(
                &std::fs::read_to_string(&declaration).expect("read a lane declaration"),
            )
            .expect("a lane declaration parses");
            for decl in decls {
                lanes.insert(Key {
                    module_id: module_id.clone(),
                    name: decl.name,
                });
            }
        }
    }
    lanes
}

#[test]
fn every_lane_a_plane_binds_is_a_lane_a_module_declares() {
    let root = repo_root();
    let planes = root.join("bin/node/src");
    let declared = declared();

    let mut scanned = 0usize;
    let mut sites = 0usize;
    let mut undeclared = Vec::new();
    let mut unreadable = Vec::new();
    for file in source_lint::rust_sources(&planes) {
        let bound = bound_keys(&source_lint::parse_source(&file));
        scanned += 1;
        sites += bound.keys.len();
        let relative = file.strip_prefix(&root).unwrap_or(&file).display();
        if bound.unreadable > 0 {
            unreadable.push(format!("{relative}: {} site(s)", bound.unreadable));
        }
        undeclared.extend(
            bound
                .keys
                .into_iter()
                .filter(|key| !declared.contains(key))
                .map(|key| format!("{relative} -> {key}")),
        );
    }

    assert!(
        scanned > 50 && sites > 0,
        "{scanned} sources and {sites} lane keys — the walk found nothing, which passes for the \
         wrong reason"
    );
    assert!(
        unreadable.is_empty(),
        "these `LaneKey` sites name something other than two string literals, so this lint cannot \
         check them — state the key literally, or the lane it waits on is unguarded:\n{}",
        unreadable.join("\n"),
    );
    assert!(
        undeclared.is_empty(),
        "these planes wait on a lane no module declares. The wait is silent and \
         forever — the plane never binds and the node says so only in a warning: \
         add the lane to that module's lanes.json, or bind the key it really \
         declares:\n{}",
        undeclared.join("\n"),
    );
}

/// The gate judges these correctly, or it is not judging anything.
///
/// The first is what the text scanner shipped green — a production key below
/// the file's first `#[cfg(test)]` item, which is exactly how the real drift
/// would have re-entered. The rest are the shapes a line-window parser cannot
/// read: a literal holding a brace, an initializer wrapped by rustfmt, and a key
/// built inside a call rather than assigned to a `const`.
#[test]
fn the_scan_tells_a_bound_key_from_a_test_one() {
    let bound = [
        (
            "a key below an early test-only item still binds",
            r#"
            #[cfg(test)]
            const IDLE: u64 = 200;
            const LANE: LaneSource =
                LaneSource::Declared(LaneKey { module_id: "chat", name: "planted" });
            "#,
        ),
        (
            "a key whose fields wrapped across lines",
            r#"
            const LANE: LaneSource = LaneSource::Declared(LaneKey {
                module_id: "chat",
                name: "planted",
            });
            "#,
        ),
        (
            "a brace inside a literal is not a brace",
            r#"
            #[cfg(test)]
            mod tests {
                const OPEN: &str = "{";
            }
            const LANE: LaneSource =
                LaneSource::Declared(LaneKey { module_id: "chat", name: "planted" });
            "#,
        ),
        (
            "a key built inside a call is still bound",
            r#"
            pub fn bind(book: &OverlayBook) {
                book.resolve(LaneKey { module_id: "chat", name: "planted" });
            }
            "#,
        ),
        (
            "a raw string is still a name",
            r##"
            const LANE: LaneSource =
                LaneSource::Declared(LaneKey { module_id: r#"chat"#, name: "planted" });
            "##,
        ),
    ];
    for (why, source) in bound {
        let found = bound_keys(&syn::parse_file(source).unwrap_or_else(|e| panic!("{why}: {e}")));
        assert_eq!(
            found.keys,
            vec![Key {
                module_id: "chat".into(),
                name: "planted".into()
            }],
            "should have been checked — {why}"
        );
        assert_eq!(found.unreadable, 0, "{why}");
    }

    let test_only = [
        (
            "a key under a test module, with production code after it",
            r#"
            pub fn run() {}
            #[cfg(test)]
            mod tests {
                const ABSENT: LaneKey = LaneKey { module_id: "gateway", name: "voice" };
            }
            pub fn also_production() {}
            "#,
        ),
        (
            "a nested module under a test module is still a test",
            r#"
            #[cfg(test)]
            mod tests {
                mod fixtures {
                    pub const ABSENT: LaneKey = LaneKey { module_id: "gateway", name: "voice" };
                }
            }
            "#,
        ),
        (
            "`all(test, …)` is a test",
            r#"
            #[cfg(all(test, feature = "slow"))]
            const ABSENT: LaneKey = LaneKey { module_id: "gateway", name: "voice" };
            "#,
        ),
        (
            "a test-only item inside a function body",
            r#"
            pub fn run() {
                #[cfg(test)]
                const ABSENT: LaneKey = LaneKey { module_id: "gateway", name: "voice" };
            }
            "#,
        ),
    ];
    for (why, source) in test_only {
        let found = bound_keys(&syn::parse_file(source).unwrap_or_else(|e| panic!("{why}: {e}")));
        assert_eq!(found.keys, Vec::new(), "should have been skipped — {why}");
    }

    // a key this cannot read is reported, not waved through: `cfg(not(test))`
    // is production, so the constant below ships and its lane is unguarded.
    let opaque = r#"
        #[cfg(not(test))]
        const LANE: LaneKey = LaneKey { module_id: CHAT, name: "presence" };
    "#;
    let found = bound_keys(&syn::parse_file(opaque).expect("parses"));
    assert_eq!(found.unreadable, 1);
    assert_eq!(found.keys, Vec::new());
}
