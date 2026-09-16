//! Copied text layout and font metadata. Font bytes remain host-owned.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum LineHeight {
    Relative(f32),
    Absolute(f32),
}
/// Where a line's glyph run sits in its column. `Start` follows the text
/// direction, as an unaligned line does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Shaping {
    Auto,
    Basic,
    Advanced,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Wrapping {
    None,
    Word,
    Glyph,
    WordOrGlyph,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FontFamily {
    Named(String),
    Serif,
    SansSerif,
    Cursive,
    Fantasy,
    Monospace,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum FontStretch {
    UltraCondensed,
    ExtraCondensed,
    Condensed,
    SemiCondensed,
    Normal,
    SemiExpanded,
    Expanded,
    ExtraExpanded,
    UltraExpanded,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum FontStyle {
    Normal,
    Italic,
    Oblique,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedFont {
    pub family: FontFamily,
    pub weight: Weight,
    pub stretch: FontStretch,
    pub style: FontStyle,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TextOptions {
    pub height: Option<Length>,
    pub align_y: Option<AlignY>,
    pub line_height: Option<LineHeight>,
    pub shaping: Option<Shaping>,
    pub wrapping: Option<Wrapping>,
    pub tracking: f32,
    pub font: Option<NamedFont>,
}
impl TextOptions {
    pub(super) fn sanitize(&mut self, text_budget: &mut usize) {
        if let Some(Length::Fixed(height)) = &mut self.height {
            *height = bounded(*height);
        }
        if let Some(line_height) = &mut self.line_height {
            line_height.sanitize();
        }
        self.tracking = bounded(self.tracking).min(MAX_TEXT_PIXELS);
        if let Some(font) = &mut self.font {
            font.sanitize(text_budget);
        }
    }
}

impl LineHeight {
    pub(super) fn sanitize(&mut self) {
        match self {
            Self::Relative(value) => {
                *value = bounded(*value).clamp(f32::EPSILON, MAX_PIXELS / MAX_TEXT_PIXELS)
            }
            Self::Absolute(value) => *value = bounded(*value).max(f32::EPSILON),
        }
    }
}

impl NamedFont {
    pub(super) fn sanitize(&mut self, text_budget: &mut usize) {
        if let FontFamily::Named(name) = &mut self.family {
            spend_text(name, text_budget);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_text_metadata_is_bounded_before_native_layout() {
        let mut options = TextOptions {
            height: Some(Length::Fixed(f32::INFINITY)),
            line_height: Some(LineHeight::Relative(f32::MAX)),
            tracking: f32::NAN,
            font: Some(NamedFont {
                family: FontFamily::Named("é".repeat(64)),
                weight: Weight::Normal,
                stretch: FontStretch::Normal,
                style: FontStyle::Normal,
            }),
            ..TextOptions::default()
        };
        let mut budget = 7;
        options.sanitize(&mut budget);
        assert_eq!(options.line_height, Some(LineHeight::Relative(16.0)));
        assert_eq!(options.tracking, 0.0);
        assert!(
            matches!(options.height, Some(Length::Fixed(value)) if value.is_finite() && value <= MAX_PIXELS)
        );
        let FontFamily::Named(name) = &options.font.unwrap().family else {
            panic!()
        };
        assert_eq!(name, "ééé");
        assert_eq!(budget, 1);
    }

    #[test]
    fn tracked_graphemes_share_the_frame_node_budget() {
        let mut node = Node::Text {
            key: "tracked".into(),
            content: "é".repeat(MAX_NODES * 2),
            size: None,
            color: None,
            font: Font::default(),
            width: None,
            align_x: None,
            options: TextOptions {
                tracking: 2.0,
                ..TextOptions::default()
            },
        };
        sanitize_tree(&mut node).unwrap();
        let Node::Text { content, .. } = node else {
            panic!()
        };
        assert_eq!(content.chars().count(), MAX_NODES - 1);
    }
}
