use super::{
    Ctx, Error, Module, ModuleId, Msg, PageQuery, PageReply, Pages, Party, ResolverSyncTarget,
    StateRoot, StateSyncHandle, decode_msg, decode_query, encode_reply,
};
use attribution::{Actor, AttributionMsg, AttributionUpdate, ObjectRef, Reason, Relation};
use sdk::Origin;
use std::collections::{BTreeMap, BTreeSet};

/// A module-wide clock gives every changed source a newer revision, including
/// a deleted object whose client-minted id is later reused. Authorship lives in
/// the source record, so a large subtree needs no extra per-object store reads.
const SOURCE_REVISION_KEY: &[u8] = b"\0attribution-revision";
/// Bound source/recipient work per guest dispatch, leaving fuel and read
/// headroom for attribution's counters, history writes and subscribers.
const ATTRIBUTION_BATCH_READ_BUDGET: usize = 128;
/// Full JSON wire envelopes, including integer-array encoding of detail bytes.
/// Leave the same framing margin as ordinary Page store values below 1 MiB.
const ATTRIBUTION_BATCH_BYTES: usize = super::MAX_BLOCK_LEN;
const ATTRIBUTION_REPORT_BYTES: usize = 8 * sdk::MAX_STORE_VALUE_BYTES;

enum DiscussionEffect {
    Created(String),
    Recreated,
    Edited,
    Retargeted,
    ContextChanged,
}

impl DiscussionEffect {
    fn mutation(&self) -> super::DiscussionMutation {
        match self {
            Self::Created(_) => super::DiscussionMutation::Created,
            Self::Recreated => super::DiscussionMutation::Recreated,
            Self::Edited => super::DiscussionMutation::Edited,
            Self::Retargeted => super::DiscussionMutation::Retargeted,
            Self::ContextChanged => super::DiscussionMutation::ContextChanged,
        }
    }
}

fn comment_identity_key(id: &str) -> Vec<u8> {
    format!("\0comment-identity:{id}").into_bytes()
}

fn actor_of(party: &Party) -> Actor {
    match party {
        Party::Account(account) => Actor::Account(*account),
        Party::Key(key) => Actor::Key(key.clone()),
        Party::Module(module) => Actor::Module(module.clone()),
        Party::System => Actor::System,
    }
}

fn authored_relations(author: &Party) -> Vec<Relation> {
    match author {
        Party::Account(account) => vec![Relation {
            recipient: *account,
            reason: Reason::Authorship,
            detail: Vec::new(),
        }],
        Party::Key(_) | Party::Module(_) | Party::System => Vec::new(),
    }
}

fn source_relations(kind: &str, value: Option<&[u8]>) -> Result<Vec<Relation>, Error> {
    let relations = match (kind, value) {
        ("comment", Some(bytes)) => {
            let comment: super::Comment = sdk::wire::decode(bytes).map_err(Error::Module)?;
            if comment.deleted {
                Vec::new()
            } else {
                let mut relations = authored_relations(&comment.author);
                let mentions: BTreeSet<_> = comment.mentions.into_iter().collect();
                relations.extend(mentions.into_iter().map(|recipient| Relation {
                    recipient,
                    reason: Reason::Mention,
                    detail: Vec::new(),
                }));
                relations
            }
        }
        ("block", Some(bytes)) => {
            let block: super::Block = sdk::wire::decode(bytes).map_err(Error::Module)?;
            let mut relations = authored_relations(&block.author);
            let mentions: BTreeSet<_> = block
                .marks
                .iter()
                .filter_map(|mark| match mark.kind {
                    super::InlineMark::Mention(account) => Some(account),
                    _ => None,
                })
                .collect();
            relations.extend(mentions.into_iter().map(|recipient| Relation {
                recipient,
                reason: Reason::Mention,
                detail: Vec::new(),
            }));
            let is_page = block.kind == super::BlockKind::Page;
            if is_page && let Party::Account(recipient) = block.author {
                relations.push(Relation {
                    recipient,
                    reason: Reason::Ownership,
                    detail: Vec::new(),
                });
            }
            relations
        }
        (_, None) => Vec::new(),
        _ => unreachable!("source kinds are closed above"),
    };
    Ok(relations)
}

fn source_object(key: &[u8]) -> Result<(&str, &str), Error> {
    let key =
        std::str::from_utf8(key).map_err(|_| Error::Module("pages: corrupt logical key".into()))?;
    match key.strip_prefix("\0cc:") {
        Some(id) => Ok(("comment", id)),
        None if !key.starts_with('\0') => Ok(("block", key)),
        None => Err(Error::Module("pages: not an attribution source".into())),
    }
}

fn top_level_page<'a>(
    index: &'a BTreeMap<String, Option<String>>,
    page: &'a str,
) -> Result<&'a str, Error> {
    let mut current = page;
    for _ in 0..super::MAX_PAGES {
        match index.get(current) {
            Some(Some(parent)) => current = parent,
            Some(None) => return Ok(current),
            None => break,
        }
    }
    Err(Error::Module(super::PageError::Corrupt.to_string()))
}

fn attribution_batch(target: &str, updates: Vec<AttributionUpdate>) -> Msg {
    Msg {
        target: target.into(),
        payload: attribution::encode_msg(&AttributionMsg::AttributeBatch { updates }),
    }
}

fn push_attribution_batch(
    reports: &mut Vec<Msg>,
    target: &str,
    updates: Vec<AttributionUpdate>,
) -> Result<(), Error> {
    let report = attribution_batch(target, updates);
    let too_large = report.payload.len() > ATTRIBUTION_BATCH_BYTES;
    let total_bytes = reports
        .iter()
        .map(|report| report.payload.len())
        .sum::<usize>()
        + report.payload.len();
    let total_exceeded = total_bytes > ATTRIBUTION_REPORT_BYTES;
    if too_large || total_exceeded {
        return Err(Error::Module(
            "pages: attribution report envelope too large".into(),
        ));
    }
    reports.push(report);
    Ok(())
}

impl Pages {
    fn validate_source_envelope(&self, update: &AttributionUpdate) -> Result<(), Error> {
        let source = attribution::ObjectRelations {
            source: attribution::Source {
                module: self.id.clone(),
                kind: update.object.kind.clone(),
                object: update.object.object.clone(),
            },
            revision: update.revision,
            relations: update.relations.clone(),
            changes: u64::MAX,
        };
        // This full JSON projection is more conservative than attribution's
        // Borsh stored row, and accounts for detail as a JSON integer array.
        let source_bytes = sdk::wire::encode(&source).len();
        let oversized = source_bytes > ATTRIBUTION_BATCH_BYTES;
        if oversized {
            return Err(Error::Module(
                "pages: attribution source envelope too large".into(),
            ));
        }
        Ok(())
    }

    async fn discussion_effect(&self, msg: &super::PageMsg) -> Result<DiscussionEffect, Error> {
        match msg {
            super::PageMsg::AddComment { comment_id, .. } => {
                match self.staged.get(&comment_identity_key(comment_id)).await? {
                    None => Ok(DiscussionEffect::Created(comment_id.clone())),
                    Some(_) => Ok(DiscussionEffect::Recreated),
                }
            }
            super::PageMsg::EditComment { .. } => Ok(DiscussionEffect::Edited),
            super::PageMsg::MoveCommentThread { .. } => Ok(DiscussionEffect::Retargeted),
            _ => Ok(DiscussionEffect::ContextChanged),
        }
    }

    fn record_discussion_identity(&mut self, effect: &DiscussionEffect) {
        if let DiscussionEffect::Created(id) = effect {
            // Never purged with comment bytes: recycling an old comment ID is
            // not a new human decision, even after its last thread was deleted.
            self.staged.stage(comment_identity_key(id), Vec::new());
        }
    }

    async fn identity_account(
        &self,
        ctx: &dyn Ctx,
        query: identity::IdentityQuery,
    ) -> Result<Option<u64>, Error> {
        let Some(identity) = &self.identity else {
            return Ok(None);
        };
        let bytes = ctx.query(identity, &identity::encode_query(&query)).await?;
        let identity::IdentityReply::Account(account) =
            identity::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("pages: unexpected identity reply".into()));
        };
        Ok(account.map(|account| account.number))
    }

    async fn party_of_origin(&self, ctx: &dyn Ctx) -> Result<Party, Error> {
        match &ctx.env().origin {
            Origin::External(key) => {
                if key.is_empty() {
                    return Err(Error::Module(super::PageError::EmptyOrigin.to_string()));
                }
                if key.len() > super::MAX_COMMENT_AUTHOR_BYTES {
                    return Err(Error::Module(super::PageError::AuthorTooLarge.to_string()));
                }
                let account = self
                    .identity_account(ctx, identity::IdentityQuery::OfKey { key: key.clone() })
                    .await?;
                Ok(match account {
                    Some(account) => Party::Account(account),
                    None => Party::Key(key.clone()),
                })
            }
            Origin::Program(account) => Ok(Party::Account(*account)),
            Origin::Module(module) => {
                if module.len() > super::MAX_COMMENT_AUTHOR_BYTES {
                    return Err(Error::Module(super::PageError::AuthorTooLarge.to_string()));
                }
                Ok(Party::Module(module.clone()))
            }
            Origin::System => Ok(Party::System),
        }
    }

    async fn validate_mentions(&self, ctx: &dyn Ctx, msg: &super::PageMsg) -> Result<(), Error> {
        use super::{InlineMark, PageMsg};
        let accounts: BTreeSet<u64> = match msg {
            PageMsg::AddComment { mentions, .. } | PageMsg::EditComment { mentions, .. } => {
                mentions.iter().copied().collect()
            }
            PageMsg::CommitRecords { changes, .. } => changes
                .iter()
                .filter_map(|change| match change {
                    super::RecordChange::Upsert { document, .. } => Some(document),
                    super::RecordChange::Delete { .. } => None,
                })
                .flat_map(|document| &document.blocks)
                .flat_map(|block| &block.marks)
                .filter_map(|mark| match mark.kind {
                    InlineMark::Mention(account) => Some(account),
                    _ => None,
                })
                .collect(),
            PageMsg::InsertBlock { block, .. } => block
                .marks
                .iter()
                .filter_map(|mark| match mark.kind {
                    InlineMark::Mention(account) => Some(account),
                    _ => None,
                })
                .collect(),
            PageMsg::UpdateText {
                marks: Some(marks), ..
            } => marks
                .iter()
                .filter_map(|mark| match mark.kind {
                    InlineMark::Mention(account) => Some(account),
                    _ => None,
                })
                .collect(),
            PageMsg::SetSpanMark {
                kind: InlineMark::Mention(account),
                active: true,
                ..
            } => BTreeSet::from([*account]),
            _ => BTreeSet::new(),
        };
        let accounts: Vec<_> = accounts.into_iter().collect();
        for chunk in accounts.chunks(identity::MAX_QUERY_LIMIT as usize) {
            let numbers = match &self.identity {
                Some(identity) => {
                    let references = chunk
                        .iter()
                        .copied()
                        .map(identity::AccountRef::Account)
                        .collect();
                    let bytes = ctx
                        .query(
                            identity,
                            &identity::encode_query(&identity::IdentityQuery::Resolve {
                                references,
                            }),
                        )
                        .await?;
                    let identity::IdentityReply::Resolved(numbers) =
                        identity::decode_reply(&bytes).map_err(Error::Module)?
                    else {
                        return Err(Error::Module("pages: unexpected identity reply".into()));
                    };
                    numbers
                }
                None => vec![None; chunk.len()],
            };
            if numbers.len() != chunk.len() {
                return Err(Error::Module(
                    "pages: identity resolution count mismatch".into(),
                ));
            }
            for (number, resolved) in chunk.iter().zip(numbers) {
                let exists = *number != 0 && resolved == Some(*number);
                if !exists {
                    return Err(Error::Module(format!(
                        "pages: mention names no account: {number}"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn prefetch_relation_sources(
        &self,
        keys: Vec<Vec<u8>>,
        work: &mut BTreeSet<Vec<u8>>,
    ) -> Result<(), Error> {
        work.extend(keys.iter().cloned());
        let exceeds_budget = work.len() > super::MAX_TRAVERSAL_WORK;
        if exceeds_budget {
            return Err(Error::Module(
                "pages: attribution source work exceeded".into(),
            ));
        }
        self.staged.prefetch(&keys).await
    }

    /// Resolve current full snapshots or prior recipient identities. Thread, block and
    /// collection reads are deduplicated and prefetched by frontier; the existing
    /// page index resolves physical page ancestry without one host read per hop.
    /// The caller selects the before/after overlay, including deleted targets.
    async fn source_relation_sets(
        &self,
        values: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
        snapshot_mutation: Option<super::DiscussionMutation>,
    ) -> Result<BTreeMap<Vec<u8>, Vec<Relation>>, Error> {
        let mut relations = BTreeMap::new();
        let mut comments = BTreeMap::new();
        for (key, value) in values {
            let (kind, _) = source_object(key)?;
            relations.insert(key.clone(), source_relations(kind, value.as_deref())?);
            let ("comment", Some(bytes)) = (kind, value) else {
                continue;
            };
            let comment: super::Comment = sdk::wire::decode(bytes).map_err(Error::Module)?;
            if !comment.deleted {
                comments.insert(key, comment);
            }
        }
        if comments.is_empty() {
            return Ok(relations);
        }
        let thread_ids: BTreeSet<_> = comments
            .values()
            .map(|comment| comment.thread_id.clone())
            .collect();
        let mut work = values.keys().cloned().collect();
        let keys = std::iter::once(super::PAGE_INDEX_KEY.as_bytes().to_vec())
            .chain(
                thread_ids
                    .iter()
                    .map(|id| crate::comment_ops::thread_key(id).into_bytes()),
            )
            .collect();
        self.prefetch_relation_sources(keys, &mut work).await?;
        let mut threads = BTreeMap::new();
        for id in thread_ids {
            let thread = self
                .load_thread(&id)
                .await
                .map_err(|error| Error::Module(error.to_string()))?
                .ok_or_else(|| Error::Module(super::PageError::Corrupt.to_string()))?;
            threads.insert(id, thread);
        }
        let targets: BTreeSet<_> = threads.values().map(|thread| &thread.target).collect();
        let keys = targets.iter().map(|id| id.as_bytes().to_vec()).collect();
        self.prefetch_relation_sources(keys, &mut work).await?;
        let index = self.load_index().await?;
        let mut roots = BTreeMap::new();
        for target in targets {
            let block = self
                .require_block(target, super::PageError::Corrupt)
                .await
                .map_err(|error| Error::Module(error.to_string()))?;
            roots.insert(
                target,
                (top_level_page(&index, &block.page)?.to_owned(), block.page),
            );
        }
        let root_ids: BTreeSet<_> = roots.values().map(|(root, _)| root).collect();
        let keys = root_ids
            .iter()
            .map(|id| crate::record_ops::collection_key(id).into_bytes())
            .collect();
        self.prefetch_relation_sources(keys, &mut work).await?;
        let mut writers = BTreeMap::new();
        for root in root_ids {
            let collection = self
                .record_collection(root)
                .await
                .map_err(|error| Error::Module(error.to_string()))?;
            if let Some(super::RecordCollection {
                writer: Party::Account(writer),
                ..
            }) = collection
            {
                writers.insert(root, writer);
            }
        }
        for (key, comment) in comments {
            let thread = &threads[&comment.thread_id];
            let (root, page_id) = &roots[&thread.target];
            let Some(writer) = writers.get(root) else {
                continue;
            };
            // Consumers must never pair mutable GetComment text with an older
            // event's actor. Freeze the source and context at this revision.
            let detail = match snapshot_mutation {
                None => Vec::new(),
                Some(mutation) => sdk::wire::encode(&super::ManagedDiscussionSnapshot {
                    mutation,
                    collection_page_id: root.clone(),
                    page_id: page_id.clone(),
                    comment,
                    thread: super::DiscussionThreadSnapshot {
                        id: thread.id.clone(),
                        target: thread.target.clone(),
                        opener: thread.opener.clone(),
                        created_at: thread.created_at,
                        anchor: thread.anchor.clone(),
                        resolved: thread.resolved,
                        resolved_by: thread.resolved_by.clone(),
                    },
                }),
            };
            let exceeds_bound = detail.len() > super::MAX_MANAGED_DISCUSSION_BYTES;
            if exceeds_bound {
                return Err(Error::Module(
                    "pages: managed discussion snapshot too large".into(),
                ));
            }
            relations
                .get_mut(key)
                .expect("source exists")
                .push(Relation {
                    recipient: *writer,
                    reason: Reason::Defined(super::MANAGED_RECORD_COMMENT_REASON.into()),
                    detail,
                });
        }
        Ok(relations)
    }

    async fn attribution_reports(
        &mut self,
        actor: &Party,
        before: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
        mutation: super::DiscussionMutation,
    ) -> Result<Vec<Msg>, Error> {
        let Some(attribution) = self.attribution.clone() else {
            return Ok(Vec::new());
        };
        let mut changed: BTreeMap<_, _> = self
            .staged
            .staged_writes()
            .iter()
            .filter(|(key, value)| {
                let source_record = !key.starts_with(b"\0") || key.starts_with(b"\0cc:");
                let changed = before.get(*key) != Some(*value);
                source_record && changed
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let written: BTreeSet<_> = changed.keys().cloned().collect();
        let changed_threads: Vec<_> = self
            .staged
            .staged_writes()
            .iter()
            .filter(|(key, value)| key.starts_with(b"\0ct:") && before.get(*key) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let unchanged = changed.is_empty() && changed_threads.is_empty();
        if unchanged {
            return Ok(Vec::new());
        }
        // Prefetch canonical pre-op records before replacing their relation
        // sets. Restore the successful view even on a prefetch/read failure.
        let after = self.staged.checkpoint();
        self.staged.restore(before.clone());
        let prior = async {
            let mut keys: Vec<_> = changed
                .keys()
                .cloned()
                .chain(changed_threads.iter().map(|(key, _)| key.clone()))
                .collect();
            keys.push(SOURCE_REVISION_KEY.to_vec());
            // Preserve the existing plain-block prefetch budget; only new
            // discussion context resolution spends the bounded source work.
            let mut work = keys.iter().cloned().collect();
            self.staged.prefetch(&keys).await?;
            // A retarget leaves comment bytes unchanged but changes their
            // discussion relation. Re-report those sources, not the thread as
            // a fabricated comment or a new comment authored by its mover.
            for (key, next) in changed_threads {
                let Some(next) = next else { continue };
                let next: super::Thread = sdk::wire::decode(&next).map_err(Error::Module)?;
                let Some(previous) = self.staged.get(&key).await? else {
                    continue;
                };
                let previous: super::Thread =
                    sdk::wire::decode(&previous).map_err(Error::Module)?;
                let retargeted = previous.target != next.target;
                if !retargeted {
                    continue;
                }
                let keys: Vec<_> = next
                    .comment_ids
                    .iter()
                    .map(|id| crate::comment_ops::comment_key(id).into_bytes())
                    .collect();
                self.prefetch_relation_sources(keys.clone(), &mut work)
                    .await?;
                for key in keys {
                    let value = self.staged.get(&key).await?;
                    changed.entry(key).or_insert(value);
                }
            }
            let mut previous = BTreeMap::new();
            for key in changed.keys() {
                previous.insert(key.clone(), self.staged.get(key).await?);
            }
            let relations = self.source_relation_sets(&previous, None).await?;
            Ok::<_, Error>((changed, relations))
        }
        .await;
        self.staged.restore(after);
        let (changed, mut previous) = prior?;
        let current = self.source_relation_sets(&changed, Some(mutation)).await?;
        let mut updates = Vec::new();
        let envelope_bytes = attribution_batch(&attribution, Vec::new()).payload.len();
        let mut batch_bytes = envelope_bytes;
        let mut batch_recipients = BTreeSet::new();
        let mut reports = Vec::new();
        let revision = self
            .staged
            .get(SOURCE_REVISION_KEY)
            .await?
            .map(|bytes| sdk::wire::decode::<u64>(&bytes))
            .transpose()
            .map_err(Error::Module)?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| Error::Module("pages: attribution revision exhausted".into()))?;
        for (key, relations) in current {
            let prior = previous
                .remove(&key)
                .expect("same source keys in both snapshots");
            let unchanged_context = !written.contains(&key) && prior == relations;
            if unchanged_context {
                continue;
            }
            let (kind, id) = source_object(&key)?;
            let recipients: BTreeSet<_> = prior
                .into_iter()
                .chain(relations.iter().cloned())
                .map(|relation| relation.recipient)
                .collect();
            let update = AttributionUpdate {
                object: ObjectRef {
                    kind: kind.into(),
                    object: id.into(),
                },
                revision,
                actor: actor_of(actor),
                relations,
                transfers: Vec::new(),
            };
            self.validate_source_envelope(&update)?;
            let update_bytes = sdk::wire::encode(&update).len();
            let additional_recipients = recipients.difference(&batch_recipients).count();
            let estimated_reads =
                updates.len() + 1 + batch_recipients.len() + additional_recipients;
            let projected_bytes = batch_bytes + update_bytes + usize::from(!updates.is_empty());
            let exceeds_reads = estimated_reads > ATTRIBUTION_BATCH_READ_BUDGET;
            let exceeds_bytes = projected_bytes > ATTRIBUTION_BATCH_BYTES;
            let flush = !updates.is_empty() && (exceeds_reads || exceeds_bytes);
            if flush {
                push_attribution_batch(&mut reports, &attribution, std::mem::take(&mut updates))?;
                batch_recipients.clear();
                batch_bytes = envelope_bytes;
            }
            batch_bytes += update_bytes + usize::from(!updates.is_empty());
            batch_recipients.extend(recipients);
            updates.push(update);
        }
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        self.staged
            .stage(SOURCE_REVISION_KEY.to_vec(), sdk::wire::encode(&revision));
        push_attribution_batch(&mut reports, &attribution, updates)?;
        Ok(reports)
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Pages {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's REAL merkle root over all blocks, as a 32-byte state root.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire
    /// requests from committed state. read-only.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    /// Resolve the authenticated actor and mention accounts, then apply the
    /// source operation and relation reports to one reversible staged unit.
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let m = decode_msg(&msg.payload).map_err(Error::Module)?;
        let actor = self.party_of_origin(ctx).await?;
        let replay = self
            .replay_record_request(&m, &actor, &msg.payload)
            .await
            .map_err(|error| Error::Module(error.to_string()))?;
        if let Some(receipt) = replay {
            ctx.set_assigned(super::encode_assigned(&super::PageAssigned { actor }));
            ctx.set_output(sdk::wire::encode(&receipt));
            return Ok(());
        }
        self.validate_mentions(ctx, &m).await?;
        let discussion = self.discussion_effect(&m).await?;
        let output = match &m {
            super::PageMsg::CreatePage { page_id, .. } => sdk::wire::encode(page_id),
            super::PageMsg::InsertBlock { block, .. } => sdk::wire::encode(&block.id),
            super::PageMsg::AddComment { comment_id, .. } => sdk::wire::encode(comment_id),
            _ => sdk::wire::encode(&()),
        };
        let checkpoint = self.staged.checkpoint();
        let now = ctx.env().consensus_time;
        let applied = async {
            let (output, mut reports) = match &m {
                super::PageMsg::CreateRecordCollection { .. }
                | super::PageMsg::CommitRecords { .. } => {
                    let (receipt, retention) = self
                        .apply_record_op(&m, &actor, &msg.payload)
                        .await
                        .map_err(|error| Error::Module(error.to_string()))?;
                    (sdk::wire::encode(&receipt), retention)
                }
                _ => {
                    self.apply(m, &actor, now)
                        .await
                        .map_err(|error| Error::Module(error.to_string()))?;
                    (output, Vec::new())
                }
            };
            self.record_discussion_identity(&discussion);
            reports.extend(
                self.attribution_reports(&actor, &checkpoint, discussion.mutation())
                    .await?,
            );
            Ok::<_, Error>((output, reports))
        }
        .await;
        let (output, reports) = match applied {
            Ok(applied) => applied,
            Err(error) => {
                self.staged.restore(checkpoint);
                return Err(error);
            }
        };
        ctx.set_assigned(super::encode_assigned(&super::PageAssigned { actor }));
        ctx.set_output(output);
        for report in reports {
            ctx.emit_msg(report);
        }
        Ok(())
    }

    /// real async read of own store state, serving STAGED-over-committed via
    /// the overlay, so reads within a block observe this block's writes. the
    /// reserved sentinel reads as absence (it is not a block).
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(Error::Module)? {
            PageQuery::RecordCollection { page_id } => {
                let value = self
                    .record_collection(&page_id)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::RecordCollection(value)))
            }
            PageQuery::Records {
                page_id,
                after,
                limit,
            } => {
                let value = self
                    .records(&page_id, after, limit)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::Records(value)))
            }
            PageQuery::Record { page_id, record_id } => {
                let value = self
                    .record(&page_id, &record_id)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::Record(value)))
            }
            PageQuery::RecordState { page_id, key } => {
                let value = self
                    .record_state(&page_id, &key)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::RecordState(value)))
            }
            PageQuery::RecordReceipt {
                page_id,
                request_id,
            } => {
                let value = self
                    .record_receipt(&page_id, &request_id)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::RecordReceipt(value)))
            }
            PageQuery::GetPage {
                page_id,
                after,
                limit,
            } => {
                let page = if page_id.starts_with('\0') {
                    None
                } else {
                    self.load_page_page(&page_id, after, limit).await?
                };
                Ok(encode_reply(&PageReply::Page(page)))
            }
            PageQuery::GetBlock { block_id } => {
                let block = if block_id.starts_with('\0') {
                    None
                } else {
                    self.load_block(&block_id).await?
                };
                Ok(encode_reply(&PageReply::Block(block)))
            }
            PageQuery::CommentThreadHead { thread_id } => {
                let head = self
                    .load_thread(&thread_id)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?
                    .map(|thread| super::CommentThreadHead {
                        target: thread.target,
                        comment_count: thread.comment_ids.len() as u64,
                    });
                Ok(encode_reply(&PageReply::CommentThreadHead(head)))
            }
            PageQuery::CommentThread { thread_id } => {
                let view = self
                    .thread_view(&thread_id)
                    .await
                    .map_err(|error| Error::Module(error.to_string()))?;
                Ok(encode_reply(&PageReply::CommentThread(view)))
            }
            PageQuery::GetComment { comment_id } => {
                let comment = self
                    .load_comment(&comment_id)
                    .await
                    .map_err(|e| Error::Module(e.to_string()))?;
                Ok(encode_reply(&PageReply::Comment(comment)))
            }
            PageQuery::TargetThreadCount { target } => {
                let ids = self
                    .load_target_index(&target)
                    .await
                    .map_err(|e| Error::Module(e.to_string()))?;
                Ok(encode_reply(
                    &PageReply::TargetThreadCount(ids.len() as u64),
                ))
            }
            PageQuery::PageCount => {
                let index = self
                    .load_index()
                    .await
                    .map_err(|e| Error::Module(e.to_string()))?;
                Ok(encode_reply(&PageReply::PageCount(index.len() as u64)))
            }
        }
    }

    /// publish the block-height's staged records in ONE store batch: writes
    /// AND deletes (a `None` value drops a key). no-op (and no root movement)
    /// if nothing was staged. BTreeMap iteration keeps the write order
    /// deterministic across validators.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    /// discard the staged records — nothing reached the store, so `root()` is
    /// unchanged.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
