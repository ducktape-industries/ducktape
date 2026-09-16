//! Renderer-independent editor positions: zero-based lines and UTF-8 byte columns.
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorCursor {
    /// Active end of the selection (the caret).
    pub position: EditorPosition,
    /// Fixed end, without sorting or losing selection direction. None clears selection.
    pub selection: Option<EditorPosition>,
}

/// Logical lines used by native Content, excluding LF/CRLF/CR/LFCR terminators.
/// Empty documents and trailing terminators retain their final empty line.
pub fn editor_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut remaining = Some(text);
    std::iter::from_fn(move || {
        let text = remaining.take()?;
        // ASCII delimiters are UTF-8 boundaries. The portable byte search skips
        // whole words of ordinary prose on repeated presentation validations.
        let end = memchr::memchr2(b'\r', b'\n', text.as_bytes()).unwrap_or(text.len());
        if end < text.len() {
            let ending = &text[end..];
            let width = if ending.starts_with("\r\n") || ending.starts_with("\n\r") {
                2
            } else {
                1
            };
            remaining = Some(&text[end + width..]);
        }
        Some(&text[..end])
    })
}

impl EditorPosition {
    fn clamp(&mut self, text: &str) {
        let (line, source) = editor_lines(text)
            .enumerate()
            .take((self.line as usize).saturating_add(1))
            .last()
            .unwrap();
        self.line = line as u32;
        let column = (self.column as usize).min(source.len());
        self.column = source
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .chain(std::iter::once(source.len()))
            .take_while(|at| *at <= column)
            .last()
            .unwrap_or(0) as u32;
    }
}
impl EditorCursor {
    /// Clamp both ends backward to extended grapheme boundaries in bounded text.
    pub fn clamp(&mut self, text: &str) {
        self.position.clamp(text);
        if let Some(anchor) = &mut self.selection {
            anchor.clamp(text);
        }
    }
}

/// A complete host observation, fenced by the guest's authoritative reset revision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorState {
    pub text: String,
    pub cursor: EditorCursor,
    pub reset: u64,
    /// Monotonic host observation order within the instance.
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logical_lines_preserve_mixed_terminators_and_utf8_byte_ranges() {
        let long = "한글 e\u{301} 👩‍💻 ordinary text ".repeat(4096);
        for first in ["\n", "\r", "\r\n", "\n\r"] {
            for second in ["\n", "\r", "\r\n", "\n\r"] {
                let text = format!("{long}{first}middle 한글{second}tail 👩‍💻");
                assert_eq!(
                    editor_lines(&text).collect::<Vec<_>>(),
                    [&long, "middle 한글", "tail 👩‍💻"],
                    "terminators {first:?}, {second:?}"
                );
            }
        }
        for (text, expected) in [
            ("", vec![""]),
            ("\r\n", vec!["", ""]),
            ("\n\r", vec!["", ""]),
            ("\r\r\n\n", vec!["", "", "", ""]),
            ("한\r\n글\n\r", vec!["한", "글", ""]),
        ] {
            assert_eq!(editor_lines(text).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn columns_exclude_native_line_ending_bytes() {
        for ending in ["\n", "\r\n", "\r", "\n\r"] {
            let text = format!("한{ending}글{ending}");
            let mut cursor = EditorCursor {
                position: EditorPosition {
                    line: 0,
                    column: u32::MAX,
                },
                selection: Some(EditorPosition {
                    line: 2,
                    column: 20,
                }),
            };
            cursor.clamp(&text);
            assert_eq!(
                cursor.position.column, 3,
                "line ending {ending:?} is not a column"
            );
            assert_eq!(
                cursor.selection.unwrap(),
                EditorPosition { line: 2, column: 0 }
            );
        }
    }

    #[test]
    fn byte_positions_snap_backward_without_splitting_graphemes() {
        for (text, inside, expected) in [
            ("- 한글", 7, 5),
            ("- 👍🏽", 6, 2),
            ("e\u{301}", 1, 0),
            ("👩‍💻", 7, 0),
        ] {
            let mut cursor = EditorCursor {
                position: EditorPosition {
                    line: 0,
                    column: inside,
                },
                selection: Some(EditorPosition {
                    line: 9,
                    column: u32::MAX,
                }),
            };
            cursor.clamp(text);
            assert_eq!(cursor.position.column, expected, "{text}");
            assert_eq!(
                cursor.selection,
                Some(EditorPosition {
                    line: 0,
                    column: text.len() as u32
                })
            );
        }
        let mut cursor = EditorCursor {
            position: EditorPosition {
                line: u32::MAX,
                column: u32::MAX,
            },
            selection: None,
        };
        cursor.clamp("한글\n");
        assert_eq!(cursor.position, EditorPosition { line: 1, column: 0 });
        assert_eq!(cursor.selection, None);
    }
}
