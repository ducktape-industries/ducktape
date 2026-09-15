//! Generic managed data plus ordinary Page documents in the same staged store.
//! The root is a top-level Page, and record documents cannot contain subpages.
//! Consequently every protected block's `page` is either the collection root or
//! a managed record ID: no ancestor walk or unbounded reverse index is needed.
use super::{
    Block, BlockKind, MAX_BLOCK_ID_BYTES, MAX_RECORD_ARTIFACTS, MAX_RECORD_BATCH_BYTES,
    MAX_RECORD_CHANGES, MAX_RECORD_DATA_BYTES, MAX_RECORD_DOCUMENT_BLOCKS,
    MAX_RECORD_METADATA_BYTES, MAX_RECORD_QUERY_LIMIT, MAX_RECORD_STATE_KEYS,
    MAX_RECORD_STATE_VALUE_BYTES, MAX_RECORDS_PER_COLLECTION, ManagedRecord, NewBlock, PageError,
    PageMsg, Pages, Party, RecordChange, RecordCollection, RecordDocument, RecordPage,
    RecordReceipt, RecordState, RecordStateChange, id_is_index_safe, to_page_err,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Serialize, Deserialize)]
struct StoredCollection {
    header: RecordCollection,
    record_ids: BTreeSet<String>,
    state_keys: BTreeSet<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    page_id: String,
    record: ManagedRecord,
}

pub(super) fn collection_key(page_id: &str) -> String {
    format!("\0record-collection:{page_id}")
}

fn record_key(record_id: &str) -> String {
    format!("\0record:{record_id}")
}

fn record_state_key(page_id: &str, key: &str) -> String {
    format!(
        "\0record-state:{}",
        serde_json::to_string(&(page_id, key)).expect("string tuple serializes")
    )
}

pub(crate) fn artifact_retention_key(page_id: &str, request_id: &str, index: usize) -> String {
    let digest = Sha256::digest(sdk::wire::encode(&(page_id, request_id, index)));
    format!("records:{}", files::to_hex(&digest))
}

pub(crate) fn receipt_key(page_id: &str, request_id: &str) -> String {
    format!(
        "\0record-receipt:{}",
        serde_json::to_string(&(page_id, request_id)).expect("string tuple serializes")
    )
}

fn valid_id(id: &str) -> Result<(), PageError> {
    let valid = !id.is_empty() && id.len() <= MAX_BLOCK_ID_BYTES && id_is_index_safe(id);
    if !valid {
        return Err(PageError::InvalidRecordBatch);
    }
    Ok(())
}

pub(super) fn request_ids(msg: &PageMsg) -> Option<(&str, &str)> {
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
pub(crate) fn document_ops(
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

impl Pages {
    async fn load_record_state<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, PageError> {
        self.staged
            .get(key.as_bytes())
            .await
            .map_err(to_page_err)?
            .map(|bytes| sdk::wire::decode(&bytes).map_err(|_| PageError::Corrupt))
            .transpose()
    }

    pub(super) async fn record_collection(
        &self,
        page_id: &str,
    ) -> Result<Option<RecordCollection>, PageError> {
        Ok(self
            .load_record_state::<StoredCollection>(&collection_key(page_id))
            .await?
            .map(|state| state.header))
    }

    pub(super) async fn record(
        &self,
        page_id: &str,
        record_id: &str,
    ) -> Result<Option<ManagedRecord>, PageError> {
        let record = self
            .load_record_state::<StoredRecord>(&record_key(record_id))
            .await?;
        Ok(record
            .filter(|record| record.page_id == page_id)
            .map(|record| record.record))
    }

    pub(super) async fn record_state(
        &self,
        page_id: &str,
        key: &str,
    ) -> Result<Option<RecordState>, PageError> {
        self.load_record_state(&record_state_key(page_id, key))
            .await
    }

    pub(super) async fn record_receipt(
        &self,
        page_id: &str,
        request_id: &str,
    ) -> Result<Option<RecordReceipt>, PageError> {
        self.load_record_state(&receipt_key(page_id, request_id))
            .await
    }

    pub(super) async fn records(
        &self,
        page_id: &str,
        after: Option<String>,
        limit: u16,
    ) -> Result<Option<RecordPage>, PageError> {
        let Some(state) = self
            .load_record_state::<StoredCollection>(&collection_key(page_id))
            .await?
        else {
            return Ok(None);
        };
        let limit = match limit {
            0 => MAX_RECORD_QUERY_LIMIT,
            limit => limit.min(MAX_RECORD_QUERY_LIMIT),
        } as usize;
        let ids: Vec<_> = state
            .record_ids
            .iter()
            .filter(|id| after.as_ref().is_none_or(|after| *id > after))
            .take(limit + 1)
            .collect();
        let keys: Vec<_> = ids
            .iter()
            .take(limit)
            .map(|id| record_key(id).into_bytes())
            .collect();
        self.staged.prefetch(&keys).await.map_err(to_page_err)?;
        let mut records = Vec::new();
        for id in ids.iter().take(limit) {
            records.push(self.record(page_id, id).await?.ok_or(PageError::Corrupt)?);
        }
        let next_after =
            (ids.len() > limit).then(|| records.last().expect("nonempty page").record_id.clone());
        Ok(Some(RecordPage {
            revision: state.header.revision,
            records,
            next_after,
        }))
    }

    /// Called before mention validation and CAS, but AFTER authenticating the
    /// designated writer. A receipt never authorizes another actor's replay.
    pub(super) async fn replay_record_request(
        &self,
        msg: &PageMsg,
        actor: &Party,
        payload: &[u8],
    ) -> Result<Option<RecordReceipt>, PageError> {
        let Some((page_id, request_id)) = request_ids(msg) else {
            return Ok(None);
        };
        valid_id(page_id)?;
        valid_id(request_id)?;
        let exceeds_payload_bound = payload.len() > MAX_RECORD_BATCH_BYTES;
        if exceeds_payload_bound {
            return Err(PageError::InvalidRecordBatch);
        }
        let Some(collection) = self.record_collection(page_id).await? else {
            return Ok(None);
        };
        let authorized_writer = &collection.writer == actor;
        if !authorized_writer {
            return Err(PageError::RecordUnauthorized);
        }
        let Some(receipt) = self.record_receipt(page_id, request_id).await? else {
            return Ok(None);
        };
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let matches_payload = receipt.payload_digest == digest;
        if !matches_payload {
            return Err(PageError::RecordRequestConflict);
        }
        Ok(Some(receipt))
    }

    /// Public ordinary block operations cannot edit either half of managed
    /// state, even when sent by its writer. Comments remain ordinary discussion.
    pub(super) async fn guard_unmanaged_block(&self, block_id: &str) -> Result<(), PageError> {
        let Some(block) = self.load_block(block_id).await.map_err(to_page_err)? else {
            return Ok(());
        };
        let collection = self.record_collection(&block.page).await?;
        if collection.is_some() {
            return Err(PageError::ManagedPage);
        }
        let record = self
            .load_record_state::<StoredRecord>(&record_key(&block.page))
            .await?;
        if record.is_some() {
            return Err(PageError::ManagedPage);
        }
        Ok(())
    }

    pub(super) async fn apply_record_op(
        &mut self,
        msg: &PageMsg,
        actor: &Party,
        payload: &[u8],
    ) -> Result<(RecordReceipt, Vec<super::Msg>), PageError> {
        let (revision, metadata, artifacts) = match msg {
            PageMsg::CreateRecordCollection { page_id, .. } => (
                self.create_record_collection(page_id, actor).await?,
                None,
                Vec::new(),
            ),
            PageMsg::CommitRecords {
                metadata,
                artifacts,
                ..
            } => {
                let revision = self.commit_records(msg, actor).await?;
                (revision, metadata.clone(), artifacts.clone())
            }
            _ => return Err(PageError::InvalidRecordBatch),
        };
        let (page_id, request_id) = request_ids(msg).ok_or(PageError::InvalidRecordBatch)?;
        let receipt = RecordReceipt {
            page_id: page_id.into(),
            request_id: request_id.into(),
            revision,
            payload_digest: Sha256::digest(payload).into(),
            metadata,
            artifacts,
        };
        let retention = self.artifact_retention_messages(&receipt)?;
        self.stage(
            &receipt_key(page_id, request_id),
            sdk::wire::encode(&receipt),
        )?;
        Ok((receipt, retention))
    }

    fn artifact_retention_messages(
        &self,
        receipt: &RecordReceipt,
    ) -> Result<Vec<super::Msg>, PageError> {
        if receipt.artifacts.is_empty() {
            return Ok(Vec::new());
        }
        let Some(target) = &self.files else {
            return Err(PageError::FilesNotConfigured);
        };
        Ok(receipt
            .artifacts
            .iter()
            .enumerate()
            .map(|(index, snapshot)| super::Msg {
                target: target.clone(),
                payload: files::encode_msg(&files::FilesMsg::CompareExchangeRetention {
                    key: artifact_retention_key(&receipt.page_id, &receipt.request_id, index),
                    expected: None,
                    replacement: Some(files::RetentionReference {
                        snapshot: snapshot.clone(),
                        revision: receipt.revision,
                    }),
                }),
            })
            .collect())
    }

    async fn create_record_collection(
        &mut self,
        page_id: &str,
        actor: &Party,
    ) -> Result<u64, PageError> {
        let collection_exists = self.record_collection(page_id).await?.is_some();
        if collection_exists {
            return Err(PageError::RecordCollectionExists);
        }
        let root = self
            .require_block(page_id, PageError::BlockNotFound)
            .await?;
        let authorized_author = &root.author == actor;
        if !authorized_author {
            return Err(PageError::RecordUnauthorized);
        }
        let top_level_page = root.kind == BlockKind::Page && root.parent.is_none();
        if !top_level_page {
            return Err(PageError::InvalidRecordCollection);
        }
        let tree = self.preflight_subtree_removal(root).await?;
        let nested_pages = tree
            .iter()
            .skip(1)
            .any(|block| block.kind == BlockKind::Page);
        if nested_pages {
            return Err(PageError::InvalidRecordCollection);
        }
        let state = StoredCollection {
            header: RecordCollection {
                page_id: page_id.into(),
                writer: actor.clone(),
                revision: 0,
                record_count: 0,
            },
            record_ids: BTreeSet::new(),
            state_keys: BTreeSet::new(),
        };
        self.stage(&collection_key(page_id), sdk::wire::encode(&state))?;
        Ok(0)
    }

    async fn commit_records(&mut self, msg: &PageMsg, actor: &Party) -> Result<u64, PageError> {
        let PageMsg::CommitRecords {
            page_id,
            expected_revision,
            changes,
            state_changes,
            metadata,
            artifacts,
            ..
        } = msg
        else {
            return Err(PageError::InvalidRecordBatch);
        };
        let expected_revision = *expected_revision;
        let metadata = metadata.as_ref();
        let mut unique = BTreeSet::new();
        let bounded_artifacts = artifacts.len() <= MAX_RECORD_ARTIFACTS;
        if !bounded_artifacts {
            return Err(PageError::InvalidRecordBatch);
        }
        for snapshot in artifacts {
            let valid = files::from_hex_32(snapshot).is_some() && unique.insert(snapshot);
            if !valid {
                return Err(PageError::InvalidRecordBatch);
            }
        }
        let missing_files = !artifacts.is_empty() && self.files.is_none();
        if missing_files {
            return Err(PageError::FilesNotConfigured);
        }
        let mut state = self
            .load_record_state::<StoredCollection>(&collection_key(page_id))
            .await?
            .ok_or(PageError::RecordCollectionNotFound)?;
        let authorized_writer = &state.header.writer == actor;
        if !authorized_writer {
            return Err(PageError::RecordUnauthorized);
        }
        let matches_revision = state.header.revision == expected_revision;
        if !matches_revision {
            return Err(PageError::RecordRevisionConflict);
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or(PageError::RecordRevisionConflict)?;
        let count = changes.len() + state_changes.len();
        let has_effect = count > 0 || metadata.is_some() || !artifacts.is_empty();
        let valid_count = has_effect && count <= MAX_RECORD_CHANGES;
        let bounded_metadata = metadata
            .is_none_or(|value| sdk::wire::encode(value).len() <= MAX_RECORD_METADATA_BYTES);
        if !valid_count || !bounded_metadata {
            return Err(PageError::InvalidRecordBatch);
        }
        let mut state_named = BTreeSet::new();
        for change in state_changes {
            let key = match change {
                RecordStateChange::Put { key, value } => {
                    let bounded = sdk::wire::encode(value).len() <= MAX_RECORD_STATE_VALUE_BYTES;
                    if !bounded {
                        return Err(PageError::InvalidRecordBatch);
                    }
                    key
                }
                RecordStateChange::Delete { key } => key,
            };
            valid_id(key)?;
            let duplicate = !state_named.insert(key);
            if duplicate {
                return Err(PageError::InvalidRecordBatch);
            }
        }
        let mut named = BTreeSet::new();
        for change in changes {
            let record_id = match change {
                RecordChange::Upsert {
                    record_id,
                    data,
                    document,
                } => {
                    let bounded = sdk::wire::encode(data).len() <= MAX_RECORD_DATA_BYTES
                        && document.blocks.len() <= MAX_RECORD_DOCUMENT_BLOCKS;
                    if !bounded {
                        return Err(PageError::InvalidRecordBatch);
                    }
                    for block in &document.blocks {
                        valid_id(&block.id)?;
                        let invalid =
                            block.kind == BlockKind::Page || !named.insert(block.id.clone());
                        if invalid {
                            return Err(PageError::InvalidRecordBatch);
                        }
                    }
                    record_id
                }
                RecordChange::Delete { record_id } => record_id,
            };
            valid_id(record_id)?;
            let duplicate_id = !named.insert(record_id.clone());
            if duplicate_id {
                return Err(PageError::InvalidRecordBatch);
            }
        }
        // Resolve known document/target keys together: a wasm store guest
        // otherwise replays the entire batch once per first block read.
        let mut keys = vec![
            page_id.as_bytes().to_vec(),
            super::PAGE_INDEX_KEY.as_bytes().to_vec(),
        ];
        for id in &named {
            keys.push(id.as_bytes().to_vec());
            keys.push(crate::comment_ops::target_index_key(id).into_bytes());
        }
        self.staged.prefetch(&keys).await.map_err(to_page_err)?;
        // Reserve block, target-index, and metadata reads even for newly
        // inserted documents; comment fanout/removals spend the rest below.
        let mut work = changes.len() * (2 + MAX_RECORD_DOCUMENT_BLOCKS * 2);
        for change in changes {
            self.apply_record_change(
                page_id,
                change,
                actor,
                revision,
                &mut state.record_ids,
                &mut work,
            )
            .await?;
        }
        for change in state_changes {
            match change {
                RecordStateChange::Put { key, value } => {
                    let entry = RecordState {
                        key: key.clone(),
                        value: value.clone(),
                        revision,
                    };
                    self.stage(&record_state_key(page_id, key), sdk::wire::encode(&entry))?;
                    state.state_keys.insert(key.clone());
                }
                RecordStateChange::Delete { key } => {
                    let exists = state.state_keys.remove(key);
                    if !exists {
                        return Err(PageError::RecordStateNotFound);
                    }
                    self.staged
                        .delete(record_state_key(page_id, key).into_bytes());
                }
            }
        }
        let exceeds_state_limit = state.state_keys.len() > MAX_RECORD_STATE_KEYS;
        if exceeds_state_limit {
            return Err(PageError::TooManyRecordStateKeys);
        }
        let exceeds_record_limit = state.record_ids.len() > MAX_RECORDS_PER_COLLECTION;
        if exceeds_record_limit {
            return Err(PageError::TooManyRecords);
        }
        state.header.revision = revision;
        state.header.record_count = state.record_ids.len() as u64;
        self.stage(&collection_key(page_id), sdk::wire::encode(&state))?;
        Ok(revision)
    }

    async fn preflight_record_document_op(
        &self,
        op: &PageMsg,
        work: &mut usize,
    ) -> Result<(), PageError> {
        match op {
            PageMsg::RemoveBlock { block_id } => {
                let block = self.require_block(block_id, PageError::Corrupt).await?;
                self.preflight_removal_with_budget(block, work).await?;
            }
            PageMsg::UpdateText { block_id, text, .. } => {
                let block = self.require_block(block_id, PageError::Corrupt).await?;
                let text_changes = block.text != *text;
                if !text_changes {
                    return Ok(());
                }
                let threads = self.load_target_index(block_id).await?;
                *work += threads.len() + 1;
                let exceeds_budget = *work > super::MAX_TRAVERSAL_WORK;
                if exceeds_budget {
                    return Err(PageError::InvalidRecordBatch);
                }
                let keys: Vec<_> = threads
                    .iter()
                    .map(|id| crate::comment_ops::thread_key(id).into_bytes())
                    .collect();
                self.staged.prefetch(&keys).await.map_err(to_page_err)?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn apply_record_change(
        &mut self,
        page_id: &str,
        change: &RecordChange,
        actor: &Party,
        revision: u64,
        ids: &mut BTreeSet<String>,
        work: &mut usize,
    ) -> Result<(), PageError> {
        match change {
            RecordChange::Upsert {
                record_id,
                data,
                document,
            } => {
                let existing = match ids.contains(record_id) {
                    true => Some(self.require_block(record_id, PageError::Corrupt).await?),
                    false => None,
                };
                let root = self.require_block(page_id, PageError::Corrupt).await?;
                let ops = document_ops(
                    page_id,
                    record_id,
                    existing.as_ref(),
                    document,
                    root.children.last().cloned(),
                );
                for op in ops {
                    self.preflight_record_document_op(&op, work).await?;
                    self.apply_block_op(op, actor).await?;
                }
                let record = StoredRecord {
                    page_id: page_id.into(),
                    record: ManagedRecord {
                        record_id: record_id.clone(),
                        data: data.clone(),
                        revision,
                    },
                };
                self.stage(&record_key(record_id), sdk::wire::encode(&record))?;
                ids.insert(record_id.clone());
                Ok(())
            }
            RecordChange::Delete { record_id } => {
                let record_exists = ids.remove(record_id);
                if !record_exists {
                    return Err(PageError::RecordNotFound);
                }
                let block = self.require_block(record_id, PageError::Corrupt).await?;
                self.preflight_removal_with_budget(block, work).await?;
                self.apply_block_op(
                    PageMsg::RemoveBlock {
                        block_id: record_id.clone(),
                    },
                    actor,
                )
                .await?;
                self.staged.delete(record_key(record_id).into_bytes());
                Ok(())
            }
        }
    }
}
