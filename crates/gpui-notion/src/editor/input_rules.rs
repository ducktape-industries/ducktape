//! Markdown input rules — the shortcuts that turn typed text into nodes and
//! marks, matching Tiptap's StarterKit rules.
//!
//! Node rules come from the registered [`BlockSpec`]s, so a new node type
//! brings its own rule with it. Mark rules are listed here, matching the
//! template's regexes.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use gpui_kit::{App, Context, Window};
use regex::Regex;

use super::block::{BlockAttrs, BlockId, BlockRegistry};
use super::mark::MarkKind;
use super::view::{Caret, NotionEditor};

/// Chooses who interprets source text typed into the editor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputRuleMode {
    #[default]
    BuiltIn,
    Application,
}

/// Compiled regexes, shared across blocks and edits.
fn regex_for(pattern: &str) -> Regex {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    cache
        .entry(pattern.to_string())
        .or_insert_with(|| Regex::new(pattern).expect("input rule regex"))
        .clone()
}

/// An inline rule: `**bold**`, `` `code` `` and friends, applied when the
/// closing delimiter is typed.
struct InlineRule {
    /// Matched against the text before the caret; group 1 is the whole match
    /// including delimiters, group 2 the text to keep.
    pattern: &'static str,
    mark: fn() -> MarkKind,
}

const INLINE_RULES: &[InlineRule] = &[
    InlineRule {
        pattern: r"(?:^|\s)(\*\*(?P<body>[^*]+)\*\*)$",
        mark: || MarkKind::Bold,
    },
    InlineRule {
        pattern: r"(?:^|\s)(__(?P<body>[^_]+)__)$",
        mark: || MarkKind::Bold,
    },
    InlineRule {
        pattern: r"(?:^|\s)(~~(?P<body>[^~]+)~~)$",
        mark: || MarkKind::Strike,
    },
    InlineRule {
        pattern: r"(?:^|\s)(==(?P<body>[^=]+)==)$",
        mark: || MarkKind::Highlight(None),
    },
    InlineRule {
        pattern: r"(?:^|\s)(\*(?P<body>[^*\s][^*]*)\*)$",
        mark: || MarkKind::Italic,
    },
    InlineRule {
        pattern: r"(?:^|\s)(_(?P<body>[^_\s][^_]*)_)$",
        mark: || MarkKind::Italic,
    },
    InlineRule {
        pattern: r"(?:^|\s)(`(?P<body>[^`]+)`)$",
        mark: || MarkKind::Code,
    },
];

/// Smart punctuation, from the template's `Typography` extension.
const TYPOGRAPHY_RULES: &[(&str, &str)] = &[
    ("(c)", "©"),
    ("(C)", "©"),
    ("(r)", "®"),
    ("(R)", "®"),
    ("(tm)", "™"),
    ("(TM)", "™"),
    ("...", "…"),
    ("<-", "←"),
    ("->", "→"),
    ("--", "–"),
    ("!=", "≠"),
    ("<=", "≤"),
    (">=", "≥"),
    ("+/-", "±"),
];

impl NotionEditor {
    pub fn set_input_rule_mode(&mut self, mode: InputRuleMode) {
        self.input_rule_mode = mode;
    }

    /// Convert a whole line that begins with a markdown prefix, the way a
    /// pasted document's lines are read. Unlike typing, the rest of the line
    /// is already there, so the rule only has to match its start.
    pub(crate) fn apply_markdown_prefix(
        &mut self,
        id: BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let application_owned = self.input_rule_mode == InputRuleMode::Application;
        if application_owned {
            return false;
        }
        let Some(ix) = self.index_of(id) else {
            return false;
        };
        if !self.spec_at(ix, cx).caps().input_rules {
            return false;
        }
        let text = self.blocks[ix].text.clone();

        for (_, rule) in BlockRegistry::global(cx).input_rules() {
            let pattern = rule.pattern.trim_end_matches('$');
            let regex = regex_for(pattern);
            let Some(caps) = regex.captures(&text) else {
                continue;
            };
            let Some(whole) = caps.get(0) else { continue };
            if whole.start() != 0 || whole.end() == 0 {
                continue;
            }
            let Some((ty, attrs)) = (rule.build)(&caps) else {
                continue;
            };
            if self.blocks[ix].ty == ty && self.blocks[ix].attrs == attrs {
                continue;
            }

            self.edit_block_text(ix, 0..whole.end(), "", Some(0), window, cx);
            self.set_block_type(id, ty, attrs, window, cx);
            return true;
        }
        false
    }

    /// Run the input rules after an edit. Returns true when one fired.
    pub(crate) fn run_input_rules(
        &mut self,
        id: BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let application_owned = self.input_rule_mode == InputRuleMode::Application;
        if application_owned {
            return false;
        }
        let Some(ix) = self.index_of(id) else {
            return false;
        };
        if !self.spec_at(ix, cx).caps().input_rules {
            return false;
        }
        let caret = self.blocks[ix].state.read(cx).cursor();
        let text = self.blocks[ix].text.clone();
        if caret > text.len() || !text.is_char_boundary(caret) {
            return false;
        }

        self.apply_node_rule(ix, &text, caret, window, cx)
            || self.apply_inline_rule(ix, &text, caret, window, cx)
            || self.apply_typography_rule(ix, &text, caret, window, cx)
    }

    /// A rule at the start of a paragraph turns it into another node type.
    fn apply_node_rule(
        &mut self,
        ix: usize,
        text: &str,
        caret: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let head = &text[..caret];
        let rules = BlockRegistry::global(cx).input_rules();

        for (_, rule) in &rules {
            let regex = regex_for(rule.pattern);
            let Some(caps) = regex.captures(head) else {
                continue;
            };
            // The rule must consume everything typed so far in the block.
            if caps.get(0).map(|m| m.end()) != Some(head.len()) {
                continue;
            }
            let Some((ty, attrs)) = (rule.build)(&caps) else {
                continue;
            };
            // A rule never re-fires on the node it produces, so typing `- `
            // inside a bullet item leaves the dashes alone.
            if self.blocks[ix].ty == ty && self.blocks[ix].attrs == attrs {
                continue;
            }

            let consumed = caps.get(0).map(|m| m.len()).unwrap_or(0);
            let id = self.blocks[ix].id;
            self.edit_block_text(ix, 0..consumed, "", Some(0), window, cx);
            self.set_block_type(id, ty.clone(), attrs, window, cx);

            if !BlockRegistry::global(cx).get(&ty).caps().textual {
                let next = self.ensure_paragraph_after(ix, window, cx);
                self.focus_block(next, Caret::Start, window, cx);
            } else {
                self.focus_block(id, Caret::Start, window, cx);
            }
            return true;
        }
        false
    }

    fn apply_inline_rule(
        &mut self,
        ix: usize,
        text: &str,
        caret: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let head = &text[..caret];
        for rule in INLINE_RULES {
            let regex = regex_for(rule.pattern);
            let Some(caps) = regex.captures(head) else {
                continue;
            };
            let (Some(whole), Some(body)) = (caps.get(1), caps.name("body")) else {
                continue;
            };
            let body_text = body.as_str().to_string();
            if body_text.trim().is_empty() {
                continue;
            }

            let start = whole.start();
            let end = whole.end();
            let caret_after = start + body_text.len();

            self.edit_block_text(ix, start..end, &body_text, Some(caret_after), window, cx);
            if let Some(block) = self.blocks.get_mut(ix) {
                block
                    .marks
                    .add((rule.mark)(), start..start + body_text.len());
                // What follows the rule is plain again.
                block.stored_marks = Some(Vec::new());
            }
            let id = self.blocks[ix].id;
            self.apply_decorations(id, cx);
            return true;
        }
        false
    }

    fn apply_typography_rule(
        &mut self,
        ix: usize,
        text: &str,
        caret: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let head = &text[..caret];
        for (from, to) in TYPOGRAPHY_RULES {
            if !head.ends_with(from) {
                continue;
            }
            // `-->` should become `→`, not `–>`; longer rules win by order.
            let start = caret - from.len();
            self.edit_block_text(ix, start..caret, to, Some(start + to.len()), window, cx);
            return true;
        }
        false
    }
}

/// Node attributes a rule may need to look up by name, exposed for specs in
/// other crates.
pub fn attrs_from_capture(caps: &regex::Captures, name: &str) -> BlockAttrs {
    let mut attrs = BlockAttrs::default();
    if let Some(value) = caps.name(name) {
        attrs.set_extra(name.to_string(), value.as_str().to_string());
    }
    attrs
}

/// Initialise the rule engine; kept for symmetry with the other modules.
pub fn init(_cx: &mut App) {}
