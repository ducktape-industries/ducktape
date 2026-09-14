use super::{PageError, PageMsg, Pages, Party};

impl Pages {
    /// apply one decoded [`PageMsg`] to the staged overlay. pure tree surgery
    /// over per-block/-comment records. The caller restores its incoming staging
    /// on error. `actor` is the canonical party the op is recorded under (the
    /// current account, or the signing key of one that has not joined an
    /// account): `CreatePage` records it as the page's author and the comment
    /// ops record it on what they write. Managed collections gate on their
    /// recorded writer and reject all ordinary block mutation routes.
    /// `now` is consulted only by the comment ops (their stored timestamps).
    pub(super) async fn apply(
        &mut self,
        msg: PageMsg,
        actor: &Party,
        now: u64,
    ) -> Result<(), PageError> {
        // no client-minted id may live in the reserved (NUL-prefixed) keyspace:
        // the enumeration index (`\0page-index`) and every comment record/index
        // key lead with NUL, so rejecting NUL-prefixed ids here — BEFORE any
        // storage touch — keeps a block/comment write from ever clobbering them.
        let named: Vec<&str> = match &msg {
            PageMsg::CreateRecordCollection { .. } | PageMsg::CommitRecords { .. } => {
                return Err(PageError::InvalidRecordBatch);
            }
            PageMsg::CreatePage {
                page_id, blocks, ..
            } => std::iter::once(page_id.as_str())
                .chain(blocks.iter().map(|b| b.id.as_str()))
                .collect(),
            PageMsg::InsertBlock { parent, block, .. } => vec![parent.as_str(), block.id.as_str()],
            PageMsg::UpdateText { block_id, .. }
            | PageMsg::SetSpanMark { block_id, .. }
            | PageMsg::SetKind { block_id, .. }
            | PageMsg::SetChecked { block_id, .. }
            | PageMsg::RemoveBlock { block_id } => vec![block_id.as_str()],
            PageMsg::MoveBlock {
                block_id, parent, ..
            } => {
                let mut ids = vec![block_id.as_str()];
                if let Some(parent) = parent {
                    ids.push(parent.as_str());
                }
                ids
            }
            PageMsg::AddComment {
                thread_id,
                comment_id,
                target,
                ..
            } => {
                vec![thread_id.as_str(), comment_id.as_str(), target.as_str()]
            }
            PageMsg::MoveCommentThread {
                thread_id, target, ..
            } => vec![thread_id.as_str(), target.as_str()],
            PageMsg::EditComment { comment_id, .. } => vec![comment_id.as_str()],
            PageMsg::DeleteComment { comment_id } => vec![comment_id.as_str()],
            PageMsg::ResolveThread { thread_id, .. } => vec![thread_id.as_str()],
        };
        if named.iter().any(|id| id.starts_with('\u{0}')) {
            return Err(PageError::ReservedId);
        }

        match &msg {
            PageMsg::InsertBlock { .. }
            | PageMsg::UpdateText { .. }
            | PageMsg::SetSpanMark { .. }
            | PageMsg::SetKind { .. }
            | PageMsg::SetChecked { .. }
            | PageMsg::MoveBlock { .. }
            | PageMsg::RemoveBlock { .. } => {
                for id in named {
                    self.guard_unmanaged_block(id).await?;
                }
            }
            _ => {}
        }

        if matches!(
            &msg,
            PageMsg::AddComment { .. }
                | PageMsg::MoveCommentThread { .. }
                | PageMsg::EditComment { .. }
                | PageMsg::DeleteComment { .. }
                | PageMsg::ResolveThread { .. }
        ) {
            return self.apply_comment_op(msg, actor, now).await;
        }
        if matches!(&msg, PageMsg::CreatePage { .. }) {
            return self.apply_page_op(msg, actor).await;
        }
        self.apply_block_op(msg, actor).await
    }
}
