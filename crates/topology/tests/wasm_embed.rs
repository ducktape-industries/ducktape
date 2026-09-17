//! A ducktape binary embeds no wasm (`AGENTS.md`, "No Embedded Wasm").
//!
//! The node is one artifact and the module set is another. Bytes compiled into
//! a binary are a second copy of a module that only a rebuild can change, and a
//! rebuild changing what a node founds or joins with is a silent network
//! change. So an `include_bytes!`/`include_str!` of a `.wasm` is allowed only in
//! a test — a file under a `tests/` directory or named `tests.rs`, or an item a
//! `#[cfg(test)]` governs. A test pins bytes on purpose; nothing else may.
//!
//! This PARSES the source rather than matching patterns in it. The rule needs
//! two judgements a regex cannot make: which items a `#[cfg(test)]` governs,
//! and whether a `.wasm` is a macro argument or just text inside a literal.
//! The shell scanner this replaces hand-rolled both — brace depth, then string
//! stripping when a brace inside a literal turned out to hold a test region
//! open over production code and green a real embed. Raw strings, braces and
//! semicolons in literals, and invocations split across lines are all free
//! here, because `syn` has already done the tokenizing.
//!
//! The first judgement is [`source_lint`]'s, shared with node-bin's
//! `lane_key_lint.rs`: this walks the file with every `#[cfg(test)]` item
//! already removed, so there is no test-vs-shipped bookkeeping left here to get
//! wrong. Only the `.wasm`-literal half is this file's own.
//!
//! Fixtures below are the cases that shell got wrong, kept as cases.

use std::path::{Path, PathBuf};
use syn::visit::Visit;

#[test]
fn no_production_source_embeds_a_wasm() {
    let root = repo_root();
    let mut embeds = Vec::new();
    let mut parsed = 0usize;
    for file in source_lint::rust_sources(&root) {
        let ast = source_lint::parse_source(&file);
        parsed += 1;
        let relative = file.strip_prefix(&root).unwrap_or(&file).display();
        embeds.extend(
            embedded_wasm(&ast)
                .into_iter()
                .map(|literal| format!("{relative}: {literal}")),
        );
    }
    assert!(
        parsed > 100,
        "only {parsed} sources scanned — the walk found nothing, which passes for the wrong reason"
    );
    assert!(
        embeds.is_empty(),
        "a non-test source embeds a .wasm — the binary is not the module set (AGENTS.md):\n  {}",
        embeds.join("\n  "),
    );
}

/// The gate judges these correctly, or it is not judging anything.
///
/// The first three are what the shell scanner shipped green: a production
/// include after a test-only constant (it stopped reading the file at the first
/// `#[cfg(test)]`), one split across lines (it matched same-line invocations
/// only), and one under a test module whose string literal holds an unbalanced
/// brace (it counted that brace as structure, so the region never closed).
#[test]
fn the_scan_tells_test_bytes_from_shipped_ones() {
    let refused = [
        (
            "a test-only constant does not make the rest of the file a test",
            r#"
            #[cfg(not(test))]
            const IDLE: u64 = 120;
            #[cfg(test)]
            const IDLE: u64 = 200;
            pub static GUEST: &[u8] = include_bytes!("../modules/chat/component.wasm");
            "#,
        ),
        (
            "an invocation split across lines",
            r#"
            pub static GUEST: &[u8] = include_bytes!(
                "../modules/chat/component.wasm"
            );
            "#,
        ),
        (
            "a brace inside a literal is not a brace",
            r#"
            #[cfg(test)]
            mod tests {
                const OPEN: &str = "{";
                #[test]
                fn it_reads() { assert_eq!(OPEN, "{"); }
            }
            pub static GUEST: &[u8] = include_bytes!("../modules/chat/component.wasm");
            "#,
        ),
        (
            "a raw string is still a path",
            r##"
            pub static GUEST: &[u8] = include_bytes!(r#"../modules/chat/component.wasm"#);
            "##,
        ),
        (
            "inside a function body is inside the binary",
            r#"
            pub fn guest() -> &'static [u8] { include_bytes!("component.wasm") }
            "#,
        ),
        (
            "a plain production include",
            r#"pub static GUEST: &[u8] = include_bytes!("component.wasm");"#,
        ),
    ];
    for (why, source) in refused {
        let ast = syn::parse_file(source).unwrap_or_else(|error| panic!("{why}: {error}"));
        assert!(
            !embedded_wasm(&ast).is_empty(),
            "should have been refused — {why}"
        );
    }

    let accepted = [
        (
            "a real test module, with production code after it",
            r#"
            pub fn run() {}
            #[cfg(test)]
            mod tests {
                const FIXTURE: &[u8] = include_bytes!("fixtures/hello.component.wasm");
                #[test]
                fn it_loads() { assert!(!FIXTURE.is_empty()); }
            }
            pub fn also_production() {}
            "#,
        ),
        (
            "a test-only constant whose include spans lines",
            r#"
            #[cfg(test)]
            const FIXTURE: &[u8] = include_bytes!(
                "fixtures/hello.component.wasm"
            );
            pub fn run() {}
            "#,
        ),
        (
            "a nested module under a test module is still a test",
            r#"
            #[cfg(test)]
            mod tests {
                mod fixtures {
                    pub const GUEST: &[u8] = include_bytes!("hello.component.wasm");
                }
            }
            "#,
        ),
        (
            "`all(test, ...)` is a test",
            r#"
            #[cfg(all(test, feature = "slow"))]
            const FIXTURE: &[u8] = include_bytes!("hello.component.wasm");
            "#,
        ),
        (
            "including something that is not a guest",
            r#"pub const HELP: &str = include_str!("help.txt");"#,
        ),
    ];
    for (why, source) in accepted {
        let ast = syn::parse_file(source).unwrap_or_else(|error| panic!("{why}: {error}"));
        assert_eq!(
            embedded_wasm(&ast),
            Vec::<String>::new(),
            "should have been accepted — {why}"
        );
    }
}

// ---- the scan ---------------------------------------------------------------

/// Every `.wasm` an `include_bytes!`/`include_str!` pulls in outside a
/// `#[cfg(test)]` item, in source order.
fn embedded_wasm(file: &syn::File) -> Vec<String> {
    let mut scan = Scan { found: Vec::new() };
    scan.visit_file(&source_lint::production_only(file));
    scan.found
}

struct Scan {
    found: Vec<String>,
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(path) = wasm_include(mac) {
            self.found.push(path);
        }
        syn::visit::visit_macro(self, mac);
    }
}

/// The literal an `include_bytes!`/`include_str!` names, when it ends in
/// `.wasm`. `LitStr::value` has already resolved escapes and raw strings, so
/// the spelling in the source does not matter.
fn wasm_include(mac: &syn::Macro) -> Option<String> {
    let name = mac.path.segments.last()?.ident.to_string();
    if name != "include_bytes" && name != "include_str" {
        return None;
    }
    let literal: syn::LitStr = mac.parse_body().ok()?;
    let path = literal.value();
    path.ends_with(".wasm").then_some(path)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root sits two levels above crates/topology")
}
