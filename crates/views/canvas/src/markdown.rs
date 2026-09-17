//! The markers a card's text wears, read off it.
//!
//! A board card holds one plain string and always will — the module stores
//! what was typed, byte for byte, and this is a reading of it rather than a
//! second format. What a writer types is what the editor shows; what the card
//! draws once the caret has left is the same text with its markers spent.
//!
//! The vocabulary is the one people type without thinking: `**bold**`,
//! `*italic*`, `` `code` ``, `~~struck~~`, a `# ` heading in three sizes, a
//! `- ` bullet, `1. ` numbering and a `> ` quote. Nothing nests — a marker's
//! body is taken whole — and a marker that never closes is not a marker at
//! all, it is the character that was typed, which is what keeps a lone
//! asterisk on a card from swallowing the rest of the line.

/// Which way a run of words is drawn. Four independent switches rather than
/// one style enum: `**`, `*`, `` ` `` and `~~` are separate marks and a run
/// can wear more than one of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Face {
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) code: bool,
    pub(super) struck: bool,
}

/// One stretch of a line drawn one way.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Run {
    pub(super) text: String,
    pub(super) face: Face,
}

/// One line of a card's text with its markers read off.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Line {
    /// How big this line is drawn, as a multiple of the card's own type size.
    pub(super) scale: f32,
    /// Whether the whole line is drawn heavy — a heading is.
    pub(super) heavy: bool,
    /// What the line wears in front of its words: a bullet, a number, a
    /// quote bar. Already the glyph, not the markers that asked for it.
    pub(super) lead: String,
    pub(super) runs: Vec<Run>,
}

impl Line {
    /// Whether this line is drawn exactly as it was typed. A card whose every
    /// line is plain is painted the way it always was — one text node, one
    /// paragraph, the geometry the board has drawn since before any of this.
    pub(super) fn plain(&self) -> bool {
        let unmarked = self.runs.iter().all(|run| run.face == Face::default());
        self.scale == 1. && !self.heavy && self.lead.is_empty() && unmarked
    }
}

/// The openers, longest first: `**` has to be tried before `*` or every bold
/// run is an italic one that starts with an asterisk.
const MARKS: [(&str, Face); 4] = [
    (
        "**",
        Face {
            bold: true,
            italic: false,
            code: false,
            struck: false,
        },
    ),
    (
        "~~",
        Face {
            bold: false,
            italic: false,
            code: false,
            struck: true,
        },
    ),
    (
        "`",
        Face {
            bold: false,
            italic: false,
            code: true,
            struck: false,
        },
    ),
    (
        "*",
        Face {
            bold: false,
            italic: true,
            code: false,
            struck: false,
        },
    ),
];

/// Read a card's text. One `Line` per line, always — an empty line is a blank
/// line on the card, the way it is in the editor.
pub(super) fn read(text: &str) -> Vec<Line> {
    text.split('\n').map(line).collect()
}

/// Whether anything at all is marked up. A card with no markers takes the
/// plain path, so the ordinary card is drawn by the ordinary code.
pub(super) fn marked(lines: &[Line]) -> bool {
    !lines.iter().all(Line::plain)
}

fn line(source: &str) -> Line {
    let (scale, heavy, lead, rest) = prefix(source);
    Line {
        scale,
        heavy,
        lead,
        runs: runs(rest),
    }
}

/// What a line wears in front of its words, and how big it is drawn.
fn prefix(source: &str) -> (f32, bool, String, &str) {
    let hashes = source.bytes().take_while(|byte| *byte == b'#').count();
    let heading = (1..=3).contains(&hashes) && source[hashes..].starts_with(' ');
    if heading {
        // Three steps and no more: a fourth would be the size of the card.
        let scale = [1.5, 1.25, 1.1][hashes - 1];
        return (
            scale,
            true,
            String::new(),
            source[hashes + 1..].trim_start(),
        );
    }
    // `- ` and `* ` are bullets; `*italic*` is not, because it has no space
    // after the marker and a bullet must.
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = source.strip_prefix(marker) {
            return (1., false, "•\u{2002}".to_owned(), rest);
        }
    }
    if let Some(rest) = source.strip_prefix("> ") {
        return (1., false, "\u{2503}\u{2002}".to_owned(), rest);
    }
    let digits = source.bytes().take_while(u8::is_ascii_digit).count();
    let numbered = digits > 0 && source[digits..].starts_with(". ");
    if numbered {
        let lead = format!("{}.\u{2002}", &source[..digits]);
        return (1., false, lead, &source[digits + 2..]);
    }
    (1., false, String::new(), source)
}

/// Cut a line into the stretches drawn each way.
fn runs(source: &str) -> Vec<Run> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = source;
    while !rest.is_empty() {
        let Some((mark, face)) = MARKS.iter().find(|(mark, _)| rest.starts_with(mark)) else {
            let step = rest
                .chars()
                .next()
                .expect("a non-empty remainder has a first character")
                .len_utf8();
            plain.push_str(&rest[..step]);
            rest = &rest[step..];
            continue;
        };
        let body = &rest[mark.len()..];
        // A marker hugs the words it marks, on both sides. `2 * 3 * 4` is
        // arithmetic and stays arithmetic; `*three*` is emphasis. An opener
        // that never closes, or closes on nothing, is likewise just the
        // characters someone typed.
        let hugs = !body.starts_with(char::is_whitespace);
        let closed = body
            .find(mark)
            .filter(|end| *end > 0 && !body[..*end].ends_with(char::is_whitespace));
        let Some(end) = closed.filter(|_| hugs) else {
            plain.push_str(mark);
            rest = body;
            continue;
        };
        if !plain.is_empty() {
            out.push(Run {
                text: std::mem::take(&mut plain),
                face: Face::default(),
            });
        }
        out.push(Run {
            text: body[..end].to_owned(),
            face: *face,
        });
        rest = &body[end + mark.len()..];
    }
    if !plain.is_empty() {
        out.push(Run {
            text: plain,
            face: Face::default(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faces(line: &Line) -> Vec<(&str, Face)> {
        line.runs
            .iter()
            .map(|run| (run.text.as_str(), run.face))
            .collect()
    }

    #[test]
    fn a_card_with_no_markers_is_read_as_the_text_it_is() {
        let lines = read("plain words\nand more");
        assert!(!marked(&lines));
        assert_eq!(lines.len(), 2);
        assert_eq!(faces(&lines[0]), [("plain words", Face::default())]);
    }

    #[test]
    fn the_four_inline_marks_cut_a_line_into_runs() {
        let lines = read("a **b** c *d* `e` ~~f~~");
        assert!(marked(&lines));
        let bold = Face {
            bold: true,
            ..Default::default()
        };
        let italic = Face {
            italic: true,
            ..Default::default()
        };
        let code = Face {
            code: true,
            ..Default::default()
        };
        let struck = Face {
            struck: true,
            ..Default::default()
        };
        assert_eq!(
            faces(&lines[0]),
            [
                ("a ", Face::default()),
                ("b", bold),
                (" c ", Face::default()),
                ("d", italic),
                (" ", Face::default()),
                ("e", code),
                (" ", Face::default()),
                ("f", struck),
            ]
        );
    }

    #[test]
    fn a_marker_that_never_closes_is_the_character_that_was_typed() {
        // Arithmetic is not emphasis: every marker here is loose, so the line
        // arrives as the one plain run it was typed as.
        let arithmetic = read("2 * 3 * 4 and **half");
        assert!(!marked(&arithmetic));
        assert_eq!(
            faces(&arithmetic[0]),
            [("2 * 3 * 4 and **half", Face::default())]
        );
        assert!(!marked(&read("a * b")));
        assert!(!marked(&read("****")));
        assert!(!marked(&read("a ~~ b ~~ c")));
    }

    #[test]
    fn line_markers_set_the_size_and_what_rides_in_front() {
        let lines = read("# Big\n## Less\n### Least\n#### Not a heading\n- one\n2. two\n> said");
        assert_eq!(
            lines.iter().map(|line| line.scale).collect::<Vec<_>>(),
            [1.5, 1.25, 1.1, 1., 1., 1., 1.]
        );
        assert!(lines[0].heavy && !lines[4].heavy);
        assert_eq!(lines[0].runs[0].text, "Big");
        assert_eq!(lines[3].runs[0].text, "#### Not a heading");
        assert_eq!(lines[4].lead, "•\u{2002}");
        assert_eq!(lines[4].runs[0].text, "one");
        assert_eq!(lines[5].lead, "2.\u{2002}");
        assert_eq!(lines[6].lead, "\u{2503}\u{2002}");
    }

    #[test]
    fn a_heading_carries_its_own_inline_marks() {
        let lines = read("## a **b**");
        assert_eq!(lines[0].scale, 1.25);
        assert!(lines[0].runs[1].face.bold);
    }

    #[test]
    fn an_empty_line_survives_as_an_empty_line() {
        let lines = read("one\n\ntwo");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].runs.is_empty());
        assert!(lines[1].plain());
    }
}
