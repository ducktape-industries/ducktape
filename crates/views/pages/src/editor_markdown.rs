//! The page document's markdown highlighter.
//!
//! Modelled on `ducktape-ui/examples/markdown-editor`: one `RichTextEditor`
//! over the whole document, and the SYNTAX ITSELF carries the formatting. A
//! line that starts `## ` is a Heading 2 because those three bytes are there,
//! not because a menu was opened — and the three bytes vanish unless the caret
//! is on their line, which is what makes the surface read as WYSIWYG instead of
//! as a source view.
//!
//! Inline emphasis uses the Chat grammar; Pages additionally conceals named
//! link syntax outside the active editing line, without rewriting the source.
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
/// shaped run and takes the caret's column with it. The reference uses the
/// same hair-width value for the same reason.
const HIDDEN_SIZE: f32 = 0.01;
/// The checkbox: one em square, drawn as the span highlight around the tick
/// glyph alone. The three advances under it are Geist's own at `BODY_SIZE`
/// (`[`/`]` 0.347em, a space 0.25em, an `x` 0.585em) — the box is PAINT, so
/// the glyphs are what have to be shimmed to the width it wants.
const TICK_BOX: f32 = BODY_SIZE;
const BRACKET_EM: f32 = 0.347;
const SPACE_ADVANCE: f32 = 0.25 * BODY_SIZE;
const TICK_ADVANCE: f32 = 0.585 * BODY_SIZE;
/// The width the three glyphs occupy, held CONSTANT across both states. An
/// `x` is 4.7px wider than the space it replaces, so ticking a todo widened
/// its box and shoved the whole line's text right; the brackets shrink to
/// absorb the difference, which also keeps the tick centred in its square.
const TICK_RUN: f32 = 2.0 * BRACKET_EM * BODY_SIZE + SPACE_ADVANCE;
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

/// Where the caret is, and which palette is being painted. The whole point of
/// carrying it into the highlighter is the marker reveal: syntax on the caret's
/// own line stays legible so it can be edited, everywhere else it disappears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caret {
    pub focused: bool,
    pub line: usize,
    pub column: usize,
    pub dark: bool,
    /// Document lines inside a block carrying an unresolved comment thread —
    /// they wear a quiet wash so the anchor is visible in the document.
    pub commented: Vec<i64>,
}

/// One painted run. `Marker` is the markdown syntax itself; every other variant
/// is content wearing the shape that syntax declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    Title,
    Marker {
        hidden: bool,
        style: Style,
    },
    Body(Style),
    ListMarker(Style),
    /// A line's leading whitespace. Collapsed to nothing — the nesting step
    /// it stands for is painted as the line's left padding instead.
    Indent(Style),
    /// A todo's tick — the ONE glyph between the brackets, ` ` or `x`. Its
    /// span highlight is the checkbox, and a done todo paints the glyph in
    /// the page's own ground so the box reads as ticked.
    Tick {
        checked: bool,
        style: Style,
    },
    /// A todo's `[` or `]`. Never seen either way — the box is the tick's —
    /// but SIZED rather than collapsed: the pair is the shim that holds the
    /// three-glyph run at one width, so a column of todos keeps one box
    /// column and the body does not move when one is ticked.
    TickEdge {
        checked: bool,
        style: Style,
    },
    Fence {
        hidden: bool,
    },
    CodeBody,
}

/// The shape a line's prefix declared, plus the inline marks inside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub heading: Option<u8>,
    pub quote: bool,
    pub callout: bool,
    pub divider: bool,
    pub strong: bool,
    pub emphasis: bool,
    pub link: bool,
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
    /// `- `, `1. `, `- [ ] ` — KEPT VISIBLE. The marker is the bullet; hiding
    /// it would leave a list looking like a stack of naked paragraphs.
    List,
    /// `> ` — hidden, body reads muted italic.
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
        // A palette flip restyles every line; a caret move only restyles the
        // line it left and the line it landed on.
        let restyle_everything =
            self.caret.dark != caret.dark || self.caret.commented != caret.commented;
        if restyle_everything {
            self.caret = caret.clone();
            self.fences.truncate(1);
            self.current_line = 0;
            return;
        }
        let earliest = self.caret.line.min(caret.line);
        self.caret = caret.clone();
        self.change_line(earliest);
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
        let on_caret_line = self.caret.focused && index == self.caret.line;
        let commented = self.caret.commented.contains(&(index as i64));
        let (marks, next_inside) = match index == 0 {
            true => (vec![(0..line.len(), Mark::Title)], false),
            false => highlight(line, inside_code, on_caret_line, commented),
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
fn highlight(
    line: &str,
    inside_code: bool,
    on_caret_line: bool,
    commented: bool,
) -> (Vec<(Range<usize>, Mark)>, bool) {
    if is_fence(line) {
        let fence = vec![(
            0..line.len(),
            Mark::Fence {
                hidden: !on_caret_line,
            },
        )];
        return (fence, !inside_code);
    }
    if inside_code {
        return (vec![(0..line.len(), Mark::CodeBody)], true);
    }

    let (prefix, content) = prefix_of(line);
    let marker = &line[..content];
    let ticked_done =
        prefix == Prefix::List && (marker.ends_with("[x] ") || marker.ends_with("[X] "));
    let ticked = ticked_done || (prefix == Prefix::List && marker.ends_with("[ ] "));
    let style = Style {
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
    // The marker run. A list keeps its own — it IS the bullet the reader
    // sees — but a todo's bullet yields to the checkbox: the `- ` reveals
    // only under the caret, and the three `[ ]` glyphs split so the box can
    // be a square around the MIDDLE one instead of a band across all three.
    match prefix {
        Prefix::Body => {}
        Prefix::List => match ticked {
            false => marks.push((indent..content, Mark::ListMarker(style))),
            true => {
                marks.push((
                    indent..content - 4,
                    Mark::Marker {
                        hidden: !on_caret_line,
                        style,
                    },
                ));
                marks.push((
                    content - 4..content - 3,
                    Mark::TickEdge {
                        checked: ticked_done,
                        style,
                    },
                ));
                marks.push((
                    content - 3..content - 2,
                    Mark::Tick {
                        checked: ticked_done,
                        style,
                    },
                ));
                marks.push((
                    content - 2..content - 1,
                    Mark::TickEdge {
                        checked: ticked_done,
                        style,
                    },
                ));
            }
        },
        Prefix::Divider => marks.push((
            0..line.len(),
            Mark::Marker {
                hidden: !on_caret_line,
                style,
            },
        )),
        Prefix::Heading(_) | Prefix::Quote | Prefix::Callout => marks.push((
            indent..content,
            Mark::Marker {
                hidden: !on_caret_line,
                style,
            },
        )),
    }

    if prefix == Prefix::Divider {
        return (marks, false);
    }

    // The line's own body carries the block style; the inline scanner then
    // overlays bold/italic/link on top of it, offset past the prefix. A
    // todo's body starts at the gap after the box, so the box keeps its
    // breathing room.
    let body_start = match ticked {
        true => content - 1,
        false => content,
    };
    marks.push((body_start..line.len(), Mark::Body(style)));
    for (range, inline) in document_marks(&line[content..]) {
        let shifted = content + range.start..content + range.end;
        let inline_style = match inline {
            Inline::Marker => {
                marks.push((
                    shifted,
                    Mark::Marker {
                        hidden: !on_caret_line,
                        style,
                    },
                ));
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
    rule: Color,
    tick_fill: Color,
    /// The `x` on a filled checkbox: the page's own ground, showing through.
    tick_mark: Color,
}

const fn rgb8(r: u8, g: u8, b: u8) -> Color {
    wash(r, g, b, 1.0)
}

const fn wash(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a])
}

const LIGHT: Ink = Ink {
    muted: rgb8(0x6b, 0x69, 0x62),
    marker: rgb8(0xb3, 0xb1, 0xa8),
    link: rgb8(0x5f, 0x7a, 0x9e),
    // The chat message block landed here first (#927): a quiet wash with a
    // hairline, never a dark slab — "black/26 was invisible in dark", and a
    // dark slab swallowed its own ink here too.
    code_ink: rgb8(0x3a, 0x38, 0x33),
    code_plate: wash(0x3a, 0x38, 0x33, 0.05),
    code_line: wash(0x3a, 0x38, 0x33, 0.10),
    callout_plate: wash(0xa0, 0x5a, 0x3c, 0.06),
    callout_line: wash(0xa0, 0x5a, 0x3c, 0.14),
    comment_wash: wash(0xa0, 0x5a, 0x3c, 0.09),
    rule: wash(0x3a, 0x38, 0x33, 0.16),
    tick_fill: rgb8(0xa0, 0x5a, 0x3c),
    tick_mark: rgb8(0xfd, 0xfd, 0xfb),
};

const DARK: Ink = Ink {
    muted: rgb8(0xa8, 0xa6, 0x9c),
    marker: rgb8(0x6b, 0x6a, 0x61),
    link: rgb8(0x8f, 0xa9, 0xc9),
    code_ink: rgb8(0xd4, 0xd2, 0xca),
    code_plate: wash(0xd4, 0xd2, 0xca, 0.07),
    code_line: wash(0xd4, 0xd2, 0xca, 0.12),
    callout_plate: wash(0xc9, 0x8a, 0x63, 0.10),
    callout_line: wash(0xc9, 0x8a, 0x63, 0.20),
    comment_wash: wash(0xc9, 0x8a, 0x63, 0.13),
    rule: wash(0xd4, 0xd2, 0xca, 0.18),
    tick_fill: rgb8(0xc9, 0x8a, 0x63),
    tick_mark: rgb8(0x1b, 0x1a, 0x16),
};

fn ink(dark: bool) -> &'static Ink {
    match dark {
        true => &DARK,
        false => &LIGHT,
    }
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

fn tick_advance(checked: bool) -> f32 {
    match checked {
        true => TICK_ADVANCE,
        false => SPACE_ADVANCE,
    }
}

/// The size a bracket renders at, so the invisible pair fills whatever the
/// tick leaves of `TICK_RUN`. Unticked that is the body size; ticked the
/// wider `x` squeezes them.
fn bracket_size(checked: bool) -> f32 {
    (TICK_RUN - tick_advance(checked)) / 2.0 / BRACKET_EM
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
    format
}

/// The vertical inset a run's line is owed, `[above, below]`, on top of
/// whatever its paint set.
fn block_pad(mark: &Mark) -> [f32; 2] {
    let style = match *mark {
        Mark::Indent(style)
        | Mark::Body(style)
        | Mark::ListMarker(style)
        | Mark::Marker { style, .. }
        | Mark::Tick { style, .. }
        | Mark::TickEdge { style, .. } => style,
        Mark::Title => return [BLOCK_PAD; 2],
        Mark::Fence { .. } | Mark::CodeBody => return [0.0; 2],
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
        Mark::Indent(style)
        | Mark::Body(style)
        | Mark::ListMarker(style)
        | Mark::Marker { style, .. }
        | Mark::Tick { style, .. }
        | Mark::TickEdge { style, .. } => style,
        // The title is line 0 and a fence keeps the indentation it is written
        // with — neither nests.
        Mark::Title | Mark::Fence { .. } | Mark::CodeBody => return 0.0,
    };
    f32::from(style.indent) * NEST_STEP
}

fn paint(mark: &Mark, dark: bool) -> Format {
    let ink = ink(dark);
    match *mark {
        Mark::Title => Format {
            font: Some(body_font(Weight::Semibold, FontStyle::Normal)),
            size: Some(TITLE_SIZE),
            line_height: Some(LineHeight::Absolute(TITLE_SIZE * TITLE_LINE_HEIGHT)),
            ..Format::default()
        },
        Mark::Marker { hidden, style } => {
            let mut format = body_format(style, ink);
            // The marker is scaffolding: a done todo strikes its CONTENT, never
            // its bullet — a struck collapsed bullet paints a floating dash.
            format.strikethrough = None;
            format.color = Some(match hidden {
                true => TRANSPARENT,
                false => ink.marker,
            });
            if hidden && style.divider {
                // A divider away from the caret IS the rule: the glyphs go
                // transparent at full size — the line keeps its height and
                // its click target — and the hairline paints across the
                // column. Under the caret the literal `---` returns to be
                // edited.
                format.line_rule = Some(ink.rule);
                return format;
            }
            if hidden {
                // Collapse the glyphs without collapsing the LINE: the body run
                // beside them still carries the real line height.
                format.size = Some(HIDDEN_SIZE);
                format.line_height = None;
            }
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
            // height, so this one holds the body metrics absolutely — the same
            // job the tick does beside a collapsed bullet.
            format.line_height = Some(LineHeight::Absolute(BODY_SIZE * BODY_LINE_HEIGHT));
            format
        }
        Mark::Tick { checked, style } => {
            // The box the reader sees is this one glyph's span highlight — an
            // outline until the todo is done, a filled square after, and the
            // `x` itself paints in the page's own ground on that fill.
            let mut format = body_format(style, ink);
            format.strikethrough = None;
            format.color = Some(match checked {
                true => ink.tick_mark,
                false => TRANSPARENT,
            });
            // The collapsed `- ` bullet beside this run cannot carry the line,
            // so the tick holds the body metrics ABSOLUTELY — the same job the
            // body run does for a heading's hidden marker.
            format.size = Some(BODY_SIZE);
            format.line_height = Some(LineHeight::Absolute(BODY_SIZE * BODY_LINE_HEIGHT));
            let (fill, line) = match checked {
                true => (ink.tick_fill, ink.tick_fill),
                false => (TRANSPARENT, ink.marker),
            };
            format.background = Some(fill);
            format.border = Some(border(line, 1.4, 3.0));
            // Paint-only padding, and the whole reason the box is square: the
            // span is ONE glyph and the highlight grows from its advance, so
            // each side takes half of what the glyph leaves of the box. The
            // vertical is negative — the span is the line's full height.
            let side = (TICK_BOX - tick_advance(checked)) / 2.0;
            let lift = (BODY_SIZE * BODY_LINE_HEIGHT - TICK_BOX) / 2.0;
            format.padding = Padding {
                top: -lift,
                bottom: -lift,
                left: side,
                right: side,
            };
            format
        }
        Mark::TickEdge { checked, style } => {
            let mut format = body_format(style, ink);
            format.strikethrough = None;
            format.color = Some(TRANSPARENT);
            format.size = Some(bracket_size(checked));
            format.line_height = Some(LineHeight::Absolute(BODY_SIZE * BODY_LINE_HEIGHT));
            format
        }
        Mark::Body(style) => body_format(style, ink),
        Mark::Fence { hidden } => Format {
            color: Some(match hidden {
                true => TRANSPARENT,
                false => ink.marker,
            }),
            font: Some(code_font()),
            size: Some(match hidden {
                true => HIDDEN_SIZE,
                false => CODE_SIZE,
            }),
            line_height: Some(LineHeight::Absolute(match hidden {
                // The hidden fence row is the plate's own vertical inset.
                true => CODE_PLATE_PAD,
                false => CODE_SIZE * CODE_LINE_HEIGHT,
            })),
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
    let weight = match style.strong || style.heading.is_some() {
        true => Weight::Semibold,
        false => Weight::Normal,
    };
    let italic = match style.emphasis || style.quote {
        true => FontStyle::Italic,
        false => FontStyle::Normal,
    };
    let color = if style.link {
        Some(ink.link)
    } else if style.done || style.quote || style.divider {
        Some(ink.muted)
    } else {
        None
    };
    let mut format = Format {
        color,
        font: Some(body_font(weight, italic)),
        strikethrough: style.done.then_some(ink.marker),
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
    format
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (marks, inside) = highlight("```", false, false, false);
        assert!(inside);
        assert!(matches!(marks[0].1, Mark::Fence { hidden: true }));
        // `# ` inside a fence is code, not a heading.
        let (body, still_inside) = highlight("# not a heading", true, false, false);
        assert!(still_inside);
        assert_eq!(body, vec![(0..15, Mark::CodeBody)]);
        let (_, closed) = highlight("```", true, false, false);
        assert!(!closed);
    }

    #[test]
    fn the_caret_line_reveals_its_markers_and_other_lines_hide_them() {
        let (away, _) = highlight("## Heading", false, false, false);
        assert!(matches!(away[0].1, Mark::Marker { hidden: true, .. }));
        let (under, _) = highlight("## Heading", false, true, false);
        assert!(matches!(under[0].1, Mark::Marker { hidden: false, .. }));
    }

    #[test]
    fn inline_marks_ride_on_top_of_the_block_style_at_the_right_offsets() {
        let (marks, _) = highlight("## say **hi**", false, false, false);
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
            focused: true,
            line: 0,
            column: 0,
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
        let (marks, _) = highlight(source, false, false, false);
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
            .filter(|(_, mark)| matches!(mark, Mark::Marker { hidden: true, .. }))
            .count();
        assert_eq!(hidden_markers, 2);
    }

    #[test]
    fn a_divider_away_from_the_caret_is_a_rule_not_three_dashes() {
        let (away, _) = highlight("---", false, false, false);
        assert!(matches!(away[0].1, Mark::Marker { hidden: true, .. }));
        let painted = format(&away[0].1, false);
        assert_eq!(painted.color, Some(TRANSPARENT));
        assert!(painted.line_rule.is_some());
        // Full size: the line keeps its height and its click target.
        assert!(painted.size.is_none());
        // Under the caret the literal returns and the rule goes.
        let (under, _) = highlight("---", false, true, false);
        let editable = format(&under[0].1, false);
        assert!(editable.line_rule.is_none());
        assert_ne!(editable.color, Some(TRANSPARENT));
    }

    #[test]
    fn a_todo_line_splits_into_bullet_box_and_body() {
        let (marks, _) = highlight("- [ ] ship", false, false, false);
        assert_eq!(marks[0].0, 0..2);
        assert!(matches!(marks[0].1, Mark::Marker { hidden: true, .. }));
        // The box covers EXACTLY the middle column; the brackets either side
        // are their own invisible runs.
        assert_eq!(marks[1].0, 2..3);
        assert!(matches!(marks[1].1, Mark::TickEdge { checked: false, .. }));
        assert_eq!(marks[2].0, 3..4);
        assert!(matches!(marks[2].1, Mark::Tick { checked: false, .. }));
        assert_eq!(marks[3].0, 4..5);
        assert!(matches!(marks[3].1, Mark::TickEdge { checked: false, .. }));
        // The body starts at the gap so the box keeps its breathing room.
        assert_eq!(marks[4].0, 5..10);
        assert!(matches!(marks[4].1, Mark::Body(_)));
        // An empty box is an outline around an invisible space.
        let tick = format(&marks[2].1, false);
        assert_eq!(tick.color, Some(TRANSPARENT));
        assert!(tick.background.is_some());
        assert_eq!(format(&marks[1].1, false).color, Some(TRANSPARENT));
        // A plain bullet keeps its visible marker, exactly as before.
        let (bullet, _) = highlight("- plain", false, false, false);
        assert!(matches!(bullet[0].1, Mark::ListMarker(_)));
    }

    #[test]
    fn a_done_todo_fills_the_box_and_strikes_the_text() {
        let (marks, _) = highlight("- [x] done", false, false, false);
        assert!(matches!(marks[2].1, Mark::Tick { checked: true, .. }));
        // A filled box carries a VISIBLE mark, in the ground colour — a bare
        // fill reads as a swatch, not as a ticked box.
        let tick = format(&marks[2].1, false);
        assert_eq!(tick.color, Some(LIGHT.tick_mark));
        assert_eq!(tick.background.expect("a filled box"), LIGHT.tick_fill);
        assert!(tick.strikethrough.is_none());
        let Mark::Body(style) = marks[4].1 else {
            unreachable!("a body run")
        };
        assert!(style.done);
        let body = format(&marks[4].1, false);
        assert!(body.strikethrough.is_some());
        assert_eq!(body.color, Some(LIGHT.muted));
    }

    /// The owner's complaint, as arithmetic: the box was three glyphs wide by
    /// one line tall (a rectangle), and it changed width when ticked. It is
    /// one square em now, in the same column either way.
    #[test]
    fn the_checkbox_is_one_square_column_ticked_or_not() {
        let box_of = |checked: bool| {
            let paint = format(
                &Mark::Tick {
                    checked,
                    style: Style::default(),
                },
                false,
            );
            let width = tick_advance(checked) + paint.padding.left + paint.padding.right;
            let height = BODY_SIZE * BODY_LINE_HEIGHT + paint.padding.top + paint.padding.bottom;
            // Where the box starts, measured from the run's own origin: the
            // tick glyph sits one bracket in.
            let left = bracket_size(checked) * BRACKET_EM - paint.padding.left;
            (width, height, left)
        };
        let close = |left: f32, right: f32, what: &str| {
            assert!((left - right).abs() < 0.01, "{what}: {left} vs {right}");
        };
        for checked in [false, true] {
            let (width, height, _) = box_of(checked);
            close(width, TICK_BOX, "box width");
            close(height, TICK_BOX, "box height");
            // The run is one width either way — the brackets absorb the `x`.
            let run = 2.0 * bracket_size(checked) * BRACKET_EM + tick_advance(checked);
            close(run, TICK_RUN, "run width");
        }
        // ...so the box lands in the same column, ticked or not.
        close(box_of(false).2, box_of(true).2, "box column");
        close(bracket_size(false), BODY_SIZE, "an empty box's brackets");
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

        let (todo, _) = highlight("  - [ ] nested", false, false, false);
        inset(&todo, 1.0);
        assert!(matches!(todo[0].1, Mark::Indent(_)));
        assert_eq!(todo[1].0, 2..4);
        assert!(matches!(todo[1].1, Mark::Marker { hidden: true, .. }));

        // Every run of the line asks for the same inset, so a later run with
        // its own padding cannot drop it.
        let (heading, _) = highlight("  ## nested", false, false, false);
        inset(&heading, 1.0);
        assert_eq!(heading[1].0, 2..5);
        for (_, mark) in &heading {
            assert_eq!(format(mark, false).line_padding.left, NEST_STEP);
        }

        let (bullet, _) = highlight("    - deeper", false, false, false);
        assert!(matches!(bullet[0].1, Mark::Indent(_)));
        assert_eq!(
            format(&bullet[0].1, false).line_padding.left,
            2.0 * NEST_STEP
        );
        assert_eq!(bullet[1].0, 4..6);
        assert!(matches!(bullet[1].1, Mark::ListMarker(_)));

        // A line at the margin asks for nothing and gets no indent run.
        let (flat, _) = highlight("- flat", false, false, false);
        assert_eq!(flat[0].0, 0..2);
        assert_eq!(format(&flat[0].1, false).line_padding.left, 0.0);
    }

    #[test]
    fn a_hidden_marker_keeps_a_measurable_size() {
        let hidden = format(
            &Mark::Marker {
                hidden: true,
                style: Style::default(),
            },
            false,
        );
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
            focused: true,
            line: 0,
            column: 0,
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
        let (body, _) = highlight("a paragraph", false, false, false);
        let f = format(&body[0].1, false);
        assert_eq!(
            (f.line_padding.top, f.line_padding.bottom),
            (BLOCK_PAD, BLOCK_PAD)
        );

        let (code, _) = highlight("let x = 1;", true, false, false);
        assert_eq!(
            {
                let pad = format(&code[0].1, false).line_padding;
                pad.top + pad.bottom
            },
            0.0
        );

        let (callout, _) = highlight("!> note", false, false, false);
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
            let (runs, _) = highlight(line, false, false, false);
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
}
