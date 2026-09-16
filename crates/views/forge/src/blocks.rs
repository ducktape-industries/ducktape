//! Prose as the screen draws it: the chat inline grammar, read off the
//! wire's structured blocks or tokenized out of a forge body, and the
//! account directory every author is named through.
//!
//! The desktop app folds the same shapes with `chat::client` — a crate this
//! view cannot link (its graph reaches libgit2 and the BLS C sources, which
//! do not build for wasm32). The view is its own crate and carries its own
//! reading, exactly as `host.rs` carries the app's other readings: the wire
//! carries the words, not the functions.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// One inline run of a rich paragraph, pre-sorted into the style arm the
/// span template renders it with: exactly one field is non-empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatSpan {
    pub mention: String,
    pub mention_link: String,
    pub link_text: String,
    pub link: String,
    pub bold_italic: String,
    pub bold: String,
    pub italic: String,
    pub plain: String,
}

/// One rendered block of a body: `paragraph` | `code` | `quote` | `divider`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatBlock {
    pub kind: String,
    pub text: String,
    pub lang: String,
    pub rich: bool,
    pub spans: Vec<ChatSpan>,
}

// ---------- the wire's body ----------

/// Who wrote something, as the chat wire tags it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Party {
    Account(u64),
    Key(Vec<u8>),
    Module(String),
    System,
}

/// One inline mark on a run of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Link(String),
    Mention(Party),
}

/// A run of text with uniform marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub marks: Vec<Mark>,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marks: Vec::new(),
        }
    }
}

/// One block of a message body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    Code { lang: Option<String>, text: String },
    Quote(Vec<Span>),
    Divider,
}

/// A body as the chat wire serializes it: externally tagged variants under
/// `paragraph` / `code` / `quote` / `divider`.
pub fn blocks_of_json(value: &serde_json::Value) -> Vec<Block> {
    value
        .as_array()
        .map(|blocks| blocks.iter().filter_map(block_of_json).collect())
        .unwrap_or_default()
}

fn block_of_json(value: &serde_json::Value) -> Option<Block> {
    if value.as_str() == Some("divider") {
        return Some(Block::Divider);
    }
    let (variant, payload) = value.as_object()?.iter().next()?;
    match variant.as_str() {
        "paragraph" => Some(Block::Paragraph(spans_of_json(payload))),
        "quote" => Some(Block::Quote(spans_of_json(payload))),
        "code" => Some(Block::Code {
            lang: payload["lang"].as_str().map(str::to_owned),
            text: payload["text"].as_str().unwrap_or_default().to_owned(),
        }),
        "divider" => Some(Block::Divider),
        _ => None,
    }
}

fn spans_of_json(value: &serde_json::Value) -> Vec<Span> {
    value
        .as_array()
        .map(|spans| {
            spans
                .iter()
                .map(|span| Span {
                    text: span["text"].as_str().unwrap_or_default().to_owned(),
                    marks: span["marks"]
                        .as_array()
                        .map(|marks| marks.iter().filter_map(mark_of_json).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn mark_of_json(value: &serde_json::Value) -> Option<Mark> {
    match value.as_str() {
        Some("bold") => return Some(Mark::Bold),
        Some("italic") => return Some(Mark::Italic),
        Some(_) | None => (),
    }
    let (variant, payload) = value.as_object()?.iter().next()?;
    match variant.as_str() {
        "link" => Some(Mark::Link(payload.as_str().unwrap_or_default().to_owned())),
        "mention" => Some(Mark::Mention(party_of_json(payload))),
        _ => None,
    }
}

/// A `Party` as the wire tags it: `"system"`, or one of `account` / `key` /
/// `module` carrying its value.
pub fn party_of_json(value: &serde_json::Value) -> Party {
    if value.as_str() == Some("system") {
        return Party::System;
    }
    let Some((variant, payload)) = value.as_object().and_then(|party| party.iter().next()) else {
        return Party::System;
    };
    match variant.as_str() {
        "account" => Party::Account(payload.as_u64().unwrap_or(0)),
        "key" => Party::Key(json_bytes(payload)),
        "module" => Party::Module(payload.as_str().unwrap_or_default().to_owned()),
        _ => Party::System,
    }
}

/// A serde `Vec<u8>` as it arrives over JSON: an array of numbers.
fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .map(|bytes| {
            bytes
                .iter()
                .filter_map(|byte| byte.as_u64().map(|byte| byte as u8))
                .collect()
        })
        .unwrap_or_default()
}

// ---------- the account directory ----------

/// Every account the identity roster holds, by the keys it is reached
/// through: the name a rendered author wears, and whether the account is a
/// program (drawn with the agent plate).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Names {
    /// key hex -> account number
    by_key: BTreeMap<String, u64>,
    /// account number -> display name
    by_account: BTreeMap<u64, String>,
    /// the program-controlled accounts: software, not people
    programs: Vec<u64>,
}

impl Names {
    /// The directory the identity module's account page binds.
    pub fn of_accounts(accounts: &serde_json::Value) -> Self {
        let mut names = Self::default();
        for account in accounts.as_array().cloned().unwrap_or_default() {
            let number = account["number"].as_u64().unwrap_or(0);
            let name = account["name"].as_str().unwrap_or_default().to_owned();
            let program = account["control"]
                .as_object()
                .is_some_and(|control| control.contains_key("program"));
            if program {
                names.programs.push(number);
            }
            names.by_account.insert(number, name);
            for key in account["keys"].as_array().cloned().unwrap_or_default() {
                names
                    .by_key
                    .insert(hex_encode(&json_bytes(&key["pubkey"])), number);
            }
        }
        names
    }

    /// Every account this directory names, newly learned ones added to what
    /// it already held: the roster is read page by page.
    pub fn extend(&mut self, next: Self) {
        self.by_key.extend(next.by_key);
        self.by_account.extend(next.by_account);
        self.programs.extend(next.programs);
    }

    fn of_handle(&self, handle: &str) -> Option<&str> {
        let (kind, id) = handle.split_once(':')?;
        let account = match kind {
            "acct" => id.parse::<u64>().ok()?,
            "user" => *self.by_key.get(id)?,
            _ => return None,
        };
        self.by_account.get(&account).map(String::as_str)
    }

    pub fn composer_choices(
        &self,
        members: &[crate::host::ChatMember],
    ) -> Vec<ducktape_view_composer::MentionChoice> {
        use ducktape_view_composer::MentionChoice;
        let mut choices = self
            .by_account
            .iter()
            .map(|(number, label)| MentionChoice {
                token: format!("<@{number}>"),
                label: label.clone(),
            })
            .collect::<Vec<_>>();
        for member in members {
            let token = match member.key.strip_prefix("acct:") {
                Some(number) => format!("<@{number}>"),
                None => match self.by_key.get(&member.key) {
                    Some(number) => format!("<@{number}>"),
                    None => format!("<@key:{}>", member.key),
                },
            };
            if !choices.iter().any(|choice| choice.token == token) {
                choices.push(MentionChoice {
                    token,
                    label: member.label.clone(),
                });
            }
        }
        choices
    }

    /// A member's label: the bound name, else the shortened key.
    pub fn member_label(&self, key_hex: &str) -> String {
        let handle = match key_hex.contains(':') {
            true => key_hex.to_owned(),
            false => format!("user:{key_hex}"),
        };
        self.of_handle(&handle)
            .map(str::to_owned)
            .unwrap_or_else(|| short_label(key_hex))
    }
}

/// A [`Party`] as the rendered handle the display fns parse — the same
/// vocabulary the index stamps, so every module surface names an author
/// identically.
pub fn party_handle(author: &Party) -> String {
    match author {
        Party::Account(number) => format!("acct:{number}"),
        Party::Key(key) => format!("user:{}", hex_encode(key)),
        Party::Module(id) => format!("module:{id}"),
        Party::System => "system".to_owned(),
    }
}

/// The label an author renders under: the account name the directory binds
/// to their key, else the plain handle rendering.
pub fn author_display(author: &str, names: &Names) -> String {
    names
        .of_handle(author)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| author_name(author))
}

/// The display name for a rendered author string with no directory in
/// frame: a user is named by the shortened key.
fn author_name(author: &str) -> String {
    match author.split_once(':') {
        Some(("user", id)) => format!("user {}", short_label(id)),
        Some(("acct", account)) => format!("account {account}"),
        Some(("module", id)) => id.to_owned(),
        Some(_) | None => "system".to_owned(),
    }
}

fn avatar_source(author: &str, names: &Names) -> String {
    match author.split_once(':') {
        Some(("user", id)) => names.member_label(id),
        Some(("acct", _)) => author_display(author, names),
        Some(("module", id)) => id.to_owned(),
        Some(_) | None => "system".to_owned(),
    }
}

/// The single-glyph avatar label for an author: the first alphanumeric
/// character of its identity, uppercased.
pub fn avatar_initial(author: &str, names: &Names) -> String {
    avatar_source(author, names)
        .chars()
        .find(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "•".to_owned())
}

/// A person's key or account is `human`; a program account and every module
/// or system author is `agent`.
pub fn avatar_kind(author: &str, names: &Names) -> String {
    let program = match author.split_once(':') {
        Some(("user", _)) => return "human".to_owned(),
        Some(("acct", number)) => number
            .parse::<u64>()
            .is_ok_and(|number| names.programs.contains(&number)),
        Some(_) | None => true,
    };
    match program {
        true => "agent".to_owned(),
        false => "human".to_owned(),
    }
}

fn mention_label(party: &Party, names: &Names) -> String {
    match party {
        Party::Account(account) => names
            .by_account
            .get(account)
            .filter(|name| !name.is_empty())
            .map(|name| format!("@{name}"))
            .unwrap_or_else(|| format!("@account-{account}")),
        Party::Key(key) => format!("@{}", names.member_label(&hex_encode(key))),
        Party::Module(module) => format!("@{module}"),
        Party::System => "@system".to_owned(),
    }
}

/// `duck://account/<n>` — the address a mention of an account opens. A
/// mention that names a bare key addresses no account the app can open.
fn mention_link(party: &Party) -> String {
    match party {
        Party::Account(account) => format!("duck://account/{account}"),
        Party::Key(_) | Party::Module(_) | Party::System => String::new(),
    }
}

pub fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    let looks_hex = !hex.is_empty()
        && hex.len().is_multiple_of(2)
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !looks_hex {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

// ---------- blocks as the screen draws them ----------

/// The wire's blocks as render rows, mentions named through the directory.
pub fn blocks_view(blocks: &[Block], names: &Names) -> Vec<ChatBlock> {
    named_blocks(blocks, names).iter().map(block_view).collect()
}

/// Flatten wire blocks into copyable text: one `\n` per block boundary,
/// because that is what a block boundary means (every typed line is its own
/// block).
pub fn message_body(blocks: &[Block], names: &Names) -> String {
    named_blocks(blocks, names)
        .iter()
        .map(|block| match block {
            Block::Paragraph(spans) => span_text(spans),
            Block::Code { lang, text } => match lang {
                Some(lang) => format!("{lang}\n{text}"),
                None => text.clone(),
            },
            Block::Quote(spans) => format!("“{}”", span_text(spans)),
            Block::Divider => "────────".to_owned(),
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn named_blocks(blocks: &[Block], names: &Names) -> Vec<Block> {
    let mut blocks = blocks.to_vec();
    for block in &mut blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => continue,
        };
        for span in spans {
            let mention = span.marks.iter().find_map(|mark| match mark {
                Mark::Mention(party) => Some(party),
                Mark::Bold | Mark::Italic | Mark::Link(_) => None,
            });
            if let Some(party) = mention {
                span.text = mention_label(party, names);
            }
        }
    }
    blocks
}

fn span_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// The row a deleted message draws as.
pub fn deleted_block() -> ChatBlock {
    ChatBlock {
        kind: "paragraph".into(),
        text: "Message deleted".into(),
        lang: String::new(),
        rich: false,
        spans: Vec::new(),
    }
}

fn block_view(block: &Block) -> ChatBlock {
    match block {
        Block::Paragraph(spans) => rich_block("paragraph", spans),
        Block::Quote(spans) => rich_block("quote", spans),
        Block::Code { lang, text } => ChatBlock {
            kind: "code".into(),
            text: text.clone(),
            lang: lang.clone().unwrap_or_default(),
            rich: false,
            spans: Vec::new(),
        },
        Block::Divider => ChatBlock {
            kind: "divider".into(),
            text: String::new(),
            lang: String::new(),
            rich: false,
            spans: Vec::new(),
        },
    }
}

/// A paragraph/quote block. Plain runs keep their exact text for a single
/// wrapping `text`; any inline mark switches to run-level `spans`.
fn rich_block(kind: &str, spans: &[Span]) -> ChatBlock {
    let marked = spans.iter().any(|span| !span.marks.is_empty());
    ChatBlock {
        kind: kind.into(),
        text: span_text(spans),
        lang: String::new(),
        rich: marked,
        spans: match marked {
            true => run_spans(spans),
            false => Vec::new(),
        },
    }
}

/// The one style arm a run renders through: the view's rich-text `for`
/// expands a fixed span template with no conditionals, so the arm decision
/// is made here and encoded as WHICH [`ChatSpan`] field carries the run.
enum SpanArm {
    Link(String),
    Mention(String),
    BoldItalic,
    Bold,
    Italic,
    Plain,
}

/// A link outranks every other mark, a mention outranks emphasis, and
/// emphasis resolves on the (bold, italic) pair.
fn span_arm(span: &Span) -> SpanArm {
    let link = span.marks.iter().find_map(|mark| match mark {
        Mark::Link(url) => Some(url.clone()),
        Mark::Bold | Mark::Italic | Mark::Mention(_) => None,
    });
    if let Some(url) = link {
        return SpanArm::Link(url);
    }
    let mention = span.marks.iter().find_map(|mark| match mark {
        Mark::Mention(party) => Some(party),
        Mark::Bold | Mark::Italic | Mark::Link(_) => None,
    });
    if let Some(party) = mention {
        return SpanArm::Mention(mention_link(party));
    }
    let bold = span.marks.iter().any(|mark| matches!(mark, Mark::Bold));
    let italic = span.marks.iter().any(|mark| matches!(mark, Mark::Italic));
    match (bold, italic) {
        (true, true) => SpanArm::BoldItalic,
        (true, false) => SpanArm::Bold,
        (false, true) => SpanArm::Italic,
        (false, false) => SpanArm::Plain,
    }
}

fn run_spans(spans: &[Span]) -> Vec<ChatSpan> {
    let mut out = Vec::new();
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut rendered = ChatSpan::default();
        match span_arm(span) {
            SpanArm::Link(url) => {
                rendered.link_text = span.text.clone();
                rendered.link = url;
            }
            SpanArm::Mention(link) => {
                rendered.mention = span.text.clone();
                rendered.mention_link = link;
            }
            SpanArm::BoldItalic => rendered.bold_italic = span.text.clone(),
            SpanArm::Bold => rendered.bold = span.text.clone(),
            SpanArm::Italic => rendered.italic = span.text.clone(),
            SpanArm::Plain => rendered.plain = span.text.clone(),
        }
        out.push(rendered);
    }
    out
}

// ---------- forge prose -> blocks ----------

/// One prose body through the chat tokenizer — the same grammar a chat row
/// is rendered with, so a `duck://` ref, a `[label](url)` and a bare
/// `https://` in an issue, a PR description or a review comment become the
/// same link span the app opens through its one open plane. No roster: a
/// forge body is not addressed to a channel, so a mention stays plain ink.
pub fn body_blocks(body: &str) -> Vec<ChatBlock> {
    blocks_view(&parse_message(body), &Names::default())
}

/// Parse prose into wire blocks: fenced ```code``` (optional language), `>`
/// quotes, `---`/`***` dividers, and paragraphs with inline `**bold**` /
/// `__bold__`, `*italic*` / `_italic_`, `[label](url)` references and bare
/// `http(s)`/`duck` links. A SINGLE NEWLINE IS A HARD BREAK: each typed
/// line is its own block.
pub fn parse_message(input: &str) -> Vec<Block> {
    let lines: Vec<&str> = input.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        let opens_fence = trimmed.starts_with("```");
        let is_divider = trimmed == "---" || trimmed == "***";
        let is_quote = trimmed.starts_with('>');
        let is_blank = trimmed.is_empty();
        if opens_fence {
            index = push_code_block(&lines, index, trimmed, &mut blocks);
        } else if is_divider {
            blocks.push(Block::Divider);
            index += 1;
        } else if is_quote {
            index = push_quote_block(&lines, index, &mut blocks);
        } else if is_blank {
            index += 1;
        } else {
            index = push_paragraph_block(&lines, index, &mut blocks);
        }
    }
    if blocks.is_empty() {
        blocks.push(Block::Paragraph(vec![Span::plain(input.trim().to_owned())]));
    }
    blocks
}

fn push_code_block(lines: &[&str], start: usize, opener: &str, blocks: &mut Vec<Block>) -> usize {
    let lang = opener.trim_start_matches('`').trim().to_owned();
    let mut index = start + 1;
    let mut code = Vec::new();
    while index < lines.len() && lines[index].trim() != "```" {
        code.push(lines[index]);
        index += 1;
    }
    let closed = index < lines.len();
    blocks.push(Block::Code {
        lang: (!lang.is_empty()).then_some(lang),
        text: code.join("\n"),
    });
    match closed {
        true => index + 1,
        false => index,
    }
}

fn push_quote_block(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut index = start;
    while index < lines.len() && lines[index].trim().starts_with('>') {
        let stripped = lines[index].trim().trim_start_matches('>').trim_start();
        blocks.push(Block::Quote(inline_spans(stripped)));
        index += 1;
    }
    index
}

fn push_paragraph_block(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut index = start;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        let breaks = trimmed.is_empty()
            || trimmed.starts_with('>')
            || trimmed.starts_with("```")
            || trimmed == "---"
            || trimmed == "***";
        if breaks {
            break;
        }
        blocks.push(Block::Paragraph(inline_spans(trimmed)));
        index += 1;
    }
    index
}

/// The schemes a bare run and a `[label](url)` target may carry. Anything
/// else stays plain text.
const LINK_SCHEMES: [&str; 3] = ["http://", "https://", "duck://"];

/// Scan a single line for inline marks, preserving mention identity inside
/// emphasis.
fn inline_spans(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;
    while index < chars.len() {
        let url = url_len(&chars, index);
        let reference = reference_at(&chars, index);
        let bold = fenced(&chars, index, "**").or_else(|| fenced(&chars, index, "__"));
        let italic = fenced(&chars, index, "*").or_else(|| fenced(&chars, index, "_"));
        if let Some((target, len)) = mention_at(&chars, index) {
            flush_plain(&mut plain, &mut spans);
            let handle: String = chars[index..index + len].iter().collect();
            spans.push(Span {
                text: handle,
                marks: vec![Mark::Mention(target)],
            });
            index += len;
        } else if let Some((label, target, len)) = reference {
            flush_plain(&mut plain, &mut spans);
            spans.push(Span {
                text: label,
                marks: vec![Mark::Link(target)],
            });
            index += len;
        } else if let Some(len) = url {
            flush_plain(&mut plain, &mut spans);
            let target: String = chars[index..index + len].iter().collect();
            spans.push(Span {
                text: target.clone(),
                marks: vec![Mark::Link(target)],
            });
            index += len;
        } else if let Some((inner, len)) = bold {
            flush_plain(&mut plain, &mut spans);
            spans.extend(inline_spans(&inner).into_iter().map(|mut span| {
                span.marks.push(Mark::Bold);
                span
            }));
            index += len;
        } else if let Some((inner, len)) = italic {
            flush_plain(&mut plain, &mut spans);
            spans.extend(inline_spans(&inner).into_iter().map(|mut span| {
                span.marks.push(Mark::Italic);
                span
            }));
            index += len;
        } else {
            plain.push(chars[index]);
            index += 1;
        }
    }
    flush_plain(&mut plain, &mut spans);
    if spans.is_empty() {
        spans.push(Span::plain(String::new()));
    }
    spans
}

fn flush_plain(plain: &mut String, spans: &mut Vec<Span>) {
    if !plain.is_empty() {
        spans.push(Span::plain(std::mem::take(plain)));
    }
}

/// If `chars[at..]` opens a bare link, its length in chars; else `None`.
fn url_len(chars: &[char], at: usize) -> Option<usize> {
    let rest: String = chars[at..].iter().collect();
    let starts_link = LINK_SCHEMES.iter().any(|scheme| rest.starts_with(scheme));
    if !starts_link {
        return None;
    }
    let mut len = chars[at..]
        .iter()
        .take_while(|character| !character.is_whitespace())
        .count();
    // A run stops at whitespace, but the `)` that closes `[x](duck://page/p1)`
    // or `(see https://x)` belongs to the prose around the address.
    while dangling_close(&chars[at..at + len]) {
        len -= 1;
    }
    (len > 0).then_some(len)
}

/// Does this run end in a `)` that opens nowhere inside it? A balanced one
/// (`…/wiki/Foo_(bar)`) is part of the address and stays.
fn dangling_close(run: &[char]) -> bool {
    let closed = run.last() == Some(&')');
    let opens = run.iter().filter(|character| **character == '(').count();
    let closes = run.iter().filter(|character| **character == ')').count();
    closed && closes > opens
}

/// If `chars[at..]` opens a `[label](url)` reference, the label, the target
/// and the total consumed length.
fn reference_at(chars: &[char], at: usize) -> Option<(String, String, usize)> {
    if chars[at] != '[' {
        return None;
    }
    let label_end = chars[at + 1..]
        .iter()
        .position(|character| *character == ']' || *character == '[')?
        + at
        + 1;
    let labelled = chars[label_end] == ']' && chars.get(label_end + 1) == Some(&'(');
    if !labelled {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = chars[url_start..]
        .iter()
        .position(|character| *character == ')' || character.is_whitespace())?
        + url_start;
    let closed = chars[url_end] == ')';
    if !closed {
        return None;
    }
    let label: String = chars[at + 1..label_end].iter().collect();
    let target: String = chars[url_start..url_end].iter().collect();
    let linkable = !label.is_empty() && LINK_SCHEMES.iter().any(|s| target.starts_with(s));
    linkable.then(|| (label, target, url_end + 1 - at))
}

fn mention_at(chars: &[char], at: usize) -> Option<(Party, usize)> {
    let opens = chars.get(at) == Some(&'<') && chars.get(at + 1) == Some(&'@');
    if !opens {
        return None;
    }
    let end = chars[at + 2..].iter().position(|c| *c == '>')? + at + 2;
    let id: String = chars[at + 2..end].iter().collect();
    let party = match id.strip_prefix("key:") {
        Some(key) => Party::Key(hex_bytes(key)?),
        None => {
            let decimal = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit());
            if !decimal {
                return None;
            }
            Party::Account(id.parse().ok()?)
        }
    };
    Some((party, end + 1 - at))
}

fn fenced(chars: &[char], at: usize, marker: &str) -> Option<(String, usize)> {
    let marks: Vec<char> = marker.chars().collect();
    let opens = chars[at..].starts_with(marks.as_slice());
    if !opens {
        return None;
    }
    let body_start = at + marks.len();
    let mut cursor = body_start;
    while cursor + marks.len() <= chars.len() {
        if chars[cursor..].starts_with(marks.as_slice()) {
            let inner: String = chars[body_start..cursor].iter().collect();
            if inner.is_empty() {
                return None;
            }
            return Some((inner, cursor + marks.len() - at));
        }
        cursor += 1;
    }
    None
}
