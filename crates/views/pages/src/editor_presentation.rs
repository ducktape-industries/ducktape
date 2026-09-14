//! The existing Pages Markdown policy copied into a bounded wire presentation.
//! The guest computes styles and hit ranges; the host alone lays out and paints.
use crate::{editor_binding, editor_binding::MenuState, editor_view::EditorReserve, markdown};
use ducktape_view_guest::{EditorStateView, wire};
use std::collections::HashMap;
use wire::editor_presentation::{
    EditorGutter, EditorHit, EditorPresentation, EditorSpan, PresentationError,
};

pub fn paint(
    state: EditorStateView<'_>,
    menu: MenuState,
    dark: bool,
    commented: Vec<i64>,
) -> EditorPresentation {
    build(state, menu, dark, commented, EditorReserve::default()).unwrap_or_default()
}

/// The line's own padding, as the host reads it: the LAST non-zero one of the
/// line's runs wins, so a reserve has to ride on that same padding or it would
/// erase the nesting indent the line was already owed.
fn reserved_padding(
    runs: &[(std::ops::Range<usize>, markdown::Mark)],
    dark: bool,
    height: i64,
) -> wire::Edges {
    let mut padding = runs
        .iter()
        .map(|(_, mark)| markdown::format(mark, dark).line_padding)
        .rfind(|padding| *padding != wire::Edges::default())
        .unwrap_or(wire::Edges::default());
    padding.bottom += height as f32;
    padding
}

pub fn build(
    state: EditorStateView<'_>,
    menu: MenuState,
    dark: bool,
    commented: Vec<i64>,
    reserve: EditorReserve,
) -> Result<EditorPresentation, PresentationError> {
    // Keep one paint pass inside the desktop tick budget. The canonical editor
    // remains complete when rich presentation is too dense to publish at once.
    if state.text.len() > 512 * 1024 || work_bound(state.text) > 8192 {
        return Err(PresentationError::Limit);
    }
    let mut result = editor_binding::menu_paint(state, menu);
    // Existing Pages geometry: 46px gutter + 2px breathing room, 28px
    // comment margin + 2px, and no vertical editor padding.
    result.padding = Some(wire::Edges {
        left: 48.0,
        right: 30.0,
        top: 0.0,
        bottom: 0.0,
    });
    let caret = markdown::Caret { dark, commented };
    let mut highlighter = markdown::DocumentHighlighter::new(&caret);
    let mut formats = HashMap::new();
    for (line, text) in wire::editor_lines(state.text).enumerate() {
        // Native highlighting emits a whole body run followed by inline
        // overrides. Wire runs are disjoint: split that body's remaining tail.
        let mut runs: Vec<(std::ops::Range<usize>, markdown::Mark)> = Vec::new();
        for (range, mark) in highlighter.highlight_line(text) {
            if let Some((tail, _)) = runs.last()
                && range.start < tail.end
            {
                let (tail, body) = runs.pop().expect("last run");
                if range.start < tail.start || range.end > tail.end {
                    return Err(PresentationError::Range);
                }
                if tail.start < range.start {
                    runs.push((tail.start..range.start, body));
                }
                runs.push((range.clone(), mark));
                if range.end < tail.end {
                    runs.push((range.end..tail.end, body));
                }
            } else {
                runs.push((range, mark));
            }
            if runs.len() + result.spans.len() > wire::editor_presentation::MAX_EDITOR_SPANS {
                return Err(PresentationError::Limit);
            }
        }
        // THE GAP IS LAYOUT, NOT PAINT. An inline comment card is a stack layer
        // over one editor widget — nothing can be inserted between two of its
        // lines — so the room it needs is taken as bottom padding on the line
        // it anchors to, and the card floats into what that opens.
        let reserved_line = reserve.height > 0 && line as i64 == reserve.line;
        let padded = match reserved_line {
            true => reserved_padding(&runs, dark, reserve.height),
            false => wire::Edges::default(),
        };
        let last_run = runs.len().saturating_sub(1);
        let mut links = Vec::new();
        for (index, (range, mark)) in runs.into_iter().enumerate() {
            if matches!(mark, markdown::Mark::Body(style) if style.link) {
                links.push(EditorHit {
                    line: line as u32,
                    start: range.start as u32,
                    end: range.end as u32,
                    tag: 2,
                });
            }
            // The host takes the line's padding from its last run that asks for
            // one, so the reserve rides on that run alone and every other line
            // keeps sharing the cached format for its mark.
            let carries_reserve = reserved_line && index == last_run;
            let cached = match carries_reserve {
                true => None,
                false => formats.get(&mark).copied(),
            };
            let format = match cached {
                Some(index) => index,
                None => {
                    if result.formats.len() == wire::editor_presentation::MAX_EDITOR_FORMATS {
                        return Err(PresentationError::Limit);
                    }
                    let index = result.formats.len() as u16;
                    let mut format = markdown::format(&mark, dark);
                    if carries_reserve {
                        format.line_padding = padded;
                    }
                    result.formats.push(format);
                    if !carries_reserve {
                        formats.insert(mark, index);
                    }
                    index
                }
            };
            if result.spans.len() == wire::editor_presentation::MAX_EDITOR_SPANS {
                return Err(PresentationError::Limit);
            }
            result.spans.push(EditorSpan {
                line: line as u32,
                start: range.start as u32,
                end: range.end as u32,
                format,
            });
        }
        if line > 0 {
            result.affordances.gutters.push(EditorGutter {
                line: line as u32,
                plus: true,
                handle: true,
            });
        }
        result.affordances.hits.extend(links);
        // Interactive maps have a separate aggregate cap. Reject while building
        // instead of allocating an arbitrarily large map and dropping its tail.
        let entries = result.affordances.gutters.len() + result.affordances.hits.len();
        if entries > wire::editor_presentation::MAX_EDITOR_SPANS {
            return Err(PresentationError::Limit);
        }
    }
    result.affordances.drop_boundaries = crate::editor_menu::drop_boundaries_text(state.text)
        .into_iter()
        .map(|line| line as u32)
        .collect();
    result.validate(state.text)?;
    Ok(result)
}

fn work_bound(text: &str) -> usize {
    text.bytes()
        .fold(3usize, |cost, byte| {
            cost.saturating_add(match byte {
                b'\n' => 3,
                b'*' | b'_' => 2,
                _ => 0,
            })
        })
        .saturating_add(text.matches("http").count().saturating_mul(2))
}

/// No full document copy: callers already borrow the installed/accepted text.
/// This conservative work bound deliberately switches dense source to plain
/// editing before expensive formatting can exhaust a guest tick.
pub fn format_notice(text: &str) -> String {
    if text.len() > 512 * 1024 || work_bound(text) > 8192 {
        "Formatting is unavailable for this document. Your text and undo history are preserved."
            .into()
    } else {
        String::new()
    }
}
