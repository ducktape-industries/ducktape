//! pages' CLIENT view model — one applied op, classified for the shell.
//!
//! Beside chat's `client` and pure, so the module-bundled-UI lane compiles it
//! unchanged. This delta stays flat because Ice reads its fields directly;
//! chat's payload enum crosses that boundary opaquely and is dispatched in
//! Rust. The exhaustiveness that matters here lives in the `match` below,
//! which names every wire variant so a new one fails the build here first.

use crate::{PageMsg, decode_msg};

/// One classified pages op. Unused fields stay empty.
#[derive(Clone, Debug, Hash, PartialEq, Default)]
pub struct PagesDelta {
    /// `text` — one block's prose was replaced, and `block_id`/`text` are the
    /// whole of it: the shell folds it and reloads nothing.
    /// `touched` — a pages op the shell must reload for.
    /// empty — not a pages-visible op.
    pub kind: String,
    pub block_id: String,
    pub text: String,
}

/// Classify one applied pages op. `Err` = the payload did not decode — the
/// caller's signal to fall back to a scoped resync.
///
/// ONLY `UpdateText` folds, and that is the whole point: the page autosave
/// commits one per tick while a reader types, so it is the entire storm. Every
/// other variant is a human-rate act (a new line, an indent, a comment) whose
/// reload nobody feels — and folding those means re-deriving `prefix`,
/// `child_count` and sibling order, which is where a fold gets a document
/// wrong.
///
/// MARKS DO NOT REACH THIS SHELL. `PageBlock` (app/src/backend) carries no mark
/// field: `page_blocks` copies `block.text` verbatim, and the editor parses
/// emphasis out of the markdown IN that text (`app/src/editor.rs`,
/// `inline_marks`). So an `UpdateText` that also replaces spans still changes
/// exactly one thing the app can draw — its text — and folding it reproduces
/// what a reload would have produced for that block, byte for byte.
pub fn delta_from_op(payload: &[u8]) -> Result<PagesDelta, String> {
    let touched = |kind: &str| PagesDelta {
        kind: kind.into(),
        ..PagesDelta::default()
    };
    Ok(match decode_msg(payload)? {
        PageMsg::UpdateText { block_id, text, .. } => PagesDelta {
            kind: "text".into(),
            block_id,
            text,
        },
        PageMsg::CommitRecords { changes, .. } => {
            if changes.is_empty() {
                PagesDelta::default()
            } else {
                touched("touched")
            }
        }
        PageMsg::CreateRecordCollection { .. }
        | PageMsg::CreatePage { .. }
        | PageMsg::InsertBlock { .. }
        | PageMsg::SetSpanMark { .. }
        | PageMsg::SetKind { .. }
        | PageMsg::SetChecked { .. }
        | PageMsg::MoveBlock { .. }
        | PageMsg::RemoveBlock { .. }
        | PageMsg::AddComment { .. }
        | PageMsg::MoveCommentThread { .. }
        | PageMsg::EditComment { .. }
        | PageMsg::DeleteComment { .. }
        | PageMsg::ResolveThread { .. } => touched("touched"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, NewBlock, encode_msg};

    #[test]
    fn an_edit_folds_and_everything_else_reloads() {
        let edit = encode_msg(&PageMsg::UpdateText {
            block_id: "b1".into(),
            text: "typed".into(),
            marks: None,
        });
        assert_eq!(
            delta_from_op(&edit).expect("decodes"),
            PagesDelta {
                kind: "text".into(),
                block_id: "b1".into(),
                text: "typed".into(),
            }
        );

        // A split/merge replaces content AND spans atomically. The content is
        // still all the shell can draw, so it folds the same way.
        let with_marks = encode_msg(&PageMsg::UpdateText {
            block_id: "b1".into(),
            text: "split".into(),
            marks: Some(Vec::new()),
        });
        assert_eq!(delta_from_op(&with_marks).expect("decodes").kind, "text");

        let insert = encode_msg(&PageMsg::InsertBlock {
            parent: "page".into(),
            after: None,
            block: NewBlock {
                id: "b2".into(),
                kind: BlockKind::Paragraph,
                text: "new line".into(),
                marks: Vec::new(),
            },
        });
        assert_eq!(delta_from_op(&insert).expect("decodes").kind, "touched");

        let moved = encode_msg(&PageMsg::MoveBlock {
            block_id: "b1".into(),
            parent: Some("page".into()),
            after: None,
        });
        assert_eq!(delta_from_op(&moved).expect("decodes").kind, "touched");
    }

    #[test]
    fn an_undecodable_payload_asks_for_a_resync() {
        assert!(delta_from_op(b"not a pages op").is_err());
    }
}
