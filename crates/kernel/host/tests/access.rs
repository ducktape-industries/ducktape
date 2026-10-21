//! The access gate: on a network that binds the access role, before an op runs the kernel asks
//! its target what the op touches, and that role checks the acting parties against the lists
//! owners set and the owners and defaults the target answers for its keys.
use std::path::Path;

use abi::role::access::{Need, Ownership, Party, Resource};
use abi::role::{self, halo, validators};
use abi::{HostOp, Message, Outcome, Receipt, Refusal, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::{NEEDS, Step, resource_key};
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, RELAY, VALSET,
};
use host::{Block, BlockId, Founding, Genesis, Host, Layer, Limits, Submission, Tip};

const OWNER: &[u8] = b"owner";
const OTHER: &[u8] = b"other";
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>, roles: &[&str]) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn script(steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&steps)
}

fn set(key: &[u8], value: Vec<u8>) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value,
    })
}

fn probe(needs: Vec<Need>, resources: Vec<(&str, Resource)>) -> Vec<u8> {
    let resources = resources
        .into_iter()
        .map(|(key, resource)| set(&resource_key(key), abi::encode(&resource)));
    script(
        std::iter::once(set(NEEDS, abi::encode(&needs)))
            .chain(resources)
            .collect(),
    )
}

fn genesis(probed: Vec<u8>) -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new(), &[role::PROGRAMS]),
            founding("valset", VALSET, Vec::new(), &[role::VALIDATORS]),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(OWNER.to_vec(), 1u64), (OTHER.to_vec(), 2u64)]),
                &[role::IDENTITY],
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new(), &[role::BLOBS]),
            founding("awaits", AWAITS, Vec::new(), &[role::AWAITS]),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
                &[role::HALO],
            ),
            founding("access", ACCESS, Vec::new(), &[role::ACCESS]),
            founding("probe", PROBE, probed, &[]),
            founding("relay", RELAY, Vec::new(), &[]),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 64,
        time: TIME,
        member_cap: 16,
        queries: abi::role::validators::QueryPolicy::Signed,
    }
}

fn unchecked(probed: Vec<u8>) -> Genesis {
    let mut genesis = genesis(probed);
    genesis
        .programs
        .retain(|founding| founding.program != "access");
    genesis
}

async fn found(context: Ctx, dir: &Path, probed: Vec<u8>) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis(probed))
        .await
        .unwrap()
        .0
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn signed(signer: &[u8], salt: u64, target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: signer.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: Some(salt),
        target: target.to_owned(),
        payload,
        client: None,
    }
}

fn write(signer: &[u8], salt: u64, value: &[u8]) -> Submission {
    signed(
        signer,
        salt,
        "probe",
        script(vec![set(b"written", value.to_vec())]),
    )
}

fn set_list(signer: &[u8], salt: u64, key: &str, list: Option<Ownership>) -> Submission {
    let op = fixture_access::Op::SetList {
        program: "probe".into(),
        key: key.into(),
        list,
    };
    signed(signer, salt, "access", abi::encode(&op))
}

fn fill(salt: u64, program: Option<&str>) -> Submission {
    let change = fixture_programs::Change::Fill {
        role: role::ACCESS.to_owned(),
        program: program.map(str::to_owned),
    };
    signed(OWNER, salt, "programs", abi::encode(&change))
}

async fn apply(host: &mut Host<Ctx>, height: u64, submissions: Vec<Submission>) -> Vec<Receipt> {
    let block = Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    };
    host.apply(block).await.unwrap().submissions
}

fn refusal(receipt: &Receipt) -> &Refusal {
    match &receipt.outcome {
        Outcome::Rejected(refusal) => refusal,
        Outcome::Applied { .. } => panic!("applied: {receipt:?}"),
    }
}

fn applied(receipt: &Receipt) {
    assert!(
        matches!(receipt.outcome, Outcome::Applied { .. }),
        "{receipt:?}"
    );
}

fn written(host: &Host<Ctx>) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed)
        .get("probe", b"written")
        .unwrap()
}

fn listed(host: &Host<Ctx>, key: &str) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed)
        .get("access", &fixture_access::list_key("probe", key))
        .unwrap()
}

fn owned_by(number: u64) -> Resource {
    Resource::owned(Party::Account(number))
}

#[test]
fn an_owner_lists_who_writes_and_the_kernel_refuses_everyone_else_before_the_op_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let probed = probe(vec![Need::write("doc/page")], vec![("doc", owned_by(1))]);
        let mut host = found(context, dir.path(), probed).await;

        let open = apply(&mut host, 1, vec![write(OTHER, 1, b"anyone")]).await;
        applied(&open[0]);
        assert_eq!(written(&host).as_deref(), Some(b"anyone".as_slice()));

        let taken = apply(
            &mut host,
            2,
            vec![set_list(OTHER, 2, "doc", Some(Ownership::Owner))],
        )
        .await;
        assert_eq!(refusal(&taken[0]).reason, reason::UNAUTHORIZED);
        let closed = apply(
            &mut host,
            3,
            vec![set_list(OWNER, 3, "doc", Some(Ownership::Owner))],
        )
        .await;
        applied(&closed[0]);

        let refused = apply(&mut host, 4, vec![write(OTHER, 4, b"other")]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
        assert_eq!(written(&host).as_deref(), Some(b"anyone".as_slice()));
        let owned = apply(&mut host, 5, vec![write(OWNER, 5, b"owner")]).await;
        applied(&owned[0]);
        assert_eq!(written(&host).as_deref(), Some(b"owner".as_slice()));

        let listed = Ownership::Listed(vec![Party::Account(2)]);
        apply(&mut host, 6, vec![set_list(OWNER, 6, "doc", Some(listed))]).await;
        let opened = apply(&mut host, 7, vec![write(OTHER, 7, b"listed")]).await;
        applied(&opened[0]);
        assert_eq!(written(&host).as_deref(), Some(b"listed".as_slice()));
    });
}

#[test]
fn a_program_that_sends_a_message_acts_as_itself() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let probed = probe(vec![Need::write("doc")], vec![("doc", owned_by(1))]);
        let mut host = found(context, dir.path(), probed).await;
        apply(
            &mut host,
            1,
            vec![set_list(OWNER, 1, "doc", Some(Ownership::Owner))],
        )
        .await;
        let relayed = |salt| {
            let message = Message {
                target: "probe".into(),
                payload: script(vec![set(b"written", b"relayed".to_vec())]),
                reply: false,
                acting: None,
            };
            let send = fixture_relay::Script::Send(vec![message]);
            signed(OWNER, salt, "relay", abi::encode(&send))
        };

        let refused = apply(&mut host, 2, vec![relayed(2)]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
        assert_eq!(written(&host), None);

        let relay = Ownership::Listed(vec![Party::Program("relay".into())]);
        apply(&mut host, 3, vec![set_list(OWNER, 3, "doc", Some(relay))]).await;
        let sent = apply(&mut host, 4, vec![relayed(4)]).await;
        applied(&sent[0]);
        assert_eq!(written(&host).as_deref(), Some(b"relayed".as_slice()));
    });
}

#[test]
fn a_default_holds_until_the_owner_sets_a_list() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let note = Resource {
            owner: Some(Party::Account(1)),
            default: Some(Ownership::Owner),
        };
        let probed = probe(vec![Need::write("room/note")], vec![("room/note", note)]);
        let mut host = found(context, dir.path(), probed).await;

        let refused = apply(&mut host, 1, vec![write(OTHER, 1, b"other")]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
        let opened = apply(
            &mut host,
            2,
            vec![
                set_list(OWNER, 2, "room/note", Some(Ownership::Anyone)),
                write(OTHER, 3, b"other"),
            ],
        )
        .await;
        applied(&opened[0]);
        applied(&opened[1]);
        assert_eq!(
            listed(&host, "room/note"),
            Some(abi::encode(&Ownership::Anyone))
        );
    });
}

#[test]
fn a_delete_drops_the_lists_beneath_it_and_falls_with_its_op() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let probed = probe(vec![Need::delete("doc")], vec![("doc/page", owned_by(1))]);
        let mut host = found(context, dir.path(), probed).await;
        apply(
            &mut host,
            1,
            vec![set_list(OWNER, 1, "doc/page", Some(Ownership::Anyone))],
        )
        .await;
        assert!(listed(&host, "doc/page").is_some());

        let failing = signed(OWNER, 2, "probe", script(vec![Step::Fail("no".into())]));
        let failed = apply(&mut host, 2, vec![failing]).await;
        assert_eq!(refusal(&failed[0]).reason, "probe");
        assert!(
            listed(&host, "doc/page").is_some(),
            "a refused op keeps the lists its check dropped"
        );

        let deleted = apply(&mut host, 3, vec![write(OWNER, 3, b"gone")]).await;
        applied(&deleted[0]);
        assert_eq!(listed(&host, "doc/page"), None);
    });
}

#[test]
fn a_signed_frame_cannot_send_the_kernels_check_and_an_op_touching_nothing_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), probe(Vec::new(), Vec::new())).await;
        let check = abi::role::access::Op::Check {
            program: "probe".into(),
            actor: Vec::new(),
            needs: vec![Need::write("doc")],
        };
        let forged = apply(
            &mut host,
            1,
            vec![
                signed(OTHER, 1, "access", abi::encode(&check)),
                write(OTHER, 2, b"free"),
            ],
        )
        .await;
        assert_eq!(refusal(&forged[0]).reason, reason::UNAUTHORIZED);
        applied(&forged[1]);
        assert_eq!(written(&host).as_deref(), Some(b"free".as_slice()));
    });
}

#[test]
fn with_no_access_role_bound_the_kernel_asks_no_target_what_an_op_touches() {
    deterministic::Runner::default().start(|context| async move {
        let unreadable = script(vec![set(NEEDS, vec![0xff])]);

        let dir = tempfile::tempdir().unwrap();
        let mut checked = found(context.child("checked"), dir.path(), unreadable.clone()).await;
        let refused = apply(&mut checked, 1, vec![write(OTHER, 1, b"checked")]).await;
        assert!(
            matches!(refused[0].outcome, Outcome::Rejected(_)),
            "{refused:?}"
        );
        assert_eq!(written(&checked), None);

        let dir = tempfile::tempdir().unwrap();
        let child = context.child("unchecked");
        let unbound = unchecked(unreadable);
        let founded = Host::found(child, "net", dir.path(), block_id(0), unbound).await;
        let (mut host, _) = founded.unwrap();
        assert_eq!(host.roles().get(role::ACCESS), None);
        let ran = apply(&mut host, 1, vec![write(OTHER, 1, b"unchecked")]).await;
        applied(&ran[0]);
        assert_eq!(written(&host).as_deref(), Some(b"unchecked".as_slice()));
    });
}

#[test]
fn unbinding_access_lifts_every_check_and_binding_it_again_brings_each_list_back() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let probed = probe(vec![Need::write("doc")], vec![("doc", owned_by(1))]);
        let mut host = found(context, dir.path(), probed).await;
        let closed = apply(
            &mut host,
            1,
            vec![set_list(OWNER, 1, "doc", Some(Ownership::Owner))],
        )
        .await;
        applied(&closed[0]);
        let refused = apply(&mut host, 2, vec![write(OTHER, 2, b"other")]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);

        applied(&apply(&mut host, 3, vec![fill(3, None)]).await[0]);
        apply(&mut host, 4, Vec::new()).await;
        assert_eq!(host.roles().get(role::ACCESS), None);
        let open = apply(&mut host, 5, vec![write(OTHER, 5, b"unbound")]).await;
        applied(&open[0]);
        assert_eq!(written(&host).as_deref(), Some(b"unbound".as_slice()));
        assert_eq!(listed(&host, "doc"), Some(abi::encode(&Ownership::Owner)));

        applied(&apply(&mut host, 6, vec![fill(6, Some("access"))]).await[0]);
        apply(&mut host, 7, Vec::new()).await;
        assert_eq!(
            host.roles().get(role::ACCESS).map(String::as_str),
            Some("access")
        );
        let again = apply(&mut host, 8, vec![write(OTHER, 8, b"rebound")]).await;
        assert_eq!(refusal(&again[0]).reason, reason::UNAUTHORIZED);
        assert_eq!(written(&host).as_deref(), Some(b"unbound".as_slice()));
    });
}
