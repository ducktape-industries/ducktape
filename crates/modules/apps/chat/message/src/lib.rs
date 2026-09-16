//! Pure conversation message types and Markdown parsing. No host or consensus runtime.
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

pub type AccountNumber = u64;

/// Reconstruct a committed body from the original payload and its assigned
/// key resolutions. Every distinct key consumes one account, in appearance
/// order; repeated keys reuse that resolution. This never consults identity,
/// whose current key ownership may differ from the committed operation's.
pub fn resolve_assigned_mentions(
    mut blocks: Vec<Block>,
    key_mentions: &[AccountNumber],
) -> Result<Vec<Block>, String> {
    let mut accounts = key_mentions.iter();
    let mut resolved = BTreeMap::new();
    for block in &mut blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => continue,
        };
        for span in spans {
            for mark in &mut span.marks {
                let Mark::Mention(Party::Key(key)) = mark else {
                    continue;
                };
                let account = match resolved.get(key) {
                    Some(account) => *account,
                    None => {
                        let account = *accounts.next().ok_or("missing assigned mention account")?;
                        if account == 0 {
                            return Err("assigned mention account is zero".into());
                        }
                        resolved.insert(key.clone(), account);
                        account
                    }
                };
                *mark = Mark::Mention(Party::Account(account));
            }
        }
    }
    if accounts.next().is_some() {
        return Err("unused assigned mention accounts".into());
    }
    Ok(blocks)
}

/// who acts on chat state — the ONE party shape every author, owner, member,
/// huddle participant, reactor and mention target takes.
///
/// the module derives the acting party from `Env.origin` at write time, never
/// from a payload: a member key resolves through identity to the account
/// holding it, a program origin IS its account, a signed key that identity
/// does not know stays a key (a node operating a channel under its own key
/// holds no account and is never spelled as one), a module is itself. an
/// account is the stable identity a person's many keys and a keyless program
/// share, so it is what relations and rosters name whenever one exists.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Party {
    /// an identity account: a resolved member key, or the program account the
    /// host ran the write as.
    Account(AccountNumber),
    /// an authenticated signing key that holds no account (non-empty).
    Key(Vec<u8>),
    /// a module that emitted the write as a follow-up.
    Module(String),
    /// genesis / system-internal.
    System,
}

impl Party {
    /// the account this party is, if it is one — the recipient an attribution
    /// relation can name. a key, a module and the system are not accounts.
    pub fn account(&self) -> Option<AccountNumber> {
        match self {
            Party::Account(account) => Some(*account),
            Party::Key(_) | Party::Module(_) | Party::System => None,
        }
    }

    /// a person's party — an account or a key — as opposed to trusted code.
    /// post policy, the `:` channel namespace, creation caps and huddles all
    /// distinguish people from modules and the system on exactly this line.
    pub fn is_person(&self) -> bool {
        match self {
            Party::Account(_) | Party::Key(_) => true,
            Party::Module(_) | Party::System => false,
        }
    }
}

/// inline formatting applied to a [`Span`]. mentions are structured so
/// hook parsing stays deterministic.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Mark {
    Bold,
    Italic,
    Link(String),
    /// a mention NAMES AN ACCOUNT: `Party::Account` names it directly and must
    /// exist; `Party::Key` names the account holding that key and is resolved
    /// at write time; a module or system mention is rejected. a write whose
    /// mention resolves to no account is rejected whole.
    Mention(Party),
}

/// a run of text with uniform marks.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub text: String,
    pub marks: Vec<Mark>,
}

impl Span {
    /// a plain, unmarked span.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marks: Vec::new(),
        }
    }
}

/// one block of a message body.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    Paragraph(Vec<Span>),
    Code { lang: Option<String>, text: String },
    Quote(Vec<Span>),
    Divider,
}

impl Block {
    /// a single-span plain paragraph.
    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::Paragraph(vec![Span::plain(text)])
    }
}

/// Parse composer text into wire `Block`s: fenced ```code``` (optional language),
/// `>` quotes, `---`/`***` dividers, and paragraphs with inline `**bold**` /
/// `__bold__`, `*italic*` / `_italic_`, and bare `http(s)` links. Everything the
/// `chat` wire enums can round-trip — nothing client-only.
///
/// A SINGLE NEWLINE IS A HARD BREAK, not CommonMark's soft break. The composer
/// hint says `⇧↵ newline` and `⇧↵` really does put a `\n` in the buffer, so
/// folding consecutive lines into one paragraph with a space posted a typed
/// list as "- apples - bananas - pears" — and the fold happens on the way to
/// the CHAIN, so no renderer recovers it. Each line is therefore its own block.
/// A rendered break has to be a block boundary rather than a `\n` inside one:
/// a marked-up line renders as a single rich-text paragraph (`run_spans`),
/// one paragraph widget per typed line.
/// Parse canonical `<@account>` and `<@key:hex>` tokens into mention marks.
/// Display names are never interpreted as recipient identities.
pub fn parse_message(input: &str) -> Vec<Block> {
    let lines: Vec<&str> = input.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
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
        blocks.push(Block::paragraph(input.trim().to_string()));
    }
    blocks
}

fn push_code_block(lines: &[&str], start: usize, opener: &str, blocks: &mut Vec<Block>) -> usize {
    let lang = opener.trim_start_matches('`').trim().to_string();
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
    if closed { index + 1 } else { index }
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

/// Scan a single line of text for inline marks, preserving mention identity
/// inside emphasis. Bare `http(s)://` and `duck://`
/// runs become `Link`s, as does a `[label](url)` reference — one span whose
/// text is the label and whose mark carries the target.
pub fn inline_spans(text: &str) -> Vec<Span> {
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
                // A directory account is one mark, regardless of its keys.
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
///
/// `duck://` is a link scheme here exactly as `http(s)://` is: the app
/// classifies a pressed link through its own module table
/// (`backend/duck_uri.rs`) and refuses what it cannot open, so the tokenizer
/// marks the run and decides nothing about where it points.
fn url_len(chars: &[char], at: usize) -> Option<usize> {
    let rest: String = chars[at..].iter().collect();
    let starts_link = LINK_SCHEMES.iter().any(|scheme| rest.starts_with(scheme));
    if !starts_link {
        return None;
    }
    let mut len = chars[at..]
        .iter()
        .take_while(|c| !c.is_whitespace())
        .count();
    // A run stops at whitespace, but the `)` that closes `[x](duck://page/p1)`
    // or `(see https://x)` belongs to the prose around the address, not to it.
    while dangling_close(&chars[at..at + len]) {
        len -= 1;
    }
    (len > 0).then_some(len)
}

/// Does this run end in a `)` that opens nowhere inside it? A balanced one
/// (`…/wiki/Foo_(bar)`) is part of the address and stays.
fn dangling_close(run: &[char]) -> bool {
    let closed = run.last() == Some(&')');
    let opens = run.iter().filter(|c| **c == '(').count();
    let closes = run.iter().filter(|c| **c == ')').count();
    closed && closes > opens
}

/// If `chars[at..]` opens a `[label](url)` reference — the form agents already
/// emit for `duck://` refs (`runs::inject`) — the label, the target, and the
/// total consumed length. The label is one line with no nested brackets and
/// the target is one whitespace-free run in a known scheme; anything else is
/// not a reference and stays the plain text it was typed as.
fn reference_at(chars: &[char], at: usize) -> Option<(String, String, usize)> {
    if chars[at] != '[' {
        return None;
    }
    let label_end = chars[at + 1..]
        .iter()
        .position(|c| *c == ']' || *c == '[')?
        + at
        + 1;
    let labelled = chars[label_end] == ']' && chars.get(label_end + 1) == Some(&'(');
    if !labelled {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = chars[url_start..]
        .iter()
        .position(|c| *c == ')' || c.is_whitespace())?
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

pub fn mention_at(chars: &[char], at: usize) -> Option<(Party, usize)> {
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

/// If `chars[at..]` opens with `marker` and has a later closing `marker`, the
/// enclosed text and the total consumed length (markers included).
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

/// An even-length all-hex string back to its bytes; anything else is not hex.
fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    let looks_hex = !hex.is_empty()
        && hex.len().is_multiple_of(2)
        && hex.bytes().all(|b| b.is_ascii_hexdigit());
    if !looks_hex {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

const LINK_SCHEMES: [&str; 3] = ["http://", "https://", "duck://"];

/// Resolve display labels for mention tokens while preserving fenced code and byte ranges.
pub fn draft_mentions(
    text: &str,
    label: impl Fn(&Party) -> String,
) -> (String, Vec<(std::ops::Range<usize>, Party)>) {
    let chars: Vec<char> = text.chars().collect();
    let mut display = String::new();
    let mut mentions = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(consumed) = code_fence_len(&chars, index) {
            display.extend(&chars[index..index + consumed]);
            index += consumed;
            continue;
        }
        if let Some((party, consumed)) = mention_at(&chars, index) {
            let start = display.len();
            display.push_str(&label(&party));
            mentions.push((start..display.len(), party));
            index += consumed;
        } else {
            display.push(chars[index]);
            index += 1;
        }
    }
    (display, mentions)
}

/// Fenced code is literal, including token-shaped text inside it.
fn code_fence_len(chars: &[char], at: usize) -> Option<usize> {
    let line_start = at == 0 || chars[at - 1] == '\n';
    if !line_start {
        return None;
    }
    let mut lines = chars[at..].split_inclusive(|ch| *ch == '\n');
    let opener = lines.next()?;
    let opener_text: String = opener.iter().collect();
    let opens_fence = opener_text.trim().starts_with("```");
    if !opens_fence {
        return None;
    }
    let mut consumed = opener.len();
    for line in lines {
        consumed += line.len();
        let line_text: String = line.iter().collect();
        let closes_fence = line_text.trim() == "```";
        if closes_fence {
            break;
        }
    }
    Some(consumed)
}
