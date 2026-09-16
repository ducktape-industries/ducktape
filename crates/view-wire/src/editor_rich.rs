//! Toolkit-independent rich editing projection. The guest owns serialization
//! into its canonical document; the host renders blocks and submits snapshots
//! through the same revision-checked editor transaction lane.
use crate::{EditorCursor, editor_presentation::EditorMenuItem};
use serde::{Deserialize, Serialize};

pub const MAX_RICH_BLOCKS: usize = 32_768;
pub const MAX_RICH_MARKS: usize = 32_768;
/// Marks and block metadata are bounded separately from canonical text bytes.
pub const MAX_RICH_BYTES: usize = 4 * crate::editor_document::MAX_EDITOR_DOCUMENT_BYTES;
/// Base and successor projections spend one bounded native request queue.
pub const MAX_RICH_QUEUE_BYTES: usize = 16 * crate::editor_document::MAX_EDITOR_DOCUMENT_BYTES;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichBlock {
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub kind: String,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub text: String,
    pub indent: u32,
    pub level: u8,
    pub checked: bool,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub language: String,
    #[serde(deserialize_with = "decode_marks")]
    pub marks: Vec<RichMark>,
    #[serde(deserialize_with = "decode_attributes")]
    pub attributes: Vec<RichAttribute>,
}

/// Renderer attribute names and values; the host validates its capability set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichAttribute {
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub name: String,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichMark {
    pub start: u32,
    pub end: u32,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub kind: String,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub value: String,
}

/// Positions address a block index and UTF-8 byte column in its plain text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichDocument {
    #[serde(deserialize_with = "decode_blocks")]
    pub blocks: Vec<RichBlock>,
    pub cursor: EditorCursor,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichPresentation {
    pub document: RichDocument,
    /// Guest-selected toolbar entries. Tags return unchanged in RichEdit.
    #[serde(deserialize_with = "decode_toolbar")]
    pub toolbar: Vec<EditorMenuItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichEdit {
    /// Last locally installed snapshot. The guest derives the native delta
    /// from this base so queued typing does not undo a preceding guest command.
    pub before: Option<RichDocument>,
    pub interaction: Option<crate::editor_presentation::EditorInteraction>,
    pub document: RichDocument,
    /// Empty for native typing; otherwise the guest's toolbar action tag.
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub action: String,
}

impl RichDocument {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.blocks.is_empty() || self.blocks.len() > MAX_RICH_BLOCKS {
            return Err("rich block count");
        }
        let mut bytes = 0usize;
        let mut marks = 0usize;
        let mut depth = 0;
        for block in &self.blocks {
            if block.indent > depth || block.indent > 64 {
                return Err("rich block depth");
            }
            depth = block.indent + 1;
            bytes = bytes
                .saturating_add(block.text.len())
                .saturating_add(block.kind.len())
                .saturating_add(block.language.len());
            if block.attributes.len() > 64 {
                return Err("rich attribute count");
            }
            let mut names = std::collections::HashSet::new();
            for attribute in &block.attributes {
                if !names.insert(&attribute.name) {
                    return Err("duplicate rich attribute");
                }
                bytes = bytes
                    .saturating_add(attribute.name.len())
                    .saturating_add(attribute.value.len());
            }
            marks = marks.saturating_add(block.marks.len());
            for mark in &block.marks {
                let start = mark.start as usize;
                let end = mark.end as usize;
                if start >= end
                    || end > block.text.len()
                    || !block.text.is_char_boundary(start)
                    || !block.text.is_char_boundary(end)
                {
                    return Err("rich mark range");
                }
                bytes = bytes
                    .saturating_add(mark.kind.len())
                    .saturating_add(mark.value.len());
            }
            if bytes > MAX_RICH_BYTES || marks > MAX_RICH_MARKS {
                return Err("rich document budget");
            }
        }
        for at in std::iter::once(self.cursor.position).chain(self.cursor.selection) {
            let Some(block) = self.blocks.get(at.line as usize) else {
                return Err("rich cursor block");
            };
            if !block.text.is_char_boundary(at.column as usize) {
                return Err("rich cursor column");
            }
        }
        Ok(())
    }
}
fn decode_blocks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<RichBlock>, D::Error> {
    crate::editor_transaction::decode_bounded::<D, _, MAX_RICH_BLOCKS>(d)
}
fn decode_attributes<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<RichAttribute>, D::Error> {
    crate::editor_transaction::decode_bounded::<D, _, 64>(d)
}
fn decode_marks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<RichMark>, D::Error> {
    crate::editor_transaction::decode_bounded::<D, _, MAX_RICH_MARKS>(d)
}
fn decode_toolbar<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<EditorMenuItem>, D::Error> {
    crate::editor_transaction::decode_bounded::<D, _, 64>(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attributes_are_bounded_unique_and_spend_the_document_budget() {
        let mut document = RichDocument {
            blocks: vec![RichBlock {
                kind: "paragraph".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let attribute = RichAttribute {
            name: "src".into(),
            value: "uri".into(),
        };
        document.blocks[0].attributes = vec![attribute.clone(), attribute.clone()];
        assert!(document.validate().is_err());
        document.blocks[0].attributes = vec![attribute.clone(); 65];
        assert!(crate::decode::<RichDocument>(&crate::encode(&document)).is_err());
        document.blocks[0].attributes = vec![RichAttribute {
            name: "src".into(),
            value: "x".repeat(MAX_RICH_BYTES),
        }];
        assert!(document.validate().is_err());
        document.blocks[0].attributes = vec![attribute];
        assert!(document.validate().is_ok());
    }

    #[test]
    fn maximal_canonical_text_still_fits_a_rich_projection() {
        let document = RichDocument {
            blocks: vec![RichBlock {
                kind: "paragraph".into(),
                text: "x".repeat(crate::editor_document::MAX_EDITOR_DOCUMENT_BYTES),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(document.validate().is_ok());
        let edit = RichEdit {
            before: Some(document.clone()),
            document,
            ..Default::default()
        };
        assert!(crate::encode(&edit).len() <= MAX_RICH_QUEUE_BYTES);
    }

    #[test]
    fn rich_positions_and_marks_reject_split_utf8_and_out_of_range_blocks() {
        let mut document = RichDocument {
            blocks: vec![RichBlock {
                kind: "paragraph".into(),
                text: "한글".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(document.validate().is_ok());
        document.cursor.position.column = 1;
        assert!(document.validate().is_err());
        document.cursor.position.column = 0;
        document.blocks[0].marks.push(RichMark {
            start: 0,
            end: 1,
            kind: "bold".into(),
            value: String::new(),
        });
        assert!(document.validate().is_err());
        document.blocks[0].marks.clear();
        document.cursor.position.line = 1;
        assert!(document.validate().is_err());
    }
    #[test]
    fn rich_projection_bounds_depth_total_text_and_decoded_block_count() {
        let mut document = RichDocument {
            blocks: vec![RichBlock {
                kind: "paragraph".into(),
                indent: u32::MAX,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(document.validate().is_err());
        document.blocks[0].indent = 0;
        document.blocks[0].text = "x".repeat(MAX_RICH_BYTES / 2 + 1);
        document.blocks.push(document.blocks[0].clone());
        assert!(document.validate().is_err());
        document.blocks = vec![RichBlock::default(); MAX_RICH_BLOCKS + 1];
        assert!(crate::decode::<RichDocument>(&crate::encode(&document)).is_err());
    }
}
