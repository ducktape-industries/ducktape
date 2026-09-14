//! Protected references live in an internal copy-on-write object tree, never
//! in the public filesystem or a growing refs image. Fixed-width hash shards
//! bound each directory to 256 entries. Reference records and reverse snapshot
//! counts share one root, so membership and ownership advance atomically.
//!
//! Records are ordinary empty FileObjs with a strict metadata grammar. Only
//! this catalog's walker interprets their snapshot edges; user file metadata
//! never creates a GC root.

use std::collections::BTreeMap;

use crate::codec::push_string;
use crate::objects::{EntryKind, FileObj, Kind, ObjectId, TreeEntry, object_id};
use crate::tree::{Store, TreeEdit};
use crate::{
    MAX_PIN_NAME_BYTES, MAX_WATCH_MODULE_ID_BYTES, RetentionReference, from_hex_32, to_hex,
};

/// One section segment followed by 32 two-hex-digit segments (the last is a
/// file). The root and the 32 parent directories bound traversal depth.
pub(crate) const CATALOG_DEPTH: usize = 33;

pub(crate) enum Record {
    Reference(RetentionReference),
    Membership { snapshot: ObjectId, count: u64 },
}

impl Record {
    pub(crate) fn snapshot(&self) -> ObjectId {
        match self {
            Self::Reference(reference) => {
                from_hex_32(&reference.snapshot).expect("decoded reference snapshot")
            }
            Self::Membership { snapshot, .. } => *snapshot,
        }
    }

    fn file(&self) -> FileObj {
        let meta = match self {
            Self::Reference(reference) => BTreeMap::from([
                ("snapshot".into(), reference.snapshot.clone()),
                ("revision".into(), reference.revision.to_string()),
            ]),
            Self::Membership { snapshot, count } => BTreeMap::from([
                ("snapshot".into(), to_hex(snapshot)),
                ("count".into(), count.to_string()),
            ]),
        };
        FileObj {
            size: 0,
            chunks: Vec::new(),
            meta,
        }
    }
}

pub(crate) fn decode_record(body: &[u8]) -> Result<Record, String> {
    let file = FileObj::decode(body)?;
    let valid_shape = file.size == 0 && file.chunks.is_empty() && file.meta.len() == 2;
    if !valid_shape {
        return Err("files: invalid retention record shape".into());
    }
    let snapshot = file
        .meta
        .get("snapshot")
        .ok_or_else(|| "files: retention record has no snapshot".to_string())?;
    let id = snapshot_id(snapshot)?;
    if let Some(revision) = file.meta.get("revision") {
        return Ok(Record::Reference(RetentionReference {
            snapshot: snapshot.clone(),
            revision: positive_number(revision)?,
        }));
    }
    let count = file
        .meta
        .get("count")
        .ok_or_else(|| "files: retention record has no count".to_string())?;
    Ok(Record::Membership {
        snapshot: id,
        count: positive_number(count)?,
    })
}

fn positive_number(value: &str) -> Result<u64, String> {
    let number = value
        .parse::<u64>()
        .map_err(|_| "files: invalid retention number".to_string())?;
    let canonical = number != 0 && number.to_string() == value;
    if !canonical {
        return Err("files: invalid retention number".into());
    }
    Ok(number)
}

pub(crate) fn snapshot_id(snapshot: &str) -> Result<ObjectId, String> {
    let id =
        from_hex_32(snapshot).ok_or_else(|| "files: invalid retention snapshot".to_string())?;
    if to_hex(&id) != snapshot {
        return Err("files: retention snapshot must be canonical hex".into());
    }
    Ok(id)
}

fn validate_name(module: &str, key: &str) -> Result<(), String> {
    let valid_module = !module.is_empty() && module.len() <= MAX_WATCH_MODULE_ID_BYTES;
    let valid_key = !key.is_empty() && key.len() <= MAX_PIN_NAME_BYTES;
    if !valid_module || !valid_key {
        return Err("files: invalid retention module or key".into());
    }
    Ok(())
}

fn shards(section: &str, id: &ObjectId) -> Vec<String> {
    let mut path = vec![section.to_string()];
    path.extend(id.iter().map(|byte| format!("{byte:02x}")));
    path
}

fn reference_path(module: &str, key: &str) -> Vec<String> {
    let mut bytes = Vec::new();
    push_string(&mut bytes, module);
    push_string(&mut bytes, key);
    shards("references", &object_id(Kind::Chunk, &bytes))
}

fn membership_path(id: &ObjectId) -> Vec<String> {
    shards("snapshots", id)
}

fn get_record(
    store: &Store,
    root: Option<ObjectId>,
    path: &[String],
) -> Result<Option<Record>, String> {
    let edit = TreeEdit::load(store, root);
    let Some(entry) = edit.get(store, path)? else {
        return Ok(None);
    };
    if entry.kind != EntryKind::File {
        return Err("files: retention record is not a file".into());
    }
    let Some((kind, body)) = store.get(&entry.id)? else {
        return Err("files: retention record missing".into());
    };
    if kind != Kind::File {
        return Err("files: retention record kind mismatch".into());
    }
    decode_record(&body).map(Some)
}

pub(crate) fn get(
    store: &Store,
    root: Option<ObjectId>,
    module: &str,
    key: &str,
) -> Result<Option<RetentionReference>, String> {
    validate_name(module, key)?;
    match get_record(store, root, &reference_path(module, key))? {
        None => Ok(None),
        Some(Record::Reference(reference)) => Ok(Some(reference)),
        Some(Record::Membership { .. }) => Err("files: invalid retention reference edge".into()),
    }
}

fn count(store: &Store, root: Option<ObjectId>, id: &ObjectId) -> Result<u64, String> {
    match get_record(store, root, &membership_path(id))? {
        None => Ok(0),
        Some(Record::Membership { snapshot, count }) => {
            if snapshot != *id {
                return Err("files: retention membership snapshot mismatch".into());
            }
            Ok(count)
        }
        Some(Record::Reference(_)) => Err("files: invalid retention membership edge".into()),
    }
}

pub(crate) fn contains(
    store: &Store,
    root: Option<ObjectId>,
    id: &ObjectId,
) -> Result<bool, String> {
    Ok(count(store, root, id)? != 0)
}

fn put(
    edit: &mut TreeEdit,
    store: &Store,
    path: &[String],
    record: Record,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<(), String> {
    let body = record.file().encode();
    let id = object_id(Kind::File, &body);
    edit.put(
        store,
        path,
        TreeEntry {
            kind: EntryKind::File,
            id,
            exec: false,
            size: 0,
        },
    )?;
    objects.push((Kind::File, body));
    Ok(())
}

/// Empty catalog ancestors are not user directories. Remove the exhausted
/// spine as well, so pruning an archive does not leave permanent index nodes.
fn remove(edit: &mut TreeEdit, store: &Store, path: &[String]) -> Result<(), String> {
    edit.rm(store, path)?;
    for depth in (1..path.len()).rev() {
        let parent = &path[..depth];
        let Some(entry) = edit.get(store, parent)? else {
            return Err("files: retention ancestor missing".into());
        };
        let empty_directory = entry.kind == EntryKind::Dir && entry.size == 0;
        if !empty_directory {
            break;
        }
        edit.rm(store, parent)?;
    }
    Ok(())
}

pub(crate) struct Built {
    pub root: Option<ObjectId>,
    pub objects: Vec<(Kind, Vec<u8>)>,
}

/// All fallible work is local. The caller installs this result only after its
/// own availability checks; rejected mutations cannot leak partial catalog roots.
pub(crate) fn compare_exchange(
    store: &Store,
    root: Option<ObjectId>,
    module: &str,
    key: &str,
    expected: Option<&RetentionReference>,
    replacement: Option<&RetentionReference>,
) -> Result<Built, String> {
    validate_name(module, key)?;
    for reference in [expected, replacement].into_iter().flatten() {
        snapshot_id(&reference.snapshot)?;
        if reference.revision == 0 {
            return Err("files: retention revision must be nonzero".into());
        }
    }
    let current = get(store, root, module, key)?;
    if current.as_ref() != expected {
        return Err("files: retention compare exchange mismatch".into());
    }
    if expected == replacement {
        return Ok(Built {
            root,
            objects: Vec::new(),
        });
    }
    let revision_regressed = expected
        .zip(replacement)
        .is_some_and(|(old, new)| new.revision <= old.revision);
    if revision_regressed {
        return Err("files: retention revision must advance".into());
    }
    let mut edit = TreeEdit::load(store, root);
    let mut objects = Vec::new();
    let path = reference_path(module, key);
    match replacement {
        Some(reference) => put(
            &mut edit,
            store,
            &path,
            Record::Reference(reference.clone()),
            &mut objects,
        )?,
        None => remove(&mut edit, store, &path)?,
    }
    let previous = expected.map(|r| snapshot_id(&r.snapshot)).transpose()?;
    let next = replacement.map(|r| snapshot_id(&r.snapshot)).transpose()?;
    if previous != next {
        if let Some(id) = previous {
            let remaining = count(store, root, &id)?
                .checked_sub(1)
                .ok_or_else(|| "files: retention membership underflow".to_string())?;
            let path = membership_path(&id);
            if remaining == 0 {
                remove(&mut edit, store, &path)?;
            } else {
                put(
                    &mut edit,
                    store,
                    &path,
                    Record::Membership {
                        snapshot: id,
                        count: remaining,
                    },
                    &mut objects,
                )?;
            }
        }
        if let Some(id) = next {
            let added = count(store, root, &id)?
                .checked_add(1)
                .ok_or_else(|| "files: retention membership overflow".to_string())?;
            put(
                &mut edit,
                store,
                &membership_path(&id),
                Record::Membership {
                    snapshot: id,
                    count: added,
                },
                &mut objects,
            )?;
        }
    }
    let root = edit.build(&mut objects)?;
    Ok(Built { root, objects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Authority, Change, Content, Fs, MemStore, ObjectStore, Refs};

    fn flush(fs: &mut Fs<MemStore>) {
        let Some((refs, _, objects)) = fs.commit_block() else {
            return;
        };
        for (kind, body) in objects {
            fs.store_mut().put(kind, &body).unwrap();
        }
        fs.adopt_refs(refs);
    }

    fn seeded() -> (Fs<MemStore>, RetentionReference) {
        let mut fs = Fs::new(MemStore::new(), Refs::default());
        fs.commit(
            &Authority::System,
            1,
            1,
            None,
            String::new(),
            vec![Change::Put {
                path: "/shared/original".into(),
                exec: false,
                meta: BTreeMap::new(),
                content: Content::Inline {
                    b64: "b3JpZ2luYWw=".into(),
                },
            }],
        )
        .unwrap();
        flush(&mut fs);
        let reference = RetentionReference {
            snapshot: to_hex(&fs.refs().head.unwrap()),
            revision: 1,
        };
        (fs, reference)
    }

    #[test]
    fn record_decoder_rejects_noncanonical_numbers_and_shapes() {
        let reference = RetentionReference {
            snapshot: "ab".repeat(32),
            revision: 1,
        };
        let file = Record::Reference(reference).file();
        for revision in ["0", "01", "-1", "18446744073709551616"] {
            let mut invalid = file.clone();
            invalid.meta.insert("revision".into(), revision.into());
            assert!(decode_record(&invalid.encode()).is_err());
        }
        let mut invalid = file.clone();
        invalid.meta.insert("snapshot".into(), "AB".repeat(32));
        assert!(decode_record(&invalid.encode()).is_err());
        invalid = file.clone();
        invalid.size = 1;
        assert!(decode_record(&invalid.encode()).is_err());
        invalid = file;
        invalid.chunks.push([1; 32]);
        assert!(decode_record(&invalid.encode()).is_err());
    }

    #[test]
    fn catalog_read_budget_rejection_and_block_abort_preserve_the_previous_root() {
        let (mut fs, first) = seeded();
        let owner = Authority::Module("owner".into());
        fs.compare_exchange_retention(&owner, 2, "head".into(), None, Some(first.clone()))
            .unwrap();
        flush(&mut fs);
        let before = fs.refs().clone();
        let second = RetentionReference {
            revision: 2,
            ..first.clone()
        };
        fs.set_object_read_budget_for_tests(1);
        let error = fs
            .compare_exchange_retention(
                &owner,
                3,
                "head".into(),
                Some(first.clone()),
                Some(second.clone()),
            )
            .unwrap_err();
        assert!(error.contains("object-read budget"), "{error}");
        assert_eq!(fs.pending_refs(), &before);
        fs.abort_block();
        assert_eq!(fs.refs(), &before);
        fs.set_object_read_budget_for_tests(crate::MAX_OBJECT_READS_PER_OP);
        fs.compare_exchange_retention(&owner, 3, "head".into(), Some(first), Some(second))
            .unwrap();
        assert_ne!(fs.pending_refs().retention_root, before.retention_root);
        fs.abort_block();
        assert_eq!(fs.refs(), &before);
        assert!(fs.missing_objects(256).unwrap().is_empty());
    }

    #[test]
    fn ordinary_record_bytes_neither_create_nor_hide_retention_edges() {
        let (mut fs, reference) = seeded();
        fs.set_history_window_for_tests(1);
        let owner = Authority::Module("owner".into());
        fs.compare_exchange_retention(&owner, 2, "head".into(), None, Some(reference.clone()))
            .unwrap();
        let base = fs.committed_head_for_test();
        fs.commit(
            &Authority::System,
            2,
            2,
            base,
            String::new(),
            vec![
                Change::Rm {
                    path: "/shared/original".into(),
                },
                Change::Put {
                    path: "/shared/metadata".into(),
                    exec: false,
                    meta: Record::Reference(reference.clone()).file().meta,
                    content: Content::Inline { b64: String::new() },
                },
            ],
        )
        .unwrap();
        flush(&mut fs);
        let snapshot = snapshot_id(&reference.snapshot).unwrap();
        fs.gc().unwrap();
        assert!(
            fs.store_mut().has(&snapshot),
            "ordinary file visitation must not mask a catalog edge"
        );
        fs.compare_exchange_retention(&owner, 3, "head".into(), Some(reference), None)
            .unwrap();
        flush(&mut fs);
        fs.gc().unwrap();
        assert!(
            !fs.store_mut().has(&snapshot),
            "ordinary metadata must not retain its snapshot string"
        );
        assert!(fs.refs().retention_root.is_none());
    }
}
