//! hashtag tags on the derived chat view — extraction, postings, catalog,
//! and the tag queries. node-local like everything in this index: no key
//! written here is ever part of any `root()`/root-hash.
//!
//! key spaces (inside chat's per-module index database, next to `tok/`):
//! - `tag/{label}/{channel}/{rseq}` — one posting per (tag, message), value =
//!   [`TokRef`] like a `tok/` posting. `rseq = u64::MAX - seq` in fixed hex,
//!   so key order within a channel is NEWEST FIRST: a channel-scoped tag page
//!   streams straight off one scan, and a label's newest live seq is the
//!   first posting under its prefix.
//! - `tagcat/{channel}/{label}`      — [`TagCat`]: the count of LIVE messages
//!   carrying the tag in that channel. count only: the fold's reads are
//!   get-only, so a stored `last_seq` could not be re-derived when the newest
//!   tagged message is deleted — instead `last_seq` is read at query time from
//!   the newest posting, which the reversed key makes an O(1) probe.
//!
//! extraction grammar (see the design doc): `#` + 1..=64 chars of Unicode
//! letters/digits/`_`/`-`, opened only at start-of-text or
//! after whitespace/punctuation — never mid-word (`foo#bar`), after another
//! `#`, or after `/`/`&` (URL fragments, HTML entities). Paragraph and Quote
//! spans only; Code blocks and Link-marked spans never carry tags. the index
//! label is the NFC-normalized lowercase form; the as-typed display form
//! stays in the message text. at most [`MAX_TAGS_PER_MESSAGE`] distinct
//! labels index per message.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use index_guest::search::DEFAULT_POSTING_CAP;
use index_guest::{Fail, MAX_SCAN_LIMIT, StateRead, Writes};

use super::{
    DEFAULT_SEARCH_LIMIT, FAIL_BAD_REQUEST, FAIL_ROW_DECODE, MAX_SEARCH_LIMIT, MsgRow, TokRef,
    msg_key,
};
use crate::{Block, Mark};

/// distinct tag labels indexed per message; later tags are dropped.
pub const MAX_TAGS_PER_MESSAGE: usize = 16;
/// chars (not bytes) a tag may carry after the `#`; longer runs are not tags.
pub const MAX_TAG_CHARS: usize = 64;

/// one catalog row of the `Tags` reply: a label, how many live messages carry
/// it (in the channel scope asked), and the newest such message's seq.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagRow {
    pub tag: String,
    pub count: u64,
    pub last_seq: u64,
}

/// the stored catalog value — live-message count only (see the module doc for
/// why `last_seq` is deliberately NOT stored).
#[derive(Debug, Serialize, Deserialize)]
struct TagCat {
    count: u64,
}

// ── keys ────────────────────────────────────────────────────────────────────

/// a tag posting's key. `u64::MAX - seq` keeps per-channel postings newest
/// first in key order (seq is per-channel, so the inversion never collides).
pub(super) fn tag_key(label: &str, channel: &str, seq: u64) -> String {
    format!("tag/{label}/{channel}/{:016x}", u64::MAX - seq)
}

fn tag_channel_prefix(label: &str, channel: &str) -> String {
    format!("tag/{label}/{channel}/")
}

fn tag_prefix(label: &str) -> String {
    format!("tag/{label}/")
}

pub(super) fn catalog_key(channel: &str, label: &str) -> String {
    format!("tagcat/{channel}/{label}")
}

/// the catalog value for `count` — one encoder so every write path produces
/// byte-identical entries.
fn encode_catalog(count: u64) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(&TagCat { count }).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

// ── extraction ──────────────────────────────────────────────────────────────

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// whether a `#` after `prev` opens a tag: start-of-text or any whitespace /
/// punctuation boundary — but never mid-word, after another `#` (no `##tag`),
/// or after `/` / `&` (URL fragments like `…/#frag`, entities like `&#39;`).
fn opens_tag(prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(p) => !p.is_alphanumeric() && !matches!(p, '#' | '/' | '&'),
    }
}

/// NFC-normalized lowercase index label of a raw tag body.
pub(super) fn normalize(raw: &str) -> String {
    raw.nfc().collect::<String>().to_lowercase()
}

/// append the labels found in one plain-text run to `out`, deduplicated
/// against what is already there, in appearance order.
fn collect_labels(text: &str, out: &mut Vec<String>) {
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '#' && opens_tag(prev) {
            let rest = &text[i + 1..];
            let mut char_count = 0usize;
            let mut byte_len = 0usize;
            for rc in rest.chars() {
                if !is_tag_char(rc) {
                    break;
                }
                char_count += 1;
                byte_len += rc.len_utf8();
            }
            // consume the whole run either way, so a rejected (over-long) run
            // is never re-entered mid-way as a fresh boundary.
            for _ in 0..char_count {
                chars.next();
            }
            prev = Some(rest[..byte_len].chars().next_back().unwrap_or(c));
            if (1..=MAX_TAG_CHARS).contains(&char_count) {
                let label = normalize(&rest[..byte_len]);
                if !out.contains(&label) {
                    out.push(label);
                }
            }
            continue;
        }
        prev = Some(c);
    }
}

/// the distinct tag labels of one message body, appearance order, capped at
/// [`MAX_TAGS_PER_MESSAGE`]. Paragraph/Quote spans only; Code blocks and
/// Link-marked spans are never scanned.
pub(super) fn labels(blocks: &[Block]) -> Vec<String> {
    let mut out = Vec::new();
    for block in blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => continue,
        };
        for span in spans {
            if span.marks.iter().any(|m| matches!(m, Mark::Link(_))) {
                continue;
            }
            collect_labels(&span.text, &mut out);
        }
    }
    out.truncate(MAX_TAGS_PER_MESSAGE);
    out
}

// ── fold maintenance ────────────────────────────────────────────────────────

/// delete the tag postings of `row`'s current tag set (the edit/delete
/// counterpart of the postings `put_row_and_toks` emits).
pub(super) fn delete_postings(out: &mut Writes, row: &MsgRow) {
    for label in &row.tags {
        index_guest::delete(out, tag_key(label, &row.channel_id, row.seq));
    }
}

/// fold one head transition's tag-set diff into the catalog: labels in `new`
/// but not `old` count up, labels in `old` but not `new` count down (their
/// entry is deleted at zero). a post passes `old = []`, a delete `new = []`.
pub(super) fn fold_catalog(
    read: &impl StateRead,
    out: &mut Writes,
    channel: &str,
    old: &[String],
    new: &[String],
) -> Result<(), Fail> {
    for label in new.iter().filter(|l| !old.contains(l)) {
        bump(read, out, channel, label, 1)?;
    }
    for label in old.iter().filter(|l| !new.contains(l)) {
        bump(read, out, channel, label, -1)?;
    }
    Ok(())
}

fn bump(
    read: &impl StateRead,
    out: &mut Writes,
    channel: &str,
    label: &str,
    delta: i64,
) -> Result<(), Fail> {
    let key = catalog_key(channel, label);
    let count = match read.get(key.as_bytes()) {
        Some(bytes) => {
            serde_json::from_slice::<TagCat>(&bytes)
                .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?
                .count
        }
        None => 0,
    };
    let count = if delta >= 0 {
        count + delta as u64
    } else {
        count.saturating_sub(delta.unsigned_abs())
    };
    if count == 0 {
        index_guest::delete(out, key);
    } else {
        index_guest::put(out, key, encode_catalog(count)?);
    }
    Ok(())
}

// ── serving ─────────────────────────────────────────────────────────────────

/// walk every entry under `prefix`, page by page.
fn scan_all<F>(read: &impl StateRead, prefix: &str, mut f: F) -> Result<(), Fail>
where
    F: FnMut(&[u8], &[u8]) -> Result<(), Fail>,
{
    let mut cursor: Option<String> = None;
    loop {
        let page = read.scan_page(
            prefix.as_bytes(),
            cursor.as_deref().map(str::as_bytes),
            MAX_SCAN_LIMIT,
        );
        for (key, value) in &page.entries {
            f(key, value)?;
        }
        match page.next_after {
            Some(next) if page.has_more => cursor = Some(next),
            _ => break,
        }
    }
    Ok(())
}

fn decode_tok(value: &[u8]) -> Result<TokRef, Fail> {
    serde_json::from_slice(value).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

/// a label's newest live seq in one channel: the first posting under the
/// channel prefix WHOSE STORED REF names that channel, or None when no such
/// posting is live. the prefix alone is not scope — `tag/{label}/g/` also
/// matches a sub-channel like `g/0`, whose keys can even sort AHEAD of `g`'s
/// own hex rseqs — but same-channel keys keep their newest-first order
/// relative to each other, so the first stored-channel match IS the max.
/// bounded by the posting cap like every tag walk; a prefix that exhausts the
/// cap without a match reports none.
fn newest_seq(read: &impl StateRead, label: &str, channel: &str) -> Result<Option<u64>, Fail> {
    let prefix = tag_channel_prefix(label, channel);
    let mut cursor: Option<String> = None;
    let mut walked = 0usize;
    loop {
        let page = read.scan_page(
            prefix.as_bytes(),
            cursor.as_deref().map(str::as_bytes),
            MAX_SCAN_LIMIT,
        );
        for (_, value) in &page.entries {
            if walked == DEFAULT_POSTING_CAP {
                return Ok(None);
            }
            walked += 1;
            let r = decode_tok(value)?;
            if r.channel_id == channel {
                return Ok(Some(r.seq));
            }
        }
        match page.next_after {
            Some(next) if page.has_more => cursor = Some(next),
            _ => break,
        }
    }
    Ok(None)
}

/// the `Tags` query: the catalog of one channel (or, with no channel, every
/// channel aggregated per label), ordered count desc then tag asc, clamped
/// like search.
pub(super) fn serve_tags(
    read: &impl StateRead,
    channel_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<TagRow>, Fail> {
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let prefix = match &channel_id {
        Some(channel) => format!("tagcat/{channel}/"),
        None => "tagcat/".to_string(),
    };
    // label → (aggregated count, channels carrying it — for last_seq probes).
    let mut agg: std::collections::BTreeMap<String, (u64, Vec<String>)> =
        std::collections::BTreeMap::new();
    scan_all(read, &prefix, |key, value| {
        let rest = String::from_utf8_lossy(&key[prefix.len()..]);
        // a label never contains `/` (tag chars only), so the LAST segment is
        // the label and everything before it is the channel.
        let (channel, label) = match &channel_id {
            Some(channel) => {
                let label = rest.into_owned();
                // the prefix also matches SUB-channels — `tagcat/g/` catches
                // channel `g/0`, whose rows would otherwise surface as bogus
                // labels like `0/shared`. a real label is tag chars only, so
                // anything that fails the grammar is another channel's row.
                if !label.chars().all(is_tag_char) {
                    return Ok(());
                }
                (channel.clone(), label)
            }
            None => match rest.rsplit_once('/') {
                Some((channel, label)) => (channel.to_string(), label.to_string()),
                None => return Ok(()),
            },
        };
        let count = serde_json::from_slice::<TagCat>(value)
            .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?
            .count;
        let entry = agg.entry(label).or_insert((0, Vec::new()));
        entry.0 += count;
        entry.1.push(channel);
        Ok(())
    })?;
    let mut rows: Vec<(String, u64, Vec<String>)> = agg
        .into_iter()
        .map(|(label, (count, channels))| (label, count, channels))
        .collect();
    // count desc, then tag asc (the BTreeMap already yields labels ascending,
    // and the sort is stable).
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    rows.truncate(limit);
    let mut out = Vec::with_capacity(rows.len());
    for (label, count, channels) in rows {
        let mut last_seq = 0u64;
        for channel in &channels {
            if let Some(seq) = newest_seq(read, &label, channel)? {
                last_seq = last_seq.max(seq);
            }
        }
        out.push(TagRow {
            tag: label,
            count,
            last_seq,
        });
    }
    Ok(out)
}

/// the `TagSearch` query: every live message carrying EXACTLY `tag` (the
/// query normalizes like the indexer, so `#Rust` finds `#rust`), newest
/// first, clamped like search. one walk over the label's postings — a
/// channel scope narrows the scan prefix but the SCOPE ITSELF is the stored
/// ref's channel id, exactly like `Search`: the prefix alone also matches
/// sub-channels (`tag/{label}/g/` catches channel `g/0`). collects up to the
/// posting cap and ranks by time like `Search`.
pub(super) fn serve_tag_search(
    read: &impl StateRead,
    tag: &str,
    channel_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<MsgRow>, Fail> {
    let label = normalize(tag.trim().trim_start_matches('#'));
    if label.is_empty() || label.chars().count() > MAX_TAG_CHARS || !label.chars().all(is_tag_char)
    {
        return Err(Fail::new(FAIL_BAD_REQUEST, "not a valid tag"));
    }
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let prefix = match &channel_id {
        Some(channel) => tag_channel_prefix(&label, channel),
        None => tag_prefix(&label),
    };
    let mut refs: Vec<TokRef> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut walked = 0usize;
    'walk: loop {
        let page = read.scan_page(
            prefix.as_bytes(),
            cursor.as_deref().map(str::as_bytes),
            MAX_SCAN_LIMIT,
        );
        for (_, value) in &page.entries {
            if walked == DEFAULT_POSTING_CAP {
                break 'walk;
            }
            walked += 1;
            let r = decode_tok(value)?;
            if channel_id.as_ref().is_none_or(|c| &r.channel_id == c) {
                refs.push(r);
            }
        }
        match page.next_after {
            Some(next) if page.has_more => cursor = Some(next),
            _ => break,
        }
    }
    // newest first; (channel, seq) tiebreak for a stable order — exactly
    // `Search`'s ranking.
    refs.sort_by(|a, b| (b.time, &b.channel_id, b.seq).cmp(&(a.time, &a.channel_id, a.seq)));
    refs.truncate(limit);
    let mut hits = Vec::with_capacity(refs.len());
    for r in refs {
        if let Some(bytes) = read.get(msg_key(&r.channel_id, r.seq).as_bytes()) {
            let row: MsgRow = serde_json::from_slice(&bytes)
                .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
            hits.push(row);
        }
    }
    Ok(hits)
}

// ── extraction tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;

    fn para(text: &str) -> Vec<Block> {
        vec![Block::paragraph(text)]
    }

    #[test]
    fn extracts_basic_tags_in_appearance_order() {
        assert_eq!(
            labels(&para("shipping #beta today, then #alpha")),
            ["beta", "alpha"]
        );
    }

    #[test]
    fn extracts_non_ascii_underscore_and_hyphen_tags() {
        assert_eq!(
            labels(&para("#naïve support #snake_case #kebab-case")),
            ["naïve", "snake_case", "kebab-case"]
        );
    }

    #[test]
    fn start_of_text_and_punctuation_open_tags() {
        assert_eq!(labels(&para("#lead rest")), ["lead"]);
        assert_eq!(
            labels(&para("(#paren) ,#comma !#bang")),
            ["paren", "comma", "bang"]
        );
    }

    #[test]
    fn mid_word_hash_is_not_a_tag() {
        assert!(labels(&para("foo#bar issue#42")).is_empty());
    }

    #[test]
    fn url_fragments_entities_and_double_hash_are_not_tags() {
        assert!(labels(&para("see https://x.com/page#frag")).is_empty());
        assert!(labels(&para("see https://x.com/#frag")).is_empty());
        assert!(labels(&para("&#39; entity")).is_empty());
        assert!(labels(&para("##double")).is_empty());
    }

    #[test]
    fn bare_hash_and_heading_are_not_tags() {
        assert!(labels(&para("# heading")).is_empty());
        assert!(labels(&para("#")).is_empty());
        assert!(labels(&para("count # things")).is_empty());
    }

    #[test]
    fn code_blocks_never_carry_tags() {
        let blocks = vec![
            Block::Code {
                lang: Some("sh".into()),
                text: "#!/bin/sh\necho #nope".into(),
            },
            Block::paragraph("#yes"),
        ];
        assert_eq!(labels(&blocks), ["yes"]);
    }

    #[test]
    fn link_marked_spans_never_carry_tags() {
        let blocks = vec![Block::Paragraph(vec![
            Span {
                text: "https://x.com/#nope".into(),
                marks: vec![Mark::Link("https://x.com/#nope".into())],
            },
            Span::plain(" #yes"),
        ])];
        assert_eq!(labels(&blocks), ["yes"]);
    }

    #[test]
    fn quote_blocks_carry_tags_dividers_do_not() {
        let blocks = vec![Block::Quote(vec![Span::plain("#quoted")]), Block::Divider];
        assert_eq!(labels(&blocks), ["quoted"]);
    }

    #[test]
    fn normalizes_nfc_lowercase_and_dedups_labels() {
        // decomposed E + acute accent folds to the lowercase precomposed character.
        assert_eq!(normalize("E\u{301}"), "é");
        // three spellings, one label.
        assert_eq!(labels(&para("#Rust #RUST #rust")), ["rust"]);
    }

    #[test]
    fn tag_length_bounds() {
        let max = "a".repeat(MAX_TAG_CHARS);
        assert_eq!(labels(&para(&format!("#{max}"))), [max.as_str()]);
        // one char over: the whole run is rejected, not truncated — and the
        // run does not re-open a tag mid-way.
        assert!(labels(&para(&format!("#{max}b"))).is_empty());
        // non-ASCII text counts CHARS, not bytes.
        let accented = "é".repeat(MAX_TAG_CHARS);
        assert_eq!(labels(&para(&format!("#{accented}"))), [accented]);
    }

    #[test]
    fn digits_are_valid_tags() {
        assert_eq!(labels(&para("we are #1")), ["1"]);
    }

    #[test]
    fn caps_at_sixteen_distinct_labels() {
        let text: String = (0..20).map(|i| format!("#tag{i:02} ")).collect();
        let got = labels(&para(&text));
        assert_eq!(got.len(), MAX_TAGS_PER_MESSAGE);
        // first sixteen in appearance order survive.
        assert_eq!(got[0], "tag00");
        assert_eq!(got[15], "tag15");
    }

    #[test]
    fn consecutive_and_adjacent_tags() {
        // `#a#b`: the second # is mid-run — only `a` tags.
        assert_eq!(labels(&para("#a#b")), ["a"]);
        assert_eq!(labels(&para("#a #b")), ["a", "b"]);
    }

    #[test]
    fn reversed_seq_keys_order_newest_first() {
        let older = tag_key("t", "g", 1);
        let newer = tag_key("t", "g", 2);
        assert!(newer < older, "higher seq must sort first");
    }
}
