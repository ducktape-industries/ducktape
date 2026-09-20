//! The half the repo's source lints share: which FILES hold shipped code, and
//! which ITEMS inside one do.
//!
//! Two lints ask those questions before asking their own — `wasm_embed`
//! (topology) refuses an `include_bytes!` of a guest outside a test, and
//! `lane_key_lint` (node-bin) refuses a `LaneKey` naming a lane no module
//! declares. Each owned a copy of the shared half, and each got it wrong the
//! same way: a scanner that cut a file at the first `#[cfg(test)]` reads
//! everything below it as test code, so a `#[cfg(test)]` constant sitting above
//! four thousand lines of production source hides all of them. The answer lives
//! here once now.
//!
//! It PARSES rather than matches. Two judgements a regex cannot make: which
//! items a `#[cfg(test)]` governs, and whether a literal is code or text inside
//! a string. Raw strings, braces and semicolons in literals, and invocations
//! split across lines are all free here, because `syn` has already tokenized.
//!
//! A lint using this walks [`production_only`]'s output and matches whatever it
//! cares about, with no test-vs-shipped bookkeeping of its own to forget.

use std::fs;
use std::path::{Path, PathBuf};
use syn::visit_mut::VisitMut;

/// Every `.rs` under `root` that holds shipped code.
///
/// Build outputs and the gitignored scratch directories are skipped by name
/// rather than by asking git, so this needs no repository. A file under a
/// `tests/` directory or named `tests.rs` is skipped too: a test pins bytes and
/// names absent lanes on purpose, and `#[cfg(test)] #[path = "tests.rs"] mod
/// tests;` puts a whole test module in a file whose own text carries no
/// attribute to notice.
pub fn rust_sources(root: &Path) -> Vec<PathBuf> {
    // any `target*`: a worktree builds into its own `target-<unit>`.
    const SKIP: &[&str] = &[".git", ".claude", ".codex", ".worktree", "node_modules"];
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
                let build_output = name.starts_with("target");
                let skipped = build_output || SKIP.contains(&name.as_ref()) || name == "tests";
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

/// Parse one source file, naming it on failure.
///
/// A file this cannot read or parse is not silently skipped: a lint that skips
/// what it cannot understand passes by failing, which is the failure mode every
/// lint here exists to prevent.
pub fn parse_source(path: &Path) -> syn::File {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    syn::parse_file(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// `file` with every `#[cfg(test)]` item removed, at every depth — what the
/// binary actually compiles.
///
/// Removing them, rather than counting how deep a walk is inside one, is what
/// makes a lint on top of this hard to get wrong: there is no depth to
/// maintain, so there is no arm that forgets to.
pub fn production_only(file: &syn::File) -> syn::File {
    let mut shipped = file.clone();
    Prune.visit_file_mut(&mut shipped);
    shipped
}

/// `#[cfg(test)]`, and `#[cfg(all(test, …))]` with it.
///
/// `not(test)` is production, which is the case that matters: a constant shrunk
/// under `cfg(test)` sits directly beneath its `cfg(not(test))` twin, and
/// reading the pair as "this file is a test" is how 4,500 lines of
/// `crates/services/broker/src/lib.rs` went unscanned. `any(test, …)` is NOT
/// treated as a test: it compiles in production too, so what sits under one is
/// reported rather than excused.
fn is_cfg_test(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg") && attr.parse_args().is_ok_and(|meta| names_test(&meta))
}

/// Whether this item compiles only under `cargo test`.
fn is_test_only(item: &syn::Item) -> bool {
    item_attrs(item).is_some_and(|attrs| attrs.iter().any(is_cfg_test))
}

struct Prune;

impl VisitMut for Prune {
    fn visit_file_mut(&mut self, file: &mut syn::File) {
        file.items.retain(|item| !is_test_only(item));
        syn::visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut module.content {
            items.retain(|item| !is_test_only(item));
        }
        syn::visit_mut::visit_item_mod_mut(self, module);
    }

    /// an item declared inside a function body is an item too, and
    /// `#[cfg(test)]` on one means the same thing there.
    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        block.stmts.retain(|stmt| match stmt {
            syn::Stmt::Item(item) => !is_test_only(item),
            _ => true,
        });
        syn::visit_mut::visit_block_mut(self, block);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `production_only` is the whole judgement two lints now delegate, so its
    /// own cases live here rather than only in theirs: the three places a
    /// `#[cfg(test)]` item can sit, and the two spellings that are NOT a test.
    #[test]
    fn a_cfg_test_item_is_pruned_at_every_depth_and_nothing_else_is() {
        let source = r#"
            #[cfg(test)]
            const TOP: u8 = 1;
            #[cfg(all(test, feature = "slow"))]
            const ALL: u8 = 2;
            #[cfg(not(test))]
            const SHIPPED: u8 = 3;
            #[cfg(any(test, feature = "probe"))]
            const ALSO_SHIPPED: u8 = 4;
            mod inner {
                #[cfg(test)]
                const NESTED: u8 = 5;
                pub const KEPT: u8 = 6;
            }
            pub fn run() {
                #[cfg(test)]
                const IN_BODY: u8 = 7;
                let _ = 8;
            }
            #[cfg(test)]
            mod tests {
                mod deeper {
                    pub const BURIED: u8 = 9;
                }
            }
        "#;
        let shipped = production_only(&syn::parse_file(source).expect("parses"));
        let mut names = Names(Vec::new());
        syn::visit::Visit::visit_file(&mut names, &shipped);
        // `not(test)` is production, and `any(test, …)` compiles in production
        // too — reading either as "this is a test" is the bug this replaces.
        assert_eq!(names.0, ["SHIPPED", "ALSO_SHIPPED", "KEPT"]);
    }

    /// every constant still standing, in source order.
    struct Names(Vec<String>);

    impl<'ast> syn::visit::Visit<'ast> for Names {
        fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
            self.0.push(item.ident.to_string());
            syn::visit::visit_item_const(self, item);
        }
    }

    /// A test may name an absent lane and pin guest bytes on purpose, and
    /// `#[cfg(test)] #[path = "tests.rs"] mod tests;` puts a whole test module
    /// in a file whose own text carries no attribute to notice — so the walk
    /// has to skip those files, not the parser.
    #[test]
    fn the_walk_skips_what_a_test_may_live_in() {
        let root = std::env::temp_dir().join("source-lint-walk");
        let _ = fs::remove_dir_all(&root);
        for directory in ["src", "src/tests", "target"] {
            fs::create_dir_all(root.join(directory)).expect("scratch tree");
        }
        for file in [
            "src/lib.rs",
            "src/tests.rs",
            "src/tests/helper.rs",
            "target/generated.rs",
            "src/notes.txt",
        ] {
            fs::write(root.join(file), "").expect("scratch file");
        }

        let found: Vec<String> = rust_sources(&root)
            .iter()
            .map(|path| {
                path.strip_prefix(&root)
                    .expect("under the scratch root")
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(found, ["src/lib.rs"]);
        fs::remove_dir_all(&root).expect("scratch tree removed");
    }
}
