use std::ops::Range;

/// A renderer-parity inline mark, plus the marker glyphs themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Marker,
    Bold,
    Italic,
    Strike,
    Code,
    Highlight,
    Link,
    /// `@name` — a member named in the prose.
    Mention,
}

/// The fences the inline grammar knows, longest first so `**` is never read
/// as two `*`. Each pairs with the mark its body wears.
const FENCES: &[(&str, Inline)] = &[
    ("**", Inline::Bold),
    ("__", Inline::Bold),
    ("~~", Inline::Strike),
    ("==", Inline::Highlight),
    ("`", Inline::Code),
    ("*", Inline::Italic),
    ("_", Inline::Italic),
];

/// The bytes that can open a mark. Ordinary prose is skipped in one scan
/// instead of retrying every delimiter at every character.
const OPENERS: &[char] = &['*', '_', '~', '=', '`', 'h', '@'];

/// Byte-ranged mirror of `chat::client::inline_spans`, minus its account
/// tokens: bare `http(s)://` runs, then the fences above, then `@name`
/// mentions; unmatched or empty fences stay plain. Ranges land on char
/// boundaries by construction — the scanner only advances through
/// `char_indices`.
pub fn inline_marks(line: &str) -> Vec<(Range<usize>, Inline)> {
    let mut marks = Vec::new();
    let mut at = 0;
    while at < line.len() {
        let rest = &line[at..];
        if !rest.starts_with(OPENERS) {
            let Some(next) = rest.find(OPENERS) else {
                break;
            };
            at += next;
            continue;
        }
        if let Some(len) = url_len(rest) {
            marks.push((at..at + len, Inline::Link));
            at += len;
            continue;
        }
        if let Some(len) = mention_len(line, at) {
            marks.push((at..at + len, Inline::Mention));
            at += len;
            continue;
        }
        let fence = FENCES
            .iter()
            .find_map(|(marker, kind)| fenced(rest, marker).map(|lens| (lens, *kind)));
        let Some(((marker_len, inner_len), kind)) = fence else {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let body = at + marker_len..at + marker_len + inner_len;
        marks.push((at..body.start, Inline::Marker));
        marks.push((body.clone(), kind));
        marks.push((body.end..body.end + marker_len, Inline::Marker));
        at = body.end + marker_len;
    }
    marks
}

/// Pages' named links retain source syntax as editable marker spans. The Chat
/// composer continues to use `inline_marks`, matching its own renderer grammar.
pub fn document_marks(line: &str) -> Vec<(Range<usize>, Inline)> {
    let mut marks = Vec::new();
    let mut start = 0;
    for (source, label, _) in named_links(line) {
        marks.extend(
            inline_marks(&line[start..source.start])
                .into_iter()
                .map(|(r, k)| (start + r.start..start + r.end, k)),
        );
        marks.push((source.start..label.start, Inline::Marker));
        marks.push((label.clone(), Inline::Link));
        marks.push((label.end..source.end, Inline::Marker));
        start = source.end;
    }
    marks.extend(
        inline_marks(&line[start..])
            .into_iter()
            .map(|(r, k)| (start + r.start..start + r.end, k)),
    );
    marks
}

pub fn document_link_at(line: &str, column: usize) -> Option<String> {
    if let Some((_, _, destination)) = named_links(line)
        .into_iter()
        .find(|(_, label, _)| label.contains(&column))
    {
        return Some(destination);
    }
    inline_marks(line).into_iter().find_map(|(range, kind)| {
        (kind == Inline::Link && range.contains(&column)).then(|| line[range].to_owned())
    })
}

/// The whole source of the named link whose label holds `column`, and the
/// label inside it — what "remove link" keeps.
pub fn named_link_at(line: &str, column: usize) -> Option<(Range<usize>, Range<usize>)> {
    named_links(line)
        .into_iter()
        .find(|(_, label, _)| label.contains(&column))
        .map(|(source, label, _)| (source, label))
}

/// Whether `column` sits anywhere in a named link's source — its label,
/// its destination or its brackets.
pub fn inside_named_link(line: &str, column: usize) -> bool {
    named_links(line)
        .into_iter()
        .any(|(source, _, _)| source.contains(&column))
}

/// CommonMark supplies source offsets and the decoded destination. Incomplete
/// syntax stays literal; escapes, URL parentheses and optional titles follow
/// the same parser as the app's Markdown readers.
fn named_links(line: &str) -> Vec<(Range<usize>, Range<usize>, String)> {
    use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
    let has_named_link = line.contains("](");
    if !has_named_link {
        return Vec::new();
    }
    let mut links = Vec::new();
    let mut parser = Parser::new(line).into_offset_iter();
    while let Some((event, source)) = parser.next() {
        let Event::Start(Tag::Link {
            link_type: LinkType::Inline,
            dest_url,
            ..
        }) = event
        else {
            continue;
        };
        let label_start = source.start + 1;
        let mut label_end = label_start;
        for (event, range) in parser.by_ref() {
            if event == Event::End(TagEnd::Link) {
                break;
            }
            label_end = label_end.max(range.end);
        }
        if label_start < label_end {
            links.push((source, label_start..label_end, dest_url.into_string()));
        }
    }
    links
}

/// If `rest` opens with `marker` and a later closing `marker` encloses a
/// non-empty body, `(marker byte length, body byte length)`.
fn fenced(rest: &str, marker: &str) -> Option<(usize, usize)> {
    let body = rest.strip_prefix(marker)?;
    let close = body.find(marker)?;
    if close == 0 {
        return None;
    }
    Some((marker.len(), close))
}

/// If `rest` opens a bare link, its byte length: the renderer's rule — an
/// `http(s)://` prefix, then everything up to whitespace.
fn url_len(rest: &str) -> Option<usize> {
    let starts_link = rest.starts_with("http://") || rest.starts_with("https://");
    if !starts_link {
        return None;
    }
    let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(len)
}

/// A character a mention handle is spelled with — the chat composer's rule.
pub fn handle_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// If `at` opens a mention — an `@` at a word start followed by a handle —
/// its byte length. An `@` inside a word (an email address) is prose.
fn mention_len(line: &str, at: usize) -> Option<usize> {
    let rest = line[at..].strip_prefix('@')?;
    let mid_word = line[..at]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    if mid_word {
        return None;
    }
    let handle = rest.find(|c| !handle_char(c)).unwrap_or(rest.len());
    (handle > 0).then_some(1 + handle)
}
