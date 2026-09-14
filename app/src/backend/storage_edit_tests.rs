use super::*;

/// A commit carries the snapshot its writer read, and duckfs refuses it once
/// somebody else has written that path — the CAS the upload lane and the
/// files view's own save both ride.
#[test]
fn a_retained_file_base_refuses_an_external_edit_of_the_same_path() {
    use duckfs_core::{
        Authority, Change, Content, FilesQuery, FilesReply, Fs, MemStore, ObjectStore, Refs,
    };
    let mut fs = Fs::new(MemStore::new(), Refs::default());
    let authority = Authority::External {
        key: vec![0xaa],
        account: None,
    };
    let change = |text: &str| Change::Put {
        path: "/shared/note.txt".into(),
        exec: false,
        meta: Default::default(),
        content: Content::Inline {
            b64: base64_encode(text.as_bytes()),
        },
    };
    let flush = |fs: &mut Fs<MemStore>| {
        let (refs, _, objects) = fs.commit_block().unwrap();
        for (kind, body) in objects {
            fs.store_mut().put(kind, &body).unwrap();
        }
        fs.adopt_refs(refs);
    };
    fs.commit(&authority, 1, 1, None, "A".into(), vec![change("A source")])
        .unwrap();
    flush(&mut fs);
    let base = duckfs_core::to_hex(&fs.pending_refs().head.unwrap());
    fs.commit(
        &authority,
        2,
        2,
        Some(base.clone()),
        "external B".into(),
        vec![change("B source")],
    )
    .unwrap();
    flush(&mut fs);
    let before = fs.pending_refs().clone();
    let bytes = files_commit_payload(
        Some(base.clone()),
        "save draft".into(),
        serde_json::to_value(change("unsaved A")).unwrap(),
    )
    .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        payload["commit"]["base_snapshot"], base,
        "saving must retain the read snapshot"
    );
    let changes: Vec<Change> =
        serde_json::from_value(payload["commit"]["changes"].clone()).unwrap();
    let Err(error) = fs.commit(
        &authority,
        3,
        3,
        serde_json::from_value(payload["commit"]["base_snapshot"].clone()).unwrap(),
        "draft".into(),
        changes,
    ) else {
        panic!("the original snapshot must conflict with the external edit");
    };
    assert!(error.contains("changed since base"), "{error}");
    assert_eq!(
        *fs.pending_refs(),
        before,
        "conflict cannot change the current file"
    );
    let reply = fs
        .query(FilesQuery::Read {
            path: "/shared/note.txt".into(),
            snapshot: None,
            offset: 0,
            len: 100,
        })
        .unwrap();
    let FilesReply::Read { b64, .. } = reply else {
        panic!("not a read reply")
    };
    assert_eq!(base64_decode(&b64).unwrap(), b"B source");
}


/// A send with files links each one under its message's attachments
/// directory, in a name a markdown link can carry.
#[test]
fn a_send_with_files_links_them_under_the_message() {
    assert_eq!(attachment_body("hi".into(), &[]), "hi");
    let link = |name: &str| {
        let stored = attachment_name(name);
        (
            name.to_owned(),
            format!("duck://files/shared/attachments/attach-1-2/{stored}"),
        )
    };
    let body = attachment_body(
        "hi\nthere".into(),
        &[link("my notes (v2).txt"), link("plain.png")],
    );
    assert_eq!(
        body,
        "hi\nthere\n[my_notes__v2_.txt](duck://files/shared/attachments/attach-1-2/my_notes__v2_.txt)\n[plain.png](duck://files/shared/attachments/attach-1-2/plain.png)"
    );
    // what the module makes of it: text, then one link span a line
    let blocks = ::chat::client::parse_message(&body);
    assert_eq!(blocks.len(), 4);
    let link = classify_duck_link("duck://files/shared/attachments/attach-1-2/plain.png".into());
    assert!(matches!(link.kind, DuckKind::Files), "{link:?}");
}
