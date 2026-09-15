use super::{
    BTreeMap, Block, BlockKind, Error, MAX_BLOCK_LEN, MAX_MOVE_SUBTREE_READS, MAX_PAGE_DEPTH,
    MAX_PAGE_QUERY_BYTES, MAX_PAGE_QUERY_LIMIT, MAX_PAGE_TITLE_LEN, MAX_PAGES, MAX_TRAVERSAL_WORK,
    MerkleStore, ModuleId, PAGE_INDEX_KEY, PageBlockPage, PageError, Pages, StagedStore,
    to_page_err,
};
use crate::comment_ops::{comment_key, target_index_key, thread_key};

impl Pages {
    /// wrap the host-constructed store under module identity `id`. sync — the
    /// store arrives already opened (or already synced to a verified root).
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Self {
        Self {
            id: id.into(),
            staged: StagedStore::new(store),
            attribution: None,
            identity: None,
            files: None,
        }
    }

    /// Publish every changed block and comment relation set to attribution.
    pub fn with_attribution(mut self, attribution: impl Into<ModuleId>) -> Self {
        self.attribution = Some(attribution.into());
        self
    }

    pub fn with_identity(mut self, identity: impl Into<ModuleId>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Retain committed artifact snapshots under immutable receipt-owned keys.
    /// Artifact-bearing commits reject when this cross-module edge is absent.
    pub fn with_files(mut self, files: impl Into<ModuleId>) -> Self {
        self.files = Some(files.into());
        self
    }

    /// read raw bytes for `key` through the staged overlay: a staged write
    /// shadows committed state, and a staged DELETE reads as absence.
    pub(super) async fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.staged.get(key).await.expect("get failed")
    }

    /// load one block (`None` == absent), through the staged-over-committed
    /// overlay. a decode failure is corruption, surfaced as an error.
    pub(super) async fn load_block(&self, block_id: &str) -> Result<Option<Block>, Error> {
        match self.get(block_id.as_bytes()).await {
            Some(b) => Ok(Some(
                serde_json::from_slice(&b).map_err(|e| Error::Module(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage serialized `bytes` under logical `key` for this block-height
    /// WITHOUT committing. rejects a value over [`MAX_BLOCK_LEN`] BEFORE
    /// staging — the shared poison-pill guard for block records AND the
    /// enumeration index value.
    pub(super) fn stage(&mut self, key: &str, bytes: Vec<u8>) -> Result<(), PageError> {
        if bytes.len() > MAX_BLOCK_LEN {
            return Err(PageError::BlockTooLarge);
        }
        self.staged.stage(key.as_bytes().to_vec(), bytes);
        Ok(())
    }

    /// stage one block record. every write path funnels here, so this is where
    /// [`MAX_PAGE_TITLE_LEN`] binds: create, nested-page insert and rename all
    /// set a title through this one guard.
    pub(super) fn store_block(&mut self, block: &Block) -> Result<(), PageError> {
        let over_title_cap = block.kind == BlockKind::Page && block.text.len() > MAX_PAGE_TITLE_LEN;
        if over_title_cap {
            return Err(PageError::TitleTooLarge);
        }
        let bytes = serde_json::to_vec(block).expect("Block is always serializable");
        self.stage(&block.id, bytes)
    }

    /// stage a DELETE of `block_id` — reads see absence at once; the key is
    /// dropped from the store (and the root) at `commit_block`.
    pub(super) fn delete_block(&mut self, block_id: &str) {
        self.staged.delete(block_id.as_bytes().to_vec());
    }

    /// Validate and collect a whole subtree without staging a mutation. Every
    /// block, target comment index, and referenced thread counts against the
    /// same local budget, so a later delete cannot outrun the wasm host.
    pub(super) async fn preflight_subtree_removal(
        &self,
        root: Block,
    ) -> Result<Vec<Block>, PageError> {
        self.preflight_removal_with_budget(root, &mut 0).await
    }

    /// Managed batches share ONE removal budget across all deleted documents.
    pub(super) async fn preflight_removal_with_budget(
        &self,
        root: Block,
        reads: &mut usize,
    ) -> Result<Vec<Block>, PageError> {
        let mut frontier = vec![root];
        let mut blocks = Vec::new();
        while !frontier.is_empty() {
            // A guest resolves all missing records in a frontier with one
            // replay. Sequential reads would repeatedly decode the entire
            // already-visited tree and exhaust fuel on valid wide subtrees.
            let mut keys = Vec::new();
            for block in &frontier {
                take_traversal_work(reads, 1, PageError::RemoveSubtreeTooLarge)?;
                take_traversal_work(
                    reads,
                    block.children.len(),
                    PageError::RemoveSubtreeTooLarge,
                )?;
                keys.push(target_index_key(&block.id).into_bytes());
                keys.extend(block.children.iter().map(|child| child.as_bytes().to_vec()));
            }
            self.staged.prefetch(&keys).await.map_err(to_page_err)?;
            let mut thread_ids = Vec::new();
            for block in &frontier {
                let ids = self.load_target_index(&block.id).await?;
                take_traversal_work(reads, ids.len(), PageError::RemoveSubtreeTooLarge)?;
                thread_ids.extend(ids);
            }
            let keys: Vec<_> = thread_ids
                .iter()
                .map(|id| thread_key(id).into_bytes())
                .collect();
            self.staged.prefetch(&keys).await.map_err(to_page_err)?;
            let mut comment_keys = Vec::new();
            for thread_id in thread_ids {
                let Some(thread) = self.load_thread(&thread_id).await? else {
                    continue;
                };
                take_traversal_work(
                    reads,
                    thread.comment_ids.len(),
                    PageError::RemoveSubtreeTooLarge,
                )?;
                comment_keys.extend(
                    thread
                        .comment_ids
                        .iter()
                        .map(|id| comment_key(id).into_bytes()),
                );
            }
            self.staged
                .prefetch(&comment_keys)
                .await
                .map_err(to_page_err)?;
            let mut next = Vec::new();
            for block in &frontier {
                for child in &block.children {
                    next.push(self.require_block(child, PageError::Corrupt).await?);
                }
            }
            blocks.append(&mut frontier);
            frontier = next;
        }
        Ok(blocks)
    }

    /// Stage a subtree plan that already passed [`Self::preflight_subtree_removal`].
    pub(super) async fn delete_subtree(&mut self, blocks: Vec<Block>) -> Result<(), PageError> {
        let mut removed_pages = Vec::new();
        for block in blocks {
            self.purge_comments_for_target(&block.id).await?;
            if block.kind == BlockKind::Page {
                removed_pages.push(block.id.clone());
            }
            self.delete_block(&block.id);
        }
        if !removed_pages.is_empty() {
            let mut index = self.load_index().await.map_err(to_page_err)?;
            for page_id in removed_pages {
                index.remove(&page_id);
            }
            self.stage_index(&index)?;
        }
        Ok(())
    }

    /// load the enumeration index — page id → containing page — through the
    /// staged-over-committed overlay. absent reads as the empty map; a decode
    /// failure is corruption. `BTreeMap` serializes with SORTED keys, so the
    /// bytes are canonical and every validator commits the same index root.
    pub(super) async fn load_index(&self) -> Result<BTreeMap<String, Option<String>>, Error> {
        match self.get(PAGE_INDEX_KEY.as_bytes()).await {
            Some(b) => serde_json::from_slice(&b).map_err(|e| Error::Module(e.to_string())),
            None => Ok(BTreeMap::new()),
        }
    }

    /// re-stage the whole index map (canonical serialization).
    pub(super) fn stage_index(
        &mut self,
        index: &BTreeMap<String, Option<String>>,
    ) -> Result<(), PageError> {
        let bytes = serde_json::to_vec(index).expect("index is always serializable");
        self.stage(PAGE_INDEX_KEY, bytes)
    }

    /// add `page_id -> parent` to the index if absent (idempotent create keeps
    /// the existing entry, so re-create never re-nests).
    pub(super) async fn index_add(
        &mut self,
        page_id: &str,
        parent: Option<String>,
    ) -> Result<(), PageError> {
        let mut index = self.load_index().await.map_err(to_page_err)?;
        if !index.contains_key(page_id) {
            if index.len() >= MAX_PAGES {
                return Err(PageError::TooManyPages);
            }
            index.insert(page_id.to_string(), parent);
            self.stage_index(&index)?;
        }
        Ok(())
    }

    /// update one existing page's containing-page relation.
    pub(super) async fn index_set_parent(
        &mut self,
        page_id: &str,
        parent: Option<String>,
    ) -> Result<(), PageError> {
        let mut index = self.load_index().await.map_err(to_page_err)?;
        if !index.contains_key(page_id) {
            return Err(PageError::Corrupt);
        }
        index.insert(page_id.to_string(), parent);
        self.stage_index(&index)
    }

    /// load a block that MUST exist (`missing` names the error when it does
    /// not) — the shared shape of every "look up then edit" op.
    pub(super) async fn require_block(
        &self,
        block_id: &str,
        missing: PageError,
    ) -> Result<Block, PageError> {
        self.load_block(block_id)
            .await
            .map_err(to_page_err)?
            .ok_or(missing)
    }

    /// walk parent pointers from `start` to the top-level page, erroring with
    /// [`PageError::CycleMove`] if `forbidden` appears on the path (that would
    /// reparent a block inside its own subtree). The local traversal cap makes
    /// native and wasm reject before the host's store-read ceiling.
    pub(super) async fn ancestry_excludes(
        &self,
        start: &str,
        forbidden: &str,
    ) -> Result<(), PageError> {
        let mut cur = start.to_string();
        for _ in 0..MAX_TRAVERSAL_WORK {
            if cur == forbidden {
                return Err(PageError::CycleMove);
            }
            let blk = self.require_block(&cur, PageError::Corrupt).await?;
            match blk.parent {
                Some(p) => cur = p,
                None => return Ok(()),
            }
        }
        Err(PageError::MoveAncestryTooDeep)
    }

    /// Depth inside the block's own document. A nested `Page` is depth zero
    /// for its document; its placement depth is derived from its parent.
    pub(super) async fn page_depth(&self, block: &Block) -> Result<usize, PageError> {
        self.page_depth_excluding(block, None).await
    }

    /// The same bounded document walk also proves a non-page move does not
    /// place a block below its own descendant.
    pub(super) async fn page_depth_excluding(
        &self,
        block: &Block,
        forbidden: Option<&str>,
    ) -> Result<usize, PageError> {
        let page_id = block.page.clone();
        let mut current = block.clone();
        for depth in 0..=MAX_PAGE_DEPTH {
            if forbidden == Some(current.id.as_str()) {
                return Err(PageError::CycleMove);
            }
            let is_root = current.id == page_id;
            if is_root {
                return if current.kind == BlockKind::Page {
                    Ok(depth)
                } else {
                    Err(PageError::Corrupt)
                };
            }
            let belongs_to_document = current.kind != BlockKind::Page && current.page == page_id;
            if !belongs_to_document {
                return Err(PageError::Corrupt);
            }
            let parent_id = current.parent.as_deref().ok_or(PageError::Corrupt)?;
            let parent = self.require_block(parent_id, PageError::Corrupt).await?;
            let parent_lists_child = parent.children.iter().any(|id| id == &current.id);
            if !parent_lists_child {
                return Err(PageError::Corrupt);
            }
            current = parent;
        }
        Err(PageError::Corrupt)
    }

    /// Prove that deepening a non-page subtree keeps its deepest block inside
    /// the target document's depth budget. Nested pages are leaves here.
    pub(super) async fn ensure_subtree_fits(
        &self,
        root: &Block,
        max_height: usize,
    ) -> Result<(), PageError> {
        if root.kind == BlockKind::Page {
            return Err(PageError::Corrupt);
        }
        let mut reads = 0_usize;
        let mut stack: Vec<_> = root
            .children
            .iter()
            .rev()
            .map(|id| (id.clone(), root.id.clone(), 1_usize))
            .collect();
        while let Some((block_id, parent_id, height)) = stack.pop() {
            if height > max_height {
                return Err(PageError::PageTooDeep);
            }
            if reads >= MAX_MOVE_SUBTREE_READS {
                return Err(PageError::MoveSubtreeTooLarge);
            }
            reads += 1;
            let child = self.require_block(&block_id, PageError::Corrupt).await?;
            let parent_matches = child.parent.as_deref() == Some(parent_id.as_str());
            let document_matches = if child.kind == BlockKind::Page {
                child.page == child.id
            } else {
                child.page == root.page
            };
            if !parent_matches || !document_matches {
                return Err(PageError::Corrupt);
            }
            if child.kind == BlockKind::Page {
                continue;
            }
            stack.extend(
                child
                    .children
                    .iter()
                    .rev()
                    .map(|id| (id.clone(), child.id.clone(), height + 1)),
            );
        }
        Ok(())
    }

    /// Read one bounded slice of a page's PREORDER traversal through the
    /// staged overlay. A nested Page is returned as one block in its parent
    /// document; its descendants belong to the nested document.
    pub(super) async fn load_page_page(
        &self,
        page_id: &str,
        after: Option<String>,
        limit: u16,
    ) -> Result<Option<PageBlockPage>, Error> {
        let mut reads = 0_usize;
        let root = match self.query_block(page_id, &mut reads).await? {
            Some(b) if b.kind == BlockKind::Page => b,
            _ => return Ok(None),
        };
        let root_id = root.id.clone();
        let mut current = match after {
            Some(cursor) => {
                if cursor.starts_with('\0') {
                    return Err(invalid_page_cursor());
                }
                let block = self
                    .query_block(&cursor, &mut reads)
                    .await?
                    .ok_or_else(invalid_page_cursor)?;
                self.validate_page_cursor(&root_id, &block, &mut reads)
                    .await?;
                self.following_page_block(&root_id, &block, &mut reads)
                    .await?
            }
            None => Some(root),
        };
        let limit = page_query_limit(limit);
        let mut blocks = Vec::with_capacity(limit);
        let mut spent = 0_usize;
        while blocks.len() < limit {
            let Some(block) = current.take() else {
                break;
            };
            let cost = encoded_len(&block);
            if cost > MAX_PAGE_QUERY_BYTES {
                return Err(corrupt());
            }
            if spent.saturating_add(cost) > MAX_PAGE_QUERY_BYTES {
                current = Some(block);
                break;
            }
            spent += cost;
            current = self
                .following_page_block(&root_id, &block, &mut reads)
                .await?;
            blocks.push(block);
        }
        let next_after = current
            .as_ref()
            .and_then(|_| blocks.last().map(|block| block.id.clone()));
        Ok(Some(PageBlockPage { blocks, next_after }))
    }

    async fn validate_page_cursor(
        &self,
        root_id: &str,
        cursor: &Block,
        reads: &mut usize,
    ) -> Result<(), Error> {
        if cursor.id == root_id {
            return Ok(());
        }
        let parent_id = match cursor.parent.as_deref() {
            Some(parent_id) => parent_id,
            None if cursor.kind == BlockKind::Page => return Err(invalid_page_cursor()),
            None => return Err(corrupt()),
        };
        let parent = self
            .query_block(parent_id, reads)
            .await?
            .ok_or_else(corrupt)?;
        if !parent.children.iter().any(|id| id == &cursor.id) {
            return Err(corrupt());
        }
        let belongs_to_page = if cursor.kind == BlockKind::Page {
            parent.page == root_id
        } else {
            cursor.page == root_id && parent.page == root_id
        };
        if belongs_to_page {
            Ok(())
        } else {
            Err(invalid_page_cursor())
        }
    }

    async fn following_page_block(
        &self,
        root_id: &str,
        current: &Block,
        reads: &mut usize,
    ) -> Result<Option<Block>, Error> {
        let may_descend = current.kind != BlockKind::Page || current.id == root_id;
        let child_id = if may_descend {
            current.children.first()
        } else {
            None
        };
        if let Some(child_id) = child_id {
            return self
                .query_block(child_id, reads)
                .await?
                .ok_or_else(corrupt)
                .map(Some);
        }
        if current.id == root_id {
            return Ok(None);
        }

        let mut child_id = current.id.clone();
        let mut parent_id = current.parent.clone();
        for _ in 0..MAX_PAGE_DEPTH {
            let Some(id) = parent_id else {
                return if child_id == root_id {
                    Ok(None)
                } else {
                    Err(corrupt())
                };
            };
            let parent = self.query_block(&id, reads).await?.ok_or_else(corrupt)?;
            let child_index = parent
                .children
                .iter()
                .position(|id| id == &child_id)
                .ok_or_else(corrupt)?;
            if let Some(sibling_id) = parent.children.get(child_index + 1) {
                return self
                    .query_block(sibling_id, reads)
                    .await?
                    .ok_or_else(corrupt)
                    .map(Some);
            }
            if parent.id == root_id {
                return Ok(None);
            }
            child_id = parent.id;
            parent_id = parent.parent;
        }
        Err(corrupt())
    }

    async fn query_block(&self, block_id: &str, reads: &mut usize) -> Result<Option<Block>, Error> {
        if *reads >= MAX_TRAVERSAL_WORK {
            return Err(page_traversal_too_deep());
        }
        *reads += 1;
        self.load_block(block_id).await
    }
}

fn take_traversal_work(work: &mut usize, count: usize, error: PageError) -> Result<(), PageError> {
    let Some(next) = work.checked_add(count) else {
        return Err(error);
    };
    let exceeds_budget = next > MAX_TRAVERSAL_WORK;
    if exceeds_budget {
        return Err(error);
    }
    *work = next;
    Ok(())
}

fn page_query_limit(limit: u16) -> usize {
    usize::from(if limit == 0 {
        MAX_PAGE_QUERY_LIMIT
    } else {
        limit.min(MAX_PAGE_QUERY_LIMIT)
    })
}

fn invalid_page_cursor() -> Error {
    Error::Module(PageError::InvalidPageCursor.to_string())
}

fn corrupt() -> Error {
    Error::Module(PageError::Corrupt.to_string())
}

fn page_traversal_too_deep() -> Error {
    Error::Module(PageError::PageTraversalTooDeep.to_string())
}

fn encoded_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .expect("page query records are serializable")
        .len()
}
