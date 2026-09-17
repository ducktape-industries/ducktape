//! the crate-reset floor: the module opens over an empty dir, roots
//! deterministically over empty refs, and routes the two op lanes (json vs
//! the binary putblob frame) before any semantics exist.

mod harness;
use harness::*;
use sdk::Module as _;

#[test]
fn opens_empty_and_roots_deterministically() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let a = open_files(&d1);
    let b = open_files(&d2);
    assert_eq!(
        a.root(),
        b.root(),
        "empty refs root must be dir-independent"
    );
    assert_ne!(a.root(), sdk::StateRoot::ZERO);
}

#[test]
fn unknown_json_op_rejects_and_putblob_frame_routes() {
    futures::executor::block_on(async {
        let d = tempfile::tempdir().unwrap();
        let mut f = open_files(&d);
        let err = f
            .execute(
                &mut test_ctx(sdk::Origin::System, 1),
                &sdk::Msg {
                    target: "files".into(),
                    payload: b"{\"nope\":{}}".to_vec(),
                },
            )
            .await
            .expect_err("unknown json must reject");
        assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "codec"));

        // the frame tag must route to the binary putblob lane, not the json
        // decoder: a well-formed chunk stages cleanly (task 7) — the json decoder
        // would instead reject these raw bytes as invalid utf-8/json.
        f.execute(
            &mut test_ctx(sdk::Origin::System, 1),
            &sdk::Msg {
                target: "files".into(),
                payload: files::encode_putblob(b"chunk bytes"),
            },
        )
        .await
        .expect("the putblob frame routes to staging");
    });
}
