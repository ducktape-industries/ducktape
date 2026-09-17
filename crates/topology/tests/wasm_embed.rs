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
//! Fixtures below are the cases that shell got wrong, kept as cases.

use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;

#[test]
fn no_production_source_embeds_a_wasm() {
    let root = repo_root();
    let mut embeds = Vec::new();
    let mut parsed = 0usize;
    for file in rust_sources(&root) {
        let text = fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        // A file this cannot parse is not silently skipped: the gate would then
        // pass by failing, which is the failure mode it exists to prevent.
        let ast = syn::parse_file(&text)
            .unwrap_or_else(|error| panic!("parse {}: {error}", file.display()));
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
    let mut scan = Scan {
        test_depth: 0,
        found: Vec::new(),
    };
    scan.visit_file(file);
    scan.found
}

struct Scan {
    /// how many enclosing items are governed by a `#[cfg(test)]`. Nonzero means
    /// everything here compiles only under `cargo test`.
    test_depth: usize,
    found: Vec<String>,
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let test = item_attrs(item).is_some_and(|attrs| attrs.iter().any(is_cfg_test));
        self.test_depth += usize::from(test);
        syn::visit::visit_item(self, item);
        self.test_depth -= usize::from(test);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.test_depth == 0
            && let Some(path) = wasm_include(mac)
        {
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

/// `#[cfg(test)]`, and `#[cfg(all(test, …))]` with it.
///
/// `not(test)` is production, which is the case that matters: a constant shrunk
/// under `cfg(test)` sits directly beneath its `cfg(not(test))` twin, and
/// reading the pair as "this file is a test" is how 4,500 lines of
/// `crates/services/broker/src/lib.rs` went unscanned. `any(test, …)` is NOT
/// treated as a test: it compiles in production too, so an embed under one is
/// reported rather than excused.
fn is_cfg_test(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg") && attr.parse_args().is_ok_and(|meta| names_test(&meta))
}

fn names_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
            .is_ok_and(|inner| inner.iter().any(names_test)),
        _ => false,
    }
}

fn item_attrs(item: &syn::Item) -> Option<&Vec<syn::Attribute>> {
    Some(match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => return None,
    })
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root sits two levels above crates/topology")
}

/// Every tracked-looking `.rs` in the tree except the ones a test may embed in:
/// a file under a `tests/` directory or named `tests.rs`. Build outputs and the
/// gitignored scratch directories are skipped by name rather than by asking git,
/// so this needs no repository.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "target",
        "target-shared",
        ".git",
        ".claude",
        ".codex",
        ".worktree",
        "node_modules",
    ];
    let mut sources = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                let skipped = SKIP.contains(&name.as_ref()) || name == "tests";
                if !skipped {
                    stack.push(path);
                }
                continue;
            }
            if name.ends_with(".rs") && name != "tests.rs" {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}
