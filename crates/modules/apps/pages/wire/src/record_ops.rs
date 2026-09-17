//! the pure half of managed records, shared by the module's executor and the
//! derived mapper: which messages carry a record request, the receipt key a
//! request settles under, and the ordinary document ops a record document
//! expands into. no store, no host.
use crate::{Block, BlockKind, NewBlock, PageMsg, RecordDocument};

pub fn receipt_key(page_id: &str, request_id: &str) -> String {
    format!(
        "\0record-receipt:{}",
        serde_json::to_string(&(page_id, request_id)).expect("string tuple serializes")
    )
}

pub fn request_ids(msg: &PageMsg) -> Option<(&str, &str)> {
    match msg {
        PageMsg::CreateRecordCollection {
            page_id,
            request_id,
        }
        | PageMsg::CommitRecords {
            page_id,
            request_id,
            ..
        } => Some((page_id, request_id)),
        _ => None,
    }
}

/// A pure expansion shared by consensus and the derived mapper. The executor
/// applies these ordinary document operations in order within one checkpoint;
/// it never routes them back through the public unmanaged mutation guard.
pub fn document_ops(
    page_id: &str,
    record_id: &str,
    existing: Option<&Block>,
    document: &RecordDocument,
    after: Option<String>,
) -> Vec<PageMsg> {
    let mut ops = Vec::new();
    let old_children = match existing {
        Some(page) => {
            ops.push(PageMsg::UpdateText {
                block_id: record_id.into(),
                text: document.title.clone(),
                marks: None,
            });
            for child in &page.children {
                let retained = document.blocks.iter().any(|block| &block.id == child);
                if !retained {
                    ops.push(PageMsg::RemoveBlock {
                        block_id: child.clone(),
                    });
                }
            }
            page.children.as_slice()
        }
        None => {
            ops.push(PageMsg::InsertBlock {
                parent: page_id.into(),
                after,
                block: NewBlock {
                    id: record_id.into(),
                    kind: BlockKind::Page,
                    text: document.title.clone(),
                    marks: Vec::new(),
                },
            });
            &[]
        }
    };
    let mut after = None;
    for block in &document.blocks {
        let retained = old_children.contains(&block.id);
        if retained {
            ops.push(PageMsg::UpdateText {
                block_id: block.id.clone(),
                text: block.text.clone(),
                marks: Some(block.marks.clone()),
            });
            ops.push(PageMsg::SetKind {
                block_id: block.id.clone(),
                kind: block.kind,
            });
            ops.push(PageMsg::MoveBlock {
                block_id: block.id.clone(),
                parent: Some(record_id.into()),
                after: after.clone(),
            });
        } else {
            ops.push(PageMsg::InsertBlock {
                parent: record_id.into(),
                after: after.clone(),
                block: block.clone(),
            });
        }
        after = Some(block.id.clone());
    }
    ops
}
