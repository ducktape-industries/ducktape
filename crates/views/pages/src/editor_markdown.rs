//! The page document's markdown highlighter.
//!
//! Modelled on `ducktape-ui/examples/markdown-editor`: one `RichTextEditor`
//! over the whole document, and the SYNTAX ITSELF carries the formatting. A
//! line that starts `## ` is a Heading 2 because those three bytes are there,
//! not because a menu was opened — and the three bytes NEVER paint, caret or
//! no caret, which is what makes the surface read as Notion's blocks instead
//! of as a source view. The shape is edited through what the syntax means:
//! Backspace at a block's edge drops its prefix, a format toggle wraps or
//! unwraps its run, and the host draws the block's own furniture (the
//! checkbox, the bullet, the quote bar, the code plate) from the line's prefix.
//!
//! Inline emphasis uses the Chat grammar; Pages additionally conceals named
//! link syntax, without rewriting the source.
//! There are no language tokens inside
//! fences. `PageBlock` carries no language field, so a fence body is one mono
//! plate.
//!
//! The line metrics are the Pages design tokens
//! (H1 20/1.25, H2 16/1.3, H3 14/1.35, body 14/1.5, quote 14/1.6, code 12/1.6,
//! callout 13/1.6), so a saved document reads at exactly the size it was typed.

use ducktape_view_guest::wire::editor_presentation::EditorFormat as Format;
use ducktape_view_guest::wire::{
    self, Edges as Padding, FontStyle, LineHeight, NamedFont as Font, Rgba as Color, Weight,
};
use std::ops::Range;
const TRANSPARENT: Color = Color([0.0; 4]);

use super::inline::{Inline, document_marks};

pub const BODY_SIZE: f32 = 14.0;
pub const BODY_LINE_HEIGHT: f32 = 1.5;
/// Line 0 is the page title. It reads a step above H1 so the document opens on
/// an obvious title, and it is the ONLY line whose shape is positional rather
/// than declared by a prefix.
const TITLE_SIZE: f32 = 22.0;
const TITLE_LINE_HEIGHT: f32 = 1.15;
const HEADING_SIZE: [f32; 3] = [20.0, 16.0, 14.0];
const HEADING_LINE_HEIGHT: [f32; 3] = [1.25, 1.3, 1.35];
const QUOTE_LINE_HEIGHT: f32 = 1.6;
const CALLOUT_SIZE: f32 = 13.0;
const CALLOUT_LINE_HEIGHT: f32 = 1.6;
const CODE_SIZE: f32 = 12.0;
const CODE_LINE_HEIGHT: f32 = 1.6;
const CODE_PLATE_PAD: f32 = 15.0;
/// A hidden marker cannot be `size 0` — a zero-metric span drops out of the
/// shaped run and takes the caret's column with it. The host drops any run
/// under one pixel from the displayed text, and maps the caret around it.
const HIDDEN_SIZE: f32 = 0.01;
/// One nesting step, as left padding. The two spaces the depth is SPELLED
/// with measure ~8px in the body face — legible as "something is different",
/// useless as "this item belongs to that one".
const NEST_STEP: f32 = 22.0;
/// The breathing room above and below one block. Every line is a block, and a
/// block is more than its glyph row: the reference (Notion) insets each by
/// `padding:3px 0;margin:1px 0`, so consecutive paragraphs sit ~8px apart
/// rather than line-height apart. Code lines take none — the plate must stay
/// continuous — and a callout already carries its own tile inset.
const BLOCK_PAD: f32 = 4.0;
/// A heading's own room, `[above, below]` per level. A heading opens a
/// section, so it stands off the block above it by about its own line and
/// hugs the block it introduces (the reference: H1 `margin-top:2em`, H2
/// `1.4em`, H3 `1em`, all `padding-bottom:3px`).
const HEADING_PAD: [[f32; 2]; 3] = [[24.0, 8.0], [18.0, 6.0], [14.0, 4.0]];

/// Which palette is being painted, and which lines carry a comment thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caret {
    pub dark: bool,
    /// Document lines inside a block carrying an unresolved comment thread —
    /// they wear a quiet wash so the anchor is visible in the document.
    pub commented: Vec<i64>,
}

/// One painted run. `Marker` is the markdown syntax itself — never seen, it
/// collapses out of the displayed line; every other variant is content
/// wearing the shape that syntax declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    Title,
    Marker(Style),
    Body(Style),
    /// An ordered item's `1. ` — the one marker the reader sees, because the
    /// number IS the content the host cannot draw for it.
    ListMarker(Style),
    /// A line's leading whitespace. Collapsed to nothing — the nesting step
    /// it stands for is painted as the line's left padding instead.
    Indent(Style),
    Fence,
    CodeBody,
}

/// Where a line sits in the column: the `-> ` (center) and `->> ` (end)
/// markers right after the block prefix, the markdown-it-center-text
/// convention. The marker is scaffolding — hidden like every other marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

impl Align {
    pub fn marker(self) -> &'static str {
        match self {
            Align::Start => "",
            Align::Center => "-> ",
            Align::End => "->> ",
        }
    }
}

/// The alignment a line's content opens with, and the marker's byte length.
pub fn align_marker(content: &str) -> (Align, usize) {
    for align in [Align::End, Align::Center] {
        if content.starts_with(align.marker()) {
            return (align, align.marker().len());
        }
    }
    (Align::Start, 0)
}

/// The byte offset a line's content starts at, past its block prefix.
pub fn content_start(line: &str) -> usize {
    prefix_of(line).1
}

/// The shape a line's prefix declared, plus the inline marks inside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub align: Align,
    pub heading: Option<u8>,
    pub quote: bool,
    pub callout: bool,
    pub divider: bool,
    pub strong: bool,
    pub emphasis: bool,
    pub link: bool,
    /// `~~struck~~`, `++underlined++`, `` `code` ``, `==marked==`, `@name`.
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub highlight: bool,
    pub mention: bool,
    /// A colour span's packed `0xRRGGBB`: the ink it asks for over any other.
    pub color: Option<u32>,
    pub commented: bool,
    /// A checked todo's content — muted and struck through.
    pub done: bool,
    /// The line's nesting depth. It is drawn as LEFT PADDING rather than as
    /// the two literal spaces it is spelled with — a space of the body face
    /// is ~4px, which reads as no nesting at all.
    pub indent: u8,
}

/// The block shape a line's leading bytes declare. `Body` is the absence of a
/// prefix, which is why it has no marker range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
    /// `# ` / `## ` / `### ` — hidden, and the body takes the heading metrics.
    Heading(u8),
    /// `- `, `1. `, `- [ ] ` — the number stays visible (it IS the content);
    /// a bullet or a box is the host's to draw, so those markers hide.
    List,
    /// `> ` — hidden; the host draws the bar.
    Quote,
    /// `!> ` — hidden. Callout has no CommonMark spelling and the block kind
    /// predates this surface, so it gets an extension rather than being
    /// silently rewritten into a quote on the first save.
    Callout,
    /// `---` on its own line.
    Divider,
    Body,
}

/// The prefix a line declares, and the byte offset its content starts at.
fn prefix_of(line: &str) -> (Prefix, usize) {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let indent = line.len() - trimmed.len();

    if trimmed.trim_end() == "---" {
        return (Prefix::Divider, line.len());
    }
    if let Some(rest) = trimmed.strip_prefix("!> ") {
        return (Prefix::Callout, line.len() - rest.len());
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return (Prefix::Quote, line.len() - rest.len());
    }
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    let heading_marked = (1..=3).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ');
    if heading_marked {
        let content = indent + hashes + 1;
        return (Prefix::Heading(hashes as u8), content);
    }
    match list_content(trimmed) {
        Some(offset) => (Prefix::List, indent + offset),
        None => (Prefix::Body, indent),
    }
}

/// The byte offset a list line's CONTENT starts at, marker and tick included.
/// `- `, `* `, `+ `, `1. `, `1) `, and any of those followed by `[ ]`/`[x]`.
fn list_content(trimmed: &str) -> Option<usize> {
    let bytes = trimmed.as_bytes();
    let mut cursor = match *bytes.first()? {
        b'-' | b'+' | b'*' => 1,
        byte if byte.is_ascii_digit() => {
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let delimited = matches!(bytes.get(digits), Some(b'.' | b')'));
            if !delimited {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    if bytes.get(cursor) != Some(&b' ') {
        return None;
    }
    cursor += 1;
    let ticked = matches!(
        bytes.get(cursor..cursor + 4),
        Some(b"[ ] " | b"[x] " | b"[X] ")
    );
    if ticked {
        cursor += 4;
    }
    Some(cursor)
}

/// True when the line opens or closes a fenced code block.
fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    trimmed.starts_with("```")
}

/// The document highlighter. `fences` is the per-line "am I inside code?"
/// carry — the one piece of cross-line state, kept as a vector so the widget's
/// incremental relayout can resume at any line instead of rescanning.
#[derive(Debug)]
pub struct DocumentHighlighter {
    current_line: usize,
    fences: Vec<bool>,
    caret: Caret,
}

impl DocumentHighlighter {
    pub fn new(caret: &Caret) -> Self {
        Self {
            current_line: 0,
            fences: vec![false],
            caret: caret.clone(),
        }
    }

    pub fn update(&mut self, caret: &Caret) {
        // A palette flip or a thread landing restyles every line; nothing
        // else the caret does changes how a line paints.
        if self.caret == *caret {
            return;
        }
        self.caret = caret.clone();
        self.fences.truncate(1);
        self.current_line = 0;
    }

    pub fn change_line(&mut self, line: usize) {
        if line >= self.fences.len() {
            self.fences.truncate(1);
            self.current_line = 0;
            return;
        }
        self.fences.truncate(line + 1);
        self.current_line = line;
    }

    pub fn highlight_line(&mut self, line: &str) -> std::vec::IntoIter<(Range<usize>, Mark)> {
        let index = self.current_line;
        let inside_code = self.fences[index];
        let commented = self.caret.commented.contains(&(index as i64));
        let (marks, next_inside) = match index == 0 {
            true => (vec![(0..line.len(), Mark::Title)], false),
            false => highlight(line, inside_code, commented),
        };

        self.current_line += 1;
        match self.fences.len() == self.current_line {
            true => self.fences.push(next_inside),
            false => self.fences[self.current_line] = next_inside,
        }
        marks.into_iter()
    }

    pub fn current_line(&self) -> usize {
        self.current_line
    }
}

/// One line's runs, and whether the NEXT line is inside a code fence.
fn highlight(line: &str, inside_code: bool, commented: bool) -> (Vec<(Range<usize>, Mark)>, bool) {
    if is_fence(line) {
        return (vec![(0..line.len(), Mark::Fence)], !inside_code);
    }
    if inside_code {
        return (vec![(0..line.len(), Mark::CodeBody)], true);
    }

    let (prefix, content) = prefix_of(line);
    let marker = &line[..content];
    let ticked_done =
        prefix == Prefix::List && (marker.ends_with("[x] ") || marker.ends_with("[X] "));
    let (align, align_len) = align_marker(&line[content..]);
    let style = Style {
        align,
        heading: match prefix {
            Prefix::Heading(level) => Some(level),
            _ => None,
        },
        quote: prefix == Prefix::Quote,
        callout: prefix == Prefix::Callout,
        divider: prefix == Prefix::Divider,
        commented,
        done: ticked_done,
        indent: super::indent::split_indent(line).0.min(u8::MAX.into()) as u8,
        ..Style::default()
    };

    let mut marks = Vec::new();
    // The leading indent is LAYOUT, not syntax. It gets its own run so that no
    // marker run starts at column 0 — a COLLAPSED marker that did swallowed
    // the indent with it, and the nested item drew flush with its parent.
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    if indent > 0 && prefix != Prefix::Divider {
        marks.push((0..indent, Mark::Indent(style)));
    }
    // The marker run. An ordered item keeps its number — the host draws a
    // bullet, a checkbox, a quote bar or a rule for every other prefix, so
    // those collapse like any syntax.
    let ordered = prefix == Prefix::List
        && marker
            .trim_start()
            .starts_with(|c: char| c.is_ascii_digit());
    match prefix {
        Prefix::Body => {}
        Prefix::List if ordered => marks.push((indent..content, Mark::ListMarker(style))),
        Prefix::Divider => marks.push((0..line.len(), Mark::Marker(style))),
        Prefix::List | Prefix::Heading(_) | Prefix::Quote | Prefix::Callout => {
            marks.push((indent..content, Mark::Marker(style)))
        }
    }

    if prefix == Prefix::Divider {
        return (marks, false);
    }

    // The line's own body carries the block style; the inline scanner then
    // overlays bold/italic/link on top of it, offset past the prefix.
    marks.push((content..line.len(), Mark::Body(style)));
    // The alignment marker is scaffolding like a fence: it collapses, and
    // the body it aligns keeps the line's height.
    if align_len > 0 {
        marks.push((content..content + align_len, Mark::Marker(style)));
    }
    let body = content + align_len;
    for (range, inline) in document_marks(&line[body..]) {
        let shifted = body + range.start..body + range.end;
        let inline_style = match inline {
            Inline::Marker => {
                marks.push((shifted, Mark::Marker(style)));
                continue;
            }
            Inline::Bold => Style {
                strong: true,
                ..style
            },
            Inline::Italic => Style {
                emphasis: true,
                ..style
            },
            Inline::Link => Style {
                link: true,
                ..style
            },
            Inline::Strike => Style {
                strike: true,
                ..style
            },
            Inline::Underline => Style {
                underline: true,
                ..style
            },
            Inline::Code => Style {
                code: true,
                ..style
            },
            Inline::Highlight => Style {
                highlight: true,
                ..style
            },
            Inline::Mention => Style {
                mention: true,
                ..style
            },
            Inline::Color(rgb) => Style {
                color: Some(rgb),
                ..style
            },
        };
        marks.push((shifted, Mark::Body(inline_style)));
    }
    (marks, false)
}

/// The document ink, one set per light/dark palette.
struct Ink {
    muted: Color,
    marker: Color,
    link: Color,
    code_ink: Color,
    code_plate: Color,
    code_line: Color,
    callout_plate: Color,
    callout_line: Color,
    comment_wash: Color,
    /// The `==marked==` wash: a highlighter pen over the prose.
    mark_plate: Color,
    rule: Color,
}

/// The document ink is the shared palette, so the page reads like the rest
/// of the app in both appearances: neutral greys for scaffolding, the one
/// accent only where a block is chosen (a ticked todo, a commented line).
fn ink(dark: bool) -> Ink {
    let p = design::palette(dark);
    Ink {
        muted: Color(p.muted),
        marker: Color(p.faint),
        link: Color(p.link),
        // A quiet raised plate inside a hairline, never a dark slab: a slab
        // swallows its own ink in dark.
        code_ink: Color(p.foreground),
        code_plate: Color(p.surface_raised),
        code_line: Color(p.border),
        callout_plate: Color(p.surface),
        callout_line: Color(p.border_strong),
        comment_wash: Color(p.accent_soft),
        mark_plate: Color(match dark {
            true => [0.91, 0.77, 0.29, 0.30],
            false => [0.91, 0.77, 0.29, 0.38],
        }),
        rule: Color(p.border_strong),
    }
}

/// A colour span's packed `0xRRGGBB` as opaque ink.
fn hex_color(rgb: u32) -> Color {
    let channel = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
    Color([channel(16), channel(8), channel(0), 1.0])
}

fn body_font(weight: Weight, style: FontStyle) -> Font {
    Font {
        family: wire::FontFamily::Named(design::fonts::FAMILY_UI.into()),
        weight,
        stretch: wire::FontStretch::Normal,
        style,
    }
}

fn code_font() -> Font {
    Font {
        family: wire::FontFamily::Named(design::fonts::FAMILY_MONO.into()),
        ..body_font(Weight::Normal, FontStyle::Normal)
    }
}

fn border(color: Color, width: f32, radius: f32) -> wire::Border {
    wire::Border {
        color: Some(color),
        width: Some(width),
        radius: Some([radius; 4]),
    }
}

/// The span plate an inline mark wears: the code wash inside its hairline for
/// `` `code` ``, the highlighter pen for `==marked==`, nothing otherwise.
fn inline_plate(style: Style, ink: &Ink) -> (Option<Color>, Option<wire::Border>) {
    if style.code {
        return (Some(ink.code_plate), Some(border(ink.code_line, 1.0, 4.0)));
    }
    if style.highlight {
        return (Some(ink.mark_plate), Some(border(TRANSPARENT, 0.0, 3.0)));
    }
    (None, None)
}

/// The paint for one run. This is the whole visual contract of the surface.
pub fn format(mark: &Mark, dark: bool) -> Format {
    let mut format = paint(mark, dark);
    // Depth is padding, not glyphs. Every run of the line asks for the same
    // inset: the layout takes it from the LAST run that wants one, and a code
    // plate or callout tile further along would otherwise drop it.
    format.line_padding.left += nest(mark);
    let [above, below] = block_pad(mark);
    format.line_padding.top += above;
    format.line_padding.bottom += below;
    format.line_align = line_align(mark);
    format
}

/// Where the run's line sits in the column, when its content asked for a
/// side. Every run of the line answers the same, so whichever the layout
/// reads last still carries it.
fn line_align(mark: &Mark) -> Option<wire::Align> {
    let style = match *mark {
        Mark::Indent(style) | Mark::Body(style) | Mark::ListMarker(style) | Mark::Marker(style) => {
            style
        }
        Mark::Title | Mark::Fence | Mark::CodeBody => return None,
    };
    match style.align {
        Align::Start => None,
        Align::Center => Some(wire::Align::Center),
        Align::End => Some(wire::Align::End),
    }
}

/// The vertical inset a run's line is owed, `[above, below]`, on top of
/// whatever its paint set.
fn block_pad(mark: &Mark) -> [f32; 2] {
    let style = match *mark {
        Mark::Indent(style) | Mark::Body(style) | Mark::ListMarker(style) | Mark::Marker(style) => {
            style
        }
        Mark::Title => return [BLOCK_PAD; 2],
        Mark::Fence | Mark::CodeBody => return [0.0; 2],
    };
    if let Some(level) = style.heading {
        return HEADING_PAD[usize::from(level).saturating_sub(1).min(2)];
    }
    match style.callout {
        true => [0.0; 2],
        false => [BLOCK_PAD; 2],
    }
}

/// The nesting inset a run's line is owed.
fn nest(mark: &Mark) -> f32 {
    let style = match *mark {
        Mark::Indent(style) | Mark::Body(style) | Mark::ListMarker(style) | Mark::Marker(style) => {
            style
        }
        // The title is line 0 and a fence keeps the indentation it is written
        // with — neither nests.
        Mark::Title | Mark::Fence | Mark::CodeBody => return 0.0,
    };
    f32::from(style.indent) * NEST_STEP
}

fn paint(mark: &Mark, dark: bool) -> Format {
    let ink = &ink(dark);
    match *mark {
        Mark::Title => Format {
            font: Some(body_font(Weight::Semibold, FontStyle::Normal)),
            size: Some(TITLE_SIZE),
            line_height: Some(LineHeight::Absolute(TITLE_SIZE * TITLE_LINE_HEIGHT)),
            ..Format::default()
        },
        Mark::Marker(style) => {
            let mut format = body_format(style, ink);
            // The marker is scaffolding: a done todo strikes its CONTENT, never
            // its bullet — a struck collapsed bullet paints a floating dash.
            format.strikethrough = None;
            format.color = Some(TRANSPARENT);
            if style.divider {
                // A divider IS the rule: the glyphs go transparent at full
                // size — the line keeps its height and its click target —
                // and the hairline paints across the column.
                format.line_rule = Some(ink.rule);
                return format;
            }
            // Collapse the glyphs without collapsing the LINE: the body run
            // beside them still carries the real line height.
            format.size = Some(HIDDEN_SIZE);
            format.line_height = None;
            format
        }
        Mark::ListMarker(style) => Format {
            color: Some(ink.muted),
            ..body_format(style, ink)
        },
        Mark::Indent(style) => {
            let mut format = body_format(style, ink);
            format.strikethrough = None;
            format.color = Some(TRANSPARENT);
            format.size = Some(HIDDEN_SIZE);
            // A line that is NOTHING but indent has no other run to carry its
            // height, so this one holds the body metrics absolutely.
            format.line_height = Some(LineHeight::Absolute(BODY_SIZE * BODY_LINE_HEIGHT));
            format
        }
        Mark::Body(style) => body_format(style, ink),
        // The fence row is the plate's own vertical inset: its glyphs
        // collapse and the row keeps a pad's height.
        Mark::Fence => Format {
            color: Some(TRANSPARENT),
            font: Some(code_font()),
            size: Some(HIDDEN_SIZE),
            line_height: Some(LineHeight::Absolute(CODE_PLATE_PAD)),
            line_background: Some(ink.code_plate),
            line_border: Some(border(ink.code_line, 1.0, 10.0)),
            line_padding: Padding {
                left: CODE_PLATE_PAD,
                right: CODE_PLATE_PAD,
                ..Padding::default()
            },
            ..Format::default()
        },
        Mark::CodeBody => Format {
            color: Some(ink.code_ink),
            font: Some(code_font()),
            size: Some(CODE_SIZE),
            line_height: Some(LineHeight::Absolute(CODE_SIZE * CODE_LINE_HEIGHT)),
            line_background: Some(ink.code_plate),
            line_border: Some(border(ink.code_line, 1.0, 10.0)),
            line_padding: Padding {
                left: CODE_PLATE_PAD,
                right: CODE_PLATE_PAD,
                ..Padding::default()
            },
            ..Format::default()
        },
    }
}

fn body_format(style: Style, ink: &Ink) -> Format {
    let weight = match (style.strong || style.heading.is_some(), style.mention) {
        (true, _) => Weight::Semibold,
        (false, true) => Weight::Medium,
        (false, false) => Weight::Normal,
    };
    let italic = match style.emphasis {
        true => FontStyle::Italic,
        false => FontStyle::Normal,
    };
    let linked = style.link || style.mention;
    // A quote is body ink behind its bar (the host's), not a muted aside.
    let muted = style.done || style.divider;
    let color = match (style.color, linked, muted) {
        (Some(rgb), _, _) => Some(hex_color(rgb)),
        (None, true, _) => Some(ink.link),
        (None, false, true) => Some(ink.muted),
        (None, false, false) => None,
    };
    let font = match style.code {
        true => code_font(),
        false => body_font(weight, italic),
    };
    // A done todo strikes in the marker grey; a `~~struck~~` run strikes in
    // its own ink, which for a plain body is the host's foreground.
    let struck = match (style.done, style.strike) {
        (true, _) => Some(ink.marker),
        (false, true) => Some(color.unwrap_or(ink.code_ink)),
        (false, false) => None,
    };
    let (background, plate_border) = inline_plate(style, ink);
    // A `++run++` underlines in its own ink, like a strike does.
    let underlined = style.underline.then(|| color.unwrap_or(ink.code_ink));
    let mut format = Format {
        color,
        font: Some(font),
        strikethrough: struck,
        underline: underlined,
        background,
        border: plate_border,
        ..Format::default()
    };
    // A commented block wears a quiet brand wash across its lines — the
    // in-document anchor for the rail's threads. The callout keeps its own
    // tile (the stronger plate already marks the line).
    if style.commented && !style.callout {
        format.line_background = Some(ink.comment_wash);
        format.line_border = Some(border(TRANSPARENT, 0.0, 6.0));
    }
    if let Some(level) = style.heading {
        let step = usize::from(level).saturating_sub(1).min(2);
        let size = HEADING_SIZE[step];
        format.size = Some(size);
        format.line_height = Some(LineHeight::Absolute(size * HEADING_LINE_HEIGHT[step]));
        return format;
    }
    if style.callout {
        format.size = Some(CALLOUT_SIZE);
        format.line_height = Some(LineHeight::Absolute(CALLOUT_SIZE * CALLOUT_LINE_HEIGHT));
        format.line_background = Some(ink.callout_plate);
        format.line_border = Some(border(ink.callout_line, 1.0, 11.0));
        format.line_padding = Padding {
            top: 9.0,
            bottom: 9.0,
            left: 14.0,
            right: 14.0,
        };
        return format;
    }
    if style.quote {
        format.line_height = Some(LineHeight::Absolute(BODY_SIZE * QUOTE_LINE_HEIGHT));
    }
    // A body run says its size outright. The host sizes a line by the LAST
    // run that named one, and a run with no size of its own paints at that:
    // a ticked todo's second bracket (7px, squeezed beside the wider `x`)
    // came right before the body and shrank the whole sentence to it.
    format.size = Some(BODY_SIZE);
    format
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_run_names_its_size_so_a_collapsed_marker_cannot_shrink_it() {
        let (marks, _) = highlight("- [x] done", false, false);
        let marker = format(&marks[0].1, false);
        assert_eq!(marker.size, Some(HIDDEN_SIZE));
        assert!(matches!(marks[1].1, Mark::Body(_)));
        assert_eq!(format(&marks[1].1, false).size, Some(BODY_SIZE));
        let (plain, _) = highlight("plain words", false, false);
        assert_eq!(format(&plain[0].1, false).size, Some(BODY_SIZE));
    }

    #[test]
    fn ordinary_text_inherits_native_ink_without_erasing_semantic_marks() {
        for dark in [false, true] {
            for mark in [
                Mark::Title,
                Mark::Body(Style::default()),
                Mark::Body(Style {
                    strong: true,
                    ..Style::default()
                }),
                Mark::Body(Style {
                    heading: Some(1),
                    ..Style::default()
                }),
            ] {
                assert_eq!(format(&mark, dark).color, None);
            }
            assert!(
                format(
                    &Mark::Body(Style {
                        link: true,
                        ..Style::default()
                    }),
                    dark
                )
                .color
                .is_some()
            );
            assert!(
                format(
                    &Mark::Body(Style {
                        commented: true,
                        ..Style::default()
                    }),
                    dark
                )
                .line_background
                .is_some()
            );
            let code = format(&Mark::CodeBody, dark);
            assert!(code.color.is_some());
            assert!(code.line_background.is_some());
        }
    }

    fn shapes(line: &str) -> (Prefix, usize) {
        prefix_of(line)
    }

    #[test]
    fn prefixes_map_to_the_block_kinds_the_module_stores() {
        assert_eq!(shapes("# Title").0, Prefix::Heading(1));
        assert_eq!(shapes("### Small").0, Prefix::Heading(3));
        assert_eq!(shapes("- item").0, Prefix::List);
        assert_eq!(shapes("1. item").0, Prefix::List);
        assert_eq!(shapes("- [ ] todo").0, Prefix::List);
        assert_eq!(shapes("> quoted").0, Prefix::Quote);
        assert_eq!(shapes("!> noted").0, Prefix::Callout);
        assert_eq!(shapes("---").0, Prefix::Divider);
        assert_eq!(shapes("plain").0, Prefix::Body);
    }

    #[test]
    fn a_fourth_hash_is_prose_because_the_module_stops_at_heading_3() {
        assert_eq!(shapes("#### four").0, Prefix::Body);
        // ...and a bare `#` with no space is a tag, not a heading.
        assert_eq!(shapes("#tag").0, Prefix::Body);
    }

    #[test]
    fn content_offsets_skip_the_marker_but_keep_list_bullets() {
        assert_eq!(shapes("## Two"), (Prefix::Heading(2), 3));
        assert_eq!(shapes("> q"), (Prefix::Quote, 2));
        assert_eq!(shapes("!> c"), (Prefix::Callout, 3));
        // The list content starts PAST the tick, so `[x] ` never reads as prose.
        assert_eq!(shapes("- [x] done"), (Prefix::List, 6));
        assert_eq!(shapes("12) twelve"), (Prefix::List, 4));
    }

    #[test]
    fn indented_lines_keep_their_indent_in_the_content_offset() {
        assert_eq!(shapes("    - nested"), (Prefix::List, 6));
        assert_eq!(shapes("  ## deep"), (Prefix::Heading(2), 5));
    }

    #[test]
    fn a_fence_toggles_the_carry_and_its_body_is_never_reparsed() {
        let (marks, inside) = highlight("```", false, false);
        assert!(inside);
        assert!(matches!(marks[0].1, Mark::Fence));
        // `# ` inside a fence is code, not a heading.
        let (body, still_inside) = highlight("# not a heading", true, false);
        assert!(still_inside);
        assert_eq!(body, vec![(0..15, Mark::CodeBody)]);
        let (_, closed) = highlight("```", true, false);
        assert!(!closed);
    }

    #[test]
    /// Notion never shows its syntax: the `## ` collapses whether or not the
    /// caret sits on the line, and the body beside it carries the heading.
    fn a_marker_never_paints_and_its_body_carries_the_line() {
        let (marks, _) = highlight("## Heading", false, false);
        assert!(matches!(marks[0].1, Mark::Marker(_)));
        let marker = format(&marks[0].1, false);
        assert_eq!(marker.color, Some(TRANSPARENT));
        assert_eq!(marker.size, Some(HIDDEN_SIZE));
        assert!(marker.line_height.is_none());
        assert!(matches!(marks[1].1, Mark::Body(style) if style.heading == Some(2)));
        assert!(format(&marks[1].1, false).line_height.is_some());
    }

    #[test]
    fn inline_marks_ride_on_top_of_the_block_style_at_the_right_offsets() {
        let (marks, _) = highlight("## say **hi**", false, false);
        let bold = marks
            .iter()
            .find(|(_, mark)| matches!(mark, Mark::Body(style) if style.strong))
            .expect("a bold run");
        // "## say **hi**" — the bold body is `hi` at bytes 9..11.
        assert_eq!(bold.0, 9..11);
        let Mark::Body(style) = bold.1 else {
            unreachable!("matched a body run")
        };
        // ...and it is STILL a heading, so it keeps the heading metrics.
        assert_eq!(style.heading, Some(2));
    }

    #[test]
    fn line_zero_is_the_title_and_nothing_in_it_is_markdown() {
        let mut highlighter = DocumentHighlighter::new(&Caret {
            dark: false,
            commented: Vec::new(),
        });
        let title: Vec<_> = highlighter.highlight_line("# still the title").collect();
        assert_eq!(title, vec![(0..17, Mark::Title)]);
        // ...and line 1 parses normally, so the title costs the body nothing.
        let body: Vec<_> = highlighter.highlight_line("# a real heading").collect();
        assert!(matches!(body[0].1, Mark::Marker { .. }));
    }

    #[test]
    fn named_link_label_keeps_its_destination_when_syntax_is_hidden() {
        let source = "[문서](duck://pages/alpha)";
        let (marks, _) = highlight(source, false, false);
        let (label, _) = marks
            .iter()
            .find(|(_, mark)| matches!(mark, Mark::Body(style) if style.link))
            .unwrap();
        assert_eq!(&source[label.clone()], "문서");
        assert_eq!(
            super::super::inline::document_link_at(source, label.start).as_deref(),
            Some("duck://pages/alpha")
        );
        let hidden_markers = marks
            .iter()
            .filter(|(_, mark)| matches!(mark, Mark::Marker(_)))
            .count();
        assert_eq!(hidden_markers, 2);
    }

    #[test]
    fn a_divider_is_a_rule_not_three_dashes() {
        let (marks, _) = highlight("---", false, false);
        assert!(matches!(marks[0].1, Mark::Marker(_)));
        let painted = format(&marks[0].1, false);
        assert_eq!(painted.color, Some(TRANSPARENT));
        assert!(painted.line_rule.is_some());
        // Full size: the line keeps its height and its click target.
        assert_eq!(painted.size, Some(BODY_SIZE));
    }

    /// The whole `- [ ] ` is one collapsed run: the checkbox is the host's
    /// widget, drawn from the prefix, never glyphs dressed up as a box. A
    /// bullet collapses the same way; an ordered item keeps its number.
    #[test]
    fn list_prefixes_collapse_except_the_number_the_host_cannot_draw() {
        let (todo, _) = highlight("- [ ] ship", false, false);
        assert_eq!(todo[0].0, 0..6);
        assert!(matches!(todo[0].1, Mark::Marker(_)));
        assert_eq!(todo[1].0, 6..10);
        assert!(matches!(todo[1].1, Mark::Body(_)));
        let (bullet, _) = highlight("- plain", false, false);
        assert_eq!(bullet[0].0, 0..2);
        assert!(matches!(bullet[0].1, Mark::Marker(_)));
        let (ordered, _) = highlight("12. twelve", false, false);
        assert_eq!(ordered[0].0, 0..4);
        assert!(matches!(ordered[0].1, Mark::ListMarker(_)));
        assert_ne!(format(&ordered[0].1, false).color, Some(TRANSPARENT));
    }

    #[test]
    fn a_done_todo_strikes_its_text_but_never_its_collapsed_prefix() {
        let (marks, _) = highlight("- [x] done", false, false);
        assert!(format(&marks[0].1, false).strikethrough.is_none());
        let Mark::Body(style) = marks[1].1 else {
            unreachable!("a body run")
        };
        assert!(style.done);
        let body = format(&marks[1].1, false);
        assert!(body.strikethrough.is_some());
        assert_eq!(body.color, Some(ink(false).muted));
    }

    /// Notion's quote is body ink behind a bar, not a muted aside: the bar is
    /// the host's, so the guest paints the words as words.
    #[test]
    fn a_quote_reads_as_body_ink_behind_the_hosts_bar() {
        let (marks, _) = highlight("> words", false, false);
        assert!(matches!(marks[0].1, Mark::Marker(_)));
        let body = format(&marks[1].1, false);
        assert_eq!(body.color, None);
        assert!(matches!(
            body.font,
            Some(Font {
                style: FontStyle::Normal,
                ..
            })
        ));
    }

    /// The nesting step is drawn as the line's left padding, not as the two
    /// spaces it is spelled with — those are ~8px and read as no nesting. The
    /// spaces themselves collapse so the step is exactly one `NEST_STEP`.
    #[test]
    fn a_nested_line_is_inset_by_padding_and_its_indent_glyphs_collapse() {
        let inset = |marks: &[(Range<usize>, Mark)], steps: f32| {
            let indent = marks
                .iter()
                .find(|(range, _)| *range == (0..2))
                .expect("an indent run");
            let format = format(&indent.1, false);
            assert_eq!(format.size, Some(HIDDEN_SIZE), "{marks:?}");
            assert_eq!(format.line_padding.left, steps * NEST_STEP, "{marks:?}");
        };

        let (todo, _) = highlight("  - [ ] nested", false, false);
        inset(&todo, 1.0);
        assert!(matches!(todo[0].1, Mark::Indent(_)));
        assert_eq!(todo[1].0, 2..8);
        assert!(matches!(todo[1].1, Mark::Marker(_)));

        // Every run of the line asks for the same inset, so a later run with
        // its own padding cannot drop it.
        let (heading, _) = highlight("  ## nested", false, false);
        inset(&heading, 1.0);
        assert_eq!(heading[1].0, 2..5);
        for (_, mark) in &heading {
            assert_eq!(format(mark, false).line_padding.left, NEST_STEP);
        }

        let (bullet, _) = highlight("    - deeper", false, false);
        assert!(matches!(bullet[0].1, Mark::Indent(_)));
        assert_eq!(
            format(&bullet[0].1, false).line_padding.left,
            2.0 * NEST_STEP
        );
        assert_eq!(bullet[1].0, 4..6);
        assert!(matches!(bullet[1].1, Mark::Marker(_)));

        // A line at the margin asks for nothing and gets no indent run.
        let (flat, _) = highlight("- flat", false, false);
        assert_eq!(flat[0].0, 0..2);
        assert_eq!(format(&flat[0].1, false).line_padding.left, 0.0);
    }

    #[test]
    fn a_hidden_marker_keeps_a_measurable_size() {
        let hidden = format(&Mark::Marker(Style::default()), false);
        assert_eq!(hidden.color, Some(TRANSPARENT));
        assert!(hidden.size.expect("a size") > 0.0);
    }
}

#[cfg(test)]
mod plate_probe {
    use super::*;

    #[test]
    fn the_code_body_line_keeps_full_size_ink() {
        let mut hl = DocumentHighlighter::new(&Caret {
            dark: false,
            commented: Vec::new(),
        });
        let lines = [
            "Team Runbook v2",
            "How we ship: branch off dev, PR, review, merge.",
            "## Rules",
            "- ship small",
            "- test everything",
            "- [ ] wire QA",
            "```",
            "cargo test -p ducktape-app",
            "```",
        ];
        let mut all = Vec::new();
        for line in lines {
            all.push(hl.highlight_line(line).collect::<Vec<_>>());
        }
        let cargo = &all[7];
        assert_eq!(cargo.len(), 1, "{cargo:?}");
        assert!(matches!(cargo[0].1, Mark::CodeBody), "{cargo:?}");
        let f = format(&cargo[0].1, false);
        assert_eq!(f.size, Some(CODE_SIZE));
        assert!(f.color.expect("ink").0[3] > 0.9, "opaque ink");
        assert!(f.line_background.is_some());
    }

    /// A block is inset above and below so paragraphs sit apart, not
    /// line-height apart; code lines take none so the plate stays continuous,
    /// and a callout keeps its own tile inset.
    #[test]
    fn every_block_but_code_and_callout_wears_the_block_gap() {
        let (body, _) = highlight("a paragraph", false, false);
        let f = format(&body[0].1, false);
        assert_eq!(
            (f.line_padding.top, f.line_padding.bottom),
            (BLOCK_PAD, BLOCK_PAD)
        );

        let (code, _) = highlight("let x = 1;", true, false);
        assert_eq!(
            {
                let pad = format(&code[0].1, false).line_padding;
                pad.top + pad.bottom
            },
            0.0
        );

        let (callout, _) = highlight("!> note", false, false);
        for (_, mark) in &callout {
            assert_eq!(format(mark, false).line_padding.top, 9.0, "{mark:?}");
        }
    }

    /// A heading stands off the block above it by about its own line and
    /// hugs the one it introduces — every run of the line, marker included,
    /// asks for the same room so the layout cannot drop it.
    #[test]
    fn a_heading_wears_its_own_room_above_and_below() {
        for (line, [above, below]) in [
            ("# One", HEADING_PAD[0]),
            ("## Two", HEADING_PAD[1]),
            ("### Three", HEADING_PAD[2]),
        ] {
            let (runs, _) = highlight(line, false, false);
            for (_, mark) in &runs {
                let f = format(mark, false);
                assert_eq!(
                    (f.line_padding.top, f.line_padding.bottom),
                    (above, below),
                    "{mark:?}"
                );
            }
        }
        assert!(HEADING_PAD[0][0] > HEADING_PAD[1][0] && HEADING_PAD[1][0] > HEADING_PAD[2][0]);
        assert!(HEADING_PAD[2][0] > BLOCK_PAD, "an H3 still opens a section");
    }
    /// Alignment is a marker after the block prefix: hidden away from the
    /// caret line, and every run of the line asks for the same side.
    #[test]
    fn an_alignment_marker_collapses_and_the_whole_line_takes_its_side() {
        assert_eq!(align_marker("-> x"), (Align::Center, 3));
        assert_eq!(align_marker("->> x"), (Align::End, 4));
        assert_eq!(align_marker("x -> y"), (Align::Start, 0));
        assert_eq!(content_start("## -> x"), 3);
        let (away, _) = highlight("## -> Head", false, false);
        let marker = away
            .iter()
            .find(|(range, _)| *range == (3..6))
            .expect("the alignment marker run");
        assert!(matches!(marker.1, Mark::Marker(_)));
        for (_, mark) in &away {
            assert_eq!(format(mark, false).line_align, Some(wire::Align::Center));
        }
        let (item, _) = highlight("- ->> item", false, false);
        let marker = item
            .iter()
            .find(|(range, _)| *range == (2..6))
            .expect("the alignment marker run");
        assert!(matches!(marker.1, Mark::Marker(_)));
        assert_eq!(format(&item[0].1, false).line_align, Some(wire::Align::End));
        let (plain, _) = highlight("plain", false, false);
        assert_eq!(format(&plain[0].1, false).line_align, None);
    }
}
