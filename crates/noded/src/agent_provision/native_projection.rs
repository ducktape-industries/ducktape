//! A retained history root must not retain an ordinary workspace, even through
//! a snapshot parent. Resolve projection receipts by committed height AND the
//! complete expected tree, never by checking only the history subtree.
use super::*;
use duckfs_core::objects::{EntryKind, TreeEntry, TreeObj};
use duckfs_core::{EntryInfo, EntryKindWire};

pub(super) async fn publish(
    node: &NodeLink,
    candidate: &str,
    config: &ConversationView,
    jsonl: &str,
) -> Result<String, String> {
    let message = duckfs_core::FilesMsg::ProjectSnapshot {
        snapshot: candidate.into(),
        path: config.history_prefix.clone(),
    };
    let height = node
        .submit("files", &duckfs_core::encode_msg(&message))
        .await?;
    let node = node.clone();
    let config = config.clone();
    let jsonl = jsonl.to_string();
    tokio::task::spawn_blocking(move || resolve(&node.files(), height, &config, &jsonl))
        .await
        .map_err(|_| "native projection verification task panicked".to_string())?
}

fn expected_tree(config: &ConversationView, jsonl: &str) -> (String, BTreeMap<String, EntryInfo>) {
    let path = format!("{}/{}", config.history_prefix, config.session_path);
    let chunks = duckfs_client::chunk::chunk_ids(jsonl.as_bytes());
    let file = duckfs_client::chunk::file_object_id(jsonl.len() as u64, &chunks, &BTreeMap::new());
    let mut entry = TreeEntry {
        kind: EntryKind::File,
        id: file,
        exec: false,
        size: jsonl.len() as u64,
    };
    let mut entries = BTreeMap::new();
    let mut segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    while let Some(name) = segments.pop() {
        let path = format!(
            "/{}",
            segments
                .iter()
                .copied()
                .chain(std::iter::once(name))
                .collect::<Vec<_>>()
                .join("/")
        );
        let kind = match entry.kind {
            EntryKind::File => EntryKindWire::File,
            EntryKind::Dir => EntryKindWire::Dir,
            EntryKind::Symlink => unreachable!("native projections contain no symlinks"),
        };
        entries.insert(
            path.clone(),
            EntryInfo {
                path,
                kind,
                size: entry.size,
                exec: false,
                object: duckfs_core::to_hex(&entry.id),
                meta: BTreeMap::new(),
            },
        );
        let tree = TreeObj {
            entries: BTreeMap::from([(name.to_string(), entry)]),
        };
        entry = TreeEntry {
            kind: EntryKind::Dir,
            id: duckfs_core::objects::object_id(duckfs_core::Kind::Tree, &tree.encode()),
            exec: false,
            // Files encodes a directory's child count, not a byte size.
            size: tree.entries.len() as u64,
        };
    }
    (duckfs_core::to_hex(&entry.id), entries)
}

pub(super) fn resolve(
    api: &dyn NodeApi,
    height: u64,
    config: &ConversationView,
    jsonl: &str,
) -> Result<String, String> {
    let (root, expected) = expected_tree(config, jsonl);
    let snapshots = api
        .history(duckfs_core::MAX_PAGE)
        .map_err(|error| format!("read native projection history: {error}"))?;
    let mut matching = Vec::new();
    for snapshot in snapshots {
        let projection = snapshot.height == height
            && snapshot.parent.is_none()
            && snapshot.message.is_empty()
            && snapshot.root_tree == root;
        if !projection {
            continue;
        }
        let actual = whole_tree(api, &snapshot.id)?;
        if actual != expected {
            return Err("native projection contains unexpected paths or metadata".into());
        }
        verify_bytes(api, &snapshot.id, config, jsonl)?;
        matching.push(snapshot.id);
    }
    // Several projections in one block may be byte-equivalent. Their complete
    // root and file bytes were checked above; choose the same id on every host.
    matching.sort();
    matching.dedup();
    matching
        .into_iter()
        .next()
        .ok_or_else(|| "committed native projection could not be uniquely verified".into())
}

fn whole_tree(api: &dyn NodeApi, snapshot: &str) -> Result<BTreeMap<String, EntryInfo>, String> {
    let mut result = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let (entries, next) = api
            .find("/", Some(snapshot), after.as_deref(), duckfs_core::MAX_PAGE)
            .map_err(|error| format!("enumerate full native projection: {error}"))?;
        for entry in entries {
            if result.insert(entry.path.clone(), entry).is_some() {
                return Err("native projection repeats a path".into());
            }
        }
        let Some(next) = next else {
            return Ok(result);
        };
        let advances = after.as_ref().is_none_or(|after| next > *after);
        if !advances {
            return Err("native projection enumeration did not advance".into());
        }
        after = Some(next);
    }
}

fn verify_bytes(
    api: &dyn NodeApi,
    snapshot: &str,
    config: &ConversationView,
    jsonl: &str,
) -> Result<(), String> {
    let path = format!("{}/{}", config.history_prefix, config.session_path);
    let mut offset = 0;
    while offset < jsonl.len() {
        let (bytes, eof) = api
            .read(
                &path,
                Some(snapshot),
                offset as u64,
                duckfs_core::MAX_READ_BYTES,
            )
            .map_err(|error| format!("read native projection session: {error}"))?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| "native projection length overflow".to_string())?;
        let exact =
            !bytes.is_empty() && jsonl.as_bytes().get(offset..end) == Some(bytes.as_slice());
        let final_range = end == jsonl.len();
        if !exact || eof != final_range {
            return Err("native projection session differs from the complete checkpoint".into());
        }
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
#[path = "native_projection_tests.rs"]
mod tests;
