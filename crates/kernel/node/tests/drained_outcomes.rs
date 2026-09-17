//! [`OrderedNode::take_drained`] correlates a caller's own submits with their
//! finalized outcomes: the [`FrameId`] returned by submit reappears with a
//! disposition once the order delivers the frame. this is the seam an app
//! surface holds a reply open on ("your op landed / was rejected / resubmit"),
//! so the properties pinned here are id round-trip, per-frame disposition, and
//! take-clears semantics.

use commonware_cryptography::{Signer as _, ed25519};
use directory::Directory;
use directory::{DirMsg, encode_msg};
use host::Host;
use node::{Disposition, OrderedNode, RoundOrderer};
use sdk::Msg;

fn dir_set(k: &str, v: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: k.into(),
            value: v.into(),
        }),
    }
}

fn genesis() -> Host {
    Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis")
}

#[test]
fn drained_outcomes_correlate_submits_with_dispositions() {
    futures::executor::block_on(async {
        let mut node = OrderedNode::new(genesis(), RoundOrderer::new());
        let signer = ed25519::PrivateKey::from_seed(1);

        let ok_id = node
            .submit(&signer, 0, dir_set("k", "v"))
            .await
            .expect("submit ok op");
        // a valid target whose payload the module cannot decode: finalizes,
        // then rejects deterministically — the no-op disposition.
        let bad_id = node
            .submit(
                &signer,
                1,
                Msg {
                    target: "directory".into(),
                    payload: b"not-json".to_vec(),
                },
            )
            .await
            .expect("submit rejectable op");
        assert_ne!(ok_id, bad_id, "distinct frames get distinct ids");

        // both ops flush into ONE batch -> ONE block at ONE height with TWO
        // member outcomes sharing the block root-hash.
        node.flush_batch().await.expect("flush");
        node.drain_delivered().await.expect("drain");
        let drained = node.take_drained();
        assert_eq!(drained.len(), 2, "every finalized frame gets an outcome");

        let ok_frame = drained
            .iter()
            .find(|d| d.id == ok_id)
            .expect("ok id drained");
        let bad_frame = drained
            .iter()
            .find(|d| d.id == bad_id)
            .expect("bad id drained");
        assert_eq!(ok_frame.disposition, Disposition::Applied);
        assert_eq!(bad_frame.disposition, Disposition::Rejected);

        // the applied frame carries its decoded op: authenticated authorship,
        // the root msg, and the block's dispatch trace.
        let op = ok_frame.op.as_ref().expect("applied frame carries its op");
        assert_eq!(op.target, "directory");
        assert_eq!(
            op.origin,
            sdk::Origin::External(signer.public_key().as_ref().to_vec()),
            "origin is the frame's verified signer"
        );
        assert!(
            !op.dispatches.is_empty(),
            "an applied op leaves a dispatch trace"
        );
        // the rejected frame decoded fine (the MODULE refused it), so its op
        // contents are still known — but a deterministic no-op leaves no trace.
        let bad_op = bad_frame
            .op
            .as_ref()
            .expect("a decoded-then-rejected frame still carries its op");
        assert!(bad_op.dispatches.is_empty(), "a rejected op leaves no trace");
        // per-frame boundary capture: the reject rolled back, so both frames
        // settled at the same composed root-hash the node now reports.
        assert_eq!(ok_frame.root_hash, node.root_hash());
        assert_eq!(bad_frame.root_hash, node.root_hash());

        // node-local observability: the rejected frame carries the MODULE's
        // framed refusal, `<class>: <sentence>` (a submitter's held reply
        // splits it into the module's own token and sentence), while an
        // applied frame carries none. framed = UNWRAPPED from the host's
        // `Module(..)` Display, so the class is the first thing on the line.
        assert!(
            ok_frame.reason.is_none(),
            "an applied frame carries no reason"
        );
        let module_err = directory::decode_msg(b"not-json").expect_err("not-json is undecodable");
        assert_eq!(
            bad_frame.reason.as_deref(),
            Some(format!("codec: {module_err}").as_str()),
            "a module-rejected frame carries the module's framed refusal: its class, then its sentence"
        );
        let reason = bad_frame.reason.as_deref().expect("reason present");
        assert!(
            !reason.starts_with("op rejected") && !reason.contains("Module("),
            "the reason is the module string unwrapped, not a SubmitError/Error wrapper: {reason}"
        );

        assert!(node.take_drained().is_empty(), "take clears the queue");
    });
}

#[test]
fn frames_past_the_ceiling_drain_as_discarded() {
    futures::executor::block_on(async {
        let mut node = OrderedNode::new(genesis(), RoundOrderer::new());
        let signer = ed25519::PrivateKey::from_seed(2);

        let id = node
            .submit(&signer, 0, dir_set("k", "v"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.set_view_ceiling(0); // every view >= 0 is past the cutover.

        node.drain_delivered().await.expect("drain");
        let drained = node.take_drained();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, id);
        assert_eq!(drained[0].disposition, Disposition::Discarded);
        assert!(
            drained[0].op.is_none(),
            "discarded at the ceiling — dropped before decode, no op contents"
        );
        // the engine clock advances (the view was agreed), but the finalized
        // STATE boundary does not: a discard is never journaled, so a
        // boundary that included it would claim a height recovery cannot
        // reproduce — and right after a cutover it would collide with the
        // new epoch's first height.
        assert_eq!(
            node.last_engine_view(),
            Some(0),
            "the engine clock advances"
        );
        assert!(
            node.finalized().is_none(),
            "no journaled block, no state boundary"
        );
    });
}
