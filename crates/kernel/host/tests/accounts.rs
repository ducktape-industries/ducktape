//! The account each frame acts as, asked of the identity role: across repeat signers, an account
//! given or changed mid-block, and an identity write rolled back, the senders and identity's root
//! are the ones the kernel gave when it asked once per frame.
use abi::role::{self, halo, validators};
use abi::{Outcome, Principal};
use commonware_runtime::{Runner as _, deterministic};
use fixture_probe::{Reply, Step};
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, VALSET,
};
use host::{Block, BlockId, Founding, Genesis, Host, Limits, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;
const IDENTITY_ROOT: &str = "62153d71a0ca604b519821de1f38abb553278455306fa3f381f9bae4cdd64a4f";

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    let roles: &[&str] = match program {
        "programs" => &[role::PROGRAMS],
        "valset" => &[role::VALIDATORS],
        "identity" => &[role::IDENTITY],
        "blobs" => &[role::BLOBS],
        "awaits" => &[role::AWAITS],
        "halo" => &[role::HALO],
        "access" => &[role::ACCESS],
        _ => &[],
    };
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn genesis() -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
            ),
            founding("access", ACCESS, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
        queries: validators::QueryPolicy::Signed,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

enum Sent {
    Env(&'static [u8]),
    Give(&'static [u8], u64),
}

fn frames(height: u64, sent: Vec<Sent>) -> Block {
    let submissions = sent
        .into_iter()
        .enumerate()
        .map(|(n, sent)| {
            let (signer, target, payload) = match sent {
                Sent::Env(signer) => (signer, "probe", abi::encode(&vec![Step::Env])),
                Sent::Give(key, account) => {
                    (SIGNER, "identity", abi::encode(&(key.to_vec(), account)))
                }
            };
            Submission {
                signer: signer.to_vec(),
                anchor: Tip {
                    height: height - 1,
                    id: block_id(height - 1),
                },
                salt: Some(height * 64 + n as u64),
                target: target.to_owned(),
                payload,
                client: None,
            }
        })
        .collect();
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    }
}

fn blocks() -> Vec<Vec<Sent>> {
    use Sent::{Env, Give};
    vec![
        vec![
            Env(b"k0"),
            Env(b"k1"),
            Env(b"k0"),
            Env(b"k1"),
            Env(b"k2"),
            Env(SIGNER),
        ],
        vec![
            Env(b"k1"),
            Give(b"k1", 11),
            Env(b"k1"),
            Env(b"k0"),
            Env(b"k1"),
        ],
        vec![
            Env(b"k2"),
            Give(b"k2", 21),
            Env(b"k2"),
            Give(b"k2", 22),
            Env(b"k2"),
            Env(b"k1"),
        ],
        vec![
            Env(b"k0"),
            Give(b"k0", u64::MAX),
            Env(b"k0"),
            Give(b"k0", 7),
            Env(b"k0"),
        ],
        vec![
            Env(b"k0"),
            Env(b"k1"),
            Env(b"k2"),
            Env(SIGNER),
            Env(SIGNER),
            Env(b"k3"),
        ],
    ]
}

fn told(applied: &host::Applied) -> Vec<String> {
    applied
        .submissions
        .iter()
        .map(|receipt| match &receipt.outcome {
            Outcome::Rejected(refusal) => format!("{} refused {}", receipt.program, refusal.reason),
            Outcome::Applied { output } if receipt.program == "probe" => {
                let replies: Vec<Reply> = abi::decode(output).unwrap();
                let [Reply::Env(env)] = replies.as_slice() else {
                    panic!("{replies:?}");
                };
                match env.sender {
                    Some(Principal::Account(account)) => format!("as {account}"),
                    Some(Principal::System) => "as the system".to_owned(),
                    None => "as no one".to_owned(),
                }
            }
            Outcome::Applied { .. } => format!("{} applied", receipt.program),
        })
        .collect()
}

#[test]
fn each_frame_acts_as_the_account_its_key_holds_when_it_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, _): (Host<Ctx>, _) =
            Host::found(context, "net", dir.path(), block_id(0), genesis())
                .await
                .unwrap();
        let mut seen = Vec::new();
        for (height, sent) in (1..).zip(blocks()) {
            let applied = host.apply(frames(height, sent)).await.unwrap();
            seen.push(told(&applied));
        }
        let expected: Vec<Vec<&str>> = vec![
            vec![
                "as no one",
                "as no one",
                "as no one",
                "as no one",
                "as no one",
                "as 1",
            ],
            vec![
                "as no one",
                "identity applied",
                "as 11",
                "as no one",
                "as 11",
            ],
            vec![
                "as no one",
                "identity applied",
                "as 21",
                "identity applied",
                "as 22",
                "as 11",
            ],
            vec![
                "as no one",
                "identity refused invalid_input",
                "as no one",
                "identity applied",
                "as 7",
            ],
            vec!["as 7", "as 11", "as 22", "as 1", "as 1", "as no one"],
        ];
        assert_eq!(seen, expected);
        let identity = host.store().root("identity").unwrap().unwrap();
        assert_eq!(abi::hex(&identity.0), IDENTITY_ROOT);
    });
}
