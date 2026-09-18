//! The refusal frame names no refuser, so a caller attributes a module-state
//! class to the module it addressed (see `refusal_class`'s "Who may mint
//! what"). A core hop on the submit lane therefore never mints one: a view
//! branches recovery on the token (`stale` = the other person's words,
//! `not_found` = leave the board), and a core sentence under one of those
//! tokens would be shown as a user's words. A hop uses the host-reserved
//! tokens, its own host-specific ones, or the request/machinery classes.
//!
//! This is the core half of wasm-host's `module_state_classes.rs` in
//! ducktape-sdk. It PARSES rather than matches: `source_lint` drops the
//! `#[cfg(test)]` items and syn drops the comments, and the walk below reads
//! the token stream, so a class named inside `format!` or `json!` (which syn
//! leaves as opaque tokens) is read like any other.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MODULE_ONLY: [(&str, &str); 6] = [
    ("NOT_FOUND", sdk::refusal::NOT_FOUND),
    ("ALREADY_EXISTS", sdk::refusal::ALREADY_EXISTS),
    ("STALE", sdk::refusal::STALE),
    ("WRONG_STATE", sdk::refusal::WRONG_STATE),
    ("NOT_YET", sdk::refusal::NOT_YET),
    ("UNAUTHORIZED", sdk::refusal::UNAUTHORIZED),
];

/// The hops a submit crosses before it reaches the addressed module.
const HOPS: [&str; 4] = [
    "crates/kernel/host/src",
    "crates/kernel/node/src",
    "crates/noded/src",
    "bin/node/src",
];

/// `ducktape fs` maps the node's HTTP 404 to a CLI `not_found`: a sentence to
/// a terminal, not a submit-lane refusal a view branches on.
const NOT_A_HOP: [&str; 1] = ["bin/node/src/fs_cli/args.rs"];

#[test]
fn a_core_hop_never_mints_a_class_about_module_state() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let mut read = 0;
    let mut minted = Vec::new();
    for hop in HOPS {
        let sources = source_lint::rust_sources(&root.join(hop));
        let tests = test_only_files(&sources);
        for path in sources.iter().filter(|path| !tests.contains(*path)) {
            let relative = path.strip_prefix(&root).unwrap().display().to_string();
            if NOT_A_HOP.contains(&relative.as_str()) {
                continue;
            }
            read += 1;
            let shipped = source_lint::production_only(&source_lint::parse_source(path));
            for (line, what) in mints(&shipped) {
                minted.push(format!("{relative}:{line} mints {what}"));
            }
        }
    }
    assert!(read > 0, "no hop source was read");
    assert!(
        minted.is_empty(),
        "a class only the addressed module may mint:\n{}",
        minted.join("\n")
    );
}

/// The parser itself: a request class passes, and everything around it that
/// only looks like a module-state class (a comment, a test, a status code)
/// passes with it.
#[test]
fn the_scan_passes_a_request_class() {
    let source = r#"
        //! refusal::STALE in a comment is prose.
        /// so is "stale" in a doc comment.
        pub fn refuse() -> String {
            sdk::refusal::encode(sdk::refusal::INVALID_INPUT, "the id is malformed")
        }
        pub fn status() -> StatusCode { StatusCode::NOT_FOUND }
        pub fn log() { tracing::warn!(reason = "budget_exceeded", "the index is stale"); }
        #[cfg(test)]
        mod tests {
            const SEEN: &str = sdk::refusal::STALE;
        }
    "#;
    assert_eq!(mints(&parse(source)), Vec::<(usize, String)>::new());
}

/// The parser itself: every spelling a hop could mint a module-state class in
/// is caught, on its line.
#[test]
fn the_scan_catches_a_module_state_class() {
    let source = r#"
        use sdk::refusal::{encode, NOT_FOUND};
        use refusal_class as classes;
        pub fn refuse() -> String {
            sdk::refusal::encode(sdk::refusal::STALE, "the head moved")
        }
        pub fn later() -> String { format!("{}: soon", refusal::NOT_YET) }
        pub fn owner() -> &'static str { classes::UNAUTHORIZED }
        pub fn spelled() -> (&'static str, &'static str) {
            ("wrong_state", "already_exists: that id is taken")
        }
    "#;
    assert_eq!(
        mints(&parse(source)),
        [
            (2, "refusal::NOT_FOUND".to_string()),
            (5, "refusal::STALE".to_string()),
            (7, "refusal::NOT_YET".to_string()),
            (8, "refusal::UNAUTHORIZED".to_string()),
            (10, "\"wrong_state\"".to_string()),
            (10, "\"already_exists: that id is taken\"".to_string()),
        ]
    );
}

/// The files a `#[cfg(test)] mod x;` among `sources` loads, and every file
/// those load in turn: test code whose own text carries no attribute to prune
/// (`#[cfg(test)] #[path = "forge_tests.rs"] mod tests;`).
fn test_only_files(sources: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut pending = Vec::new();
    for path in sources {
        let file = source_lint::parse_source(path);
        let shipped = loaded(path, &source_lint::production_only(&file));
        pending.extend(loaded(path, &file).difference(&shipped).cloned());
    }
    let mut test_only = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if test_only.insert(path.clone()) {
            pending.extend(loaded(&path, &source_lint::parse_source(&path)));
        }
    }
    test_only
}

/// The files `file`'s own out-of-line `mod x;` items load, by the compiler's
/// rule: a `#[path]` is relative to `path`'s directory, and a bare `mod x;` is
/// `x.rs` or `x/mod.rs` under the module's directory.
fn loaded(path: &Path, file: &syn::File) -> BTreeSet<PathBuf> {
    let directory = path.parent().unwrap();
    let owns_its_directory = ["lib.rs", "main.rs", "mod.rs"]
        .iter()
        .any(|root| path.ends_with(root));
    let module_directory = if owns_its_directory {
        directory.to_path_buf()
    } else {
        directory.join(path.file_stem().unwrap())
    };
    let mut loaded = BTreeSet::new();
    for item in &file.items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        if module.content.is_some() {
            continue;
        }
        let explicit = module.attrs.iter().find(|attr| attr.path().is_ident("path"));
        let Some(explicit) = explicit else {
            let flat = module_directory.join(format!("{}.rs", module.ident));
            let nested = module_directory.join(module.ident.to_string()).join("mod.rs");
            loaded.insert(if flat.exists() { flat } else { nested });
            continue;
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(relative),
            ..
        }) = &explicit.meta.require_name_value().unwrap().value
        else {
            panic!("{}: a #[path] that is not a string", path.display());
        };
        loaded.insert(directory.join(relative.value()));
    }
    loaded
}

fn parse(source: &str) -> syn::File {
    source_lint::production_only(&syn::parse_file(source).expect("fixture parses"))
}

/// Every module-state class `file` mints, as `(line, what)`.
fn mints(file: &syn::File) -> Vec<(usize, String)> {
    let mut walk = Walk {
        modules: vec!["refusal".to_string(), "refusal_class".to_string()],
        minted: Vec::new(),
    };
    walk.stream(file.to_token_stream(), false);
    walk.minted
}

struct Walk {
    /// the names the class module goes by in this file, `use … as` included.
    modules: Vec<String>,
    minted: Vec<(usize, String)>,
}

impl Walk {
    /// `in_class_braces`: this stream is the `{…}` of a `use …::refusal::{…}`.
    fn stream(&mut self, stream: TokenStream, in_class_braces: bool) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            let before = &tokens[..at];
            match token {
                TokenTree::Group(group) => {
                    if is_doc(group) {
                        continue;
                    }
                    let class_braces =
                        group.delimiter() == Delimiter::Brace && self.after_module(before);
                    self.stream(group.stream(), class_braces);
                }
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    let names_a_class = MODULE_ONLY.iter().any(|(class, _)| name == *class);
                    if names_a_class && (in_class_braces || self.after_module(before)) {
                        let line = ident.span().start().line;
                        self.minted.push((line, format!("refusal::{name}")));
                    }
                    self.alias(before, &name, tokens.get(at + 1));
                }
                TokenTree::Literal(literal) => {
                    let syn::Lit::Str(text) = syn::Lit::new(literal.clone()) else {
                        continue;
                    };
                    let text = text.value();
                    let spells_a_class = MODULE_ONLY.iter().any(|(_, token)| {
                        text == *token || text.starts_with(&format!("{token}: "))
                    });
                    if spells_a_class {
                        let line = literal.span().start().line;
                        self.minted.push((line, format!("{text:?}")));
                    }
                }
                TokenTree::Punct(_) => {}
            }
        }
    }

    /// `before` ends in `<class module> ::`.
    fn after_module(&self, before: &[TokenTree]) -> bool {
        let [.., TokenTree::Ident(module), TokenTree::Punct(first), TokenTree::Punct(second)] =
            before
        else {
            return false;
        };
        let path_separator = first.as_char() == ':' && second.as_char() == ':';
        path_separator && self.modules.contains(&module.to_string())
    }

    /// `<class module> as <alias>`: the alias names the class module too.
    fn alias(&mut self, before: &[TokenTree], name: &str, next: Option<&TokenTree>) {
        let [.., TokenTree::Ident(module)] = before else {
            return;
        };
        let Some(TokenTree::Ident(alias)) = next else {
            return;
        };
        let renames_the_module = name == "as" && self.modules.contains(&module.to_string());
        if renames_the_module {
            self.modules.push(alias.to_string());
        }
    }
}

/// `#[doc = "…"]`: a comment, as syn hands it back.
fn is_doc(group: &proc_macro2::Group) -> bool {
    let mut inner = group.stream().into_iter();
    let first_is_doc = matches!(inner.next(), Some(TokenTree::Ident(ident)) if ident == "doc");
    group.delimiter() == Delimiter::Bracket && first_is_doc
}
