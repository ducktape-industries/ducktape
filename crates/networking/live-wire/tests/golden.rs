// The wire, committed: one sample through every control message, every lane message, both
// data headers, a websocket frame of each kind and the proof preimage, as bytes. A sampled
// byte that moves fails; when the change is meant, regenerate with LIVE_GOLDEN_WRITE=1.

use std::path::PathBuf;

use live_wire::{
    Challenge, Code, Control, DatagramHeader, End, Forward, GroupHeader, Hello, KeyScheme,
    LaneMeta, Mode, Proof, Start, Track, Welcome, WsFrame, proof_preimage,
};

const MESSAGE: &str = "the live wire changed: if intended, regenerate with LIVE_GOLDEN_WRITE=1";

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn check(name: &str, actual: &[u8]) {
    let path = golden(&format!("{name}.bin"));
    if std::env::var_os("LIVE_GOLDEN_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("{}: {error}; {MESSAGE}", path.display()));
    if expected != actual {
        let first = expected
            .iter()
            .zip(actual)
            .position(|(a, b)| a != b)
            .unwrap_or(expected.len().min(actual.len()));
        panic!(
            "{MESSAGE} ({name}.bin: {} bytes expected, {} built, first difference at byte {first})",
            expected.len(),
            actual.len()
        );
    }
}

fn track() -> Track {
    Track {
        namespace: "net#0a1b2c3d/chat/general".into(),
        name: "presence/42".into(),
    }
}

fn controls() -> Vec<Control> {
    let track = track();
    vec![
        Control::Hello(Hello {
            chain: [1; 32],
            scheme: KeyScheme::Ed25519,
            key: vec![2; 32],
        }),
        Control::Challenge(Challenge { nonce: [3; 32] }),
        Control::Proof(Proof {
            signature: vec![4; 64],
        }),
        Control::Welcome(Welcome {
            account: 42,
            datagrams: true,
        }),
        Control::Publish {
            request: 1,
            track: track.clone(),
            mode: Mode::Stream,
            priority: 128,
        },
        Control::Unpublish { request: 1 },
        Control::Subscribe {
            request: 2,
            track: track.clone(),
            from: Start::Latest,
        },
        Control::Subscribe {
            request: 3,
            track: track.clone(),
            from: Start::Next,
        },
        Control::Unsubscribe { request: 2 },
        Control::Watch {
            request: 4,
            namespace: track.namespace.clone(),
        },
        Control::Unwatch { request: 4 },
        Control::Published {
            request: 1,
            alias: 7,
        },
        Control::Subscribed {
            request: 2,
            alias: 7,
            by: 42,
            mode: Mode::Datagram,
            latest: Some(9),
        },
        Control::Subscribed {
            request: 3,
            alias: 7,
            by: 42,
            mode: Mode::Stream,
            latest: None,
        },
        Control::Subscribers {
            request: 1,
            count: 3,
        },
        Control::Announce {
            request: 4,
            track: track.clone(),
            by: 42,
            mode: Mode::Stream,
            up: true,
        },
        Control::Ended {
            request: 2,
            reason: End::TrackEnded,
        },
        Control::Ended {
            request: 2,
            reason: End::PublisherGone,
        },
        Control::Ended {
            request: 2,
            reason: End::GrantRevoked,
        },
        Control::Ended {
            request: 2,
            reason: End::NodeClosing,
        },
        Control::Refused {
            request: 0,
            code: Code::NoAccount,
        },
        Control::Refused {
            request: 0,
            code: Code::BadProof,
        },
        Control::Refused {
            request: 1,
            code: Code::NotGranted,
        },
        Control::Refused {
            request: 2,
            code: Code::Unknown,
        },
        Control::Refused {
            request: 1,
            code: Code::Duplicate,
        },
        Control::Refused {
            request: 1,
            code: Code::TooLarge,
        },
        Control::Refused {
            request: 1,
            code: Code::Malformed,
        },
        Control::Waiting {
            request: 1,
            code: Code::NotGranted,
        },
        Control::GoAway { in_ms: 500 },
    ]
}

fn forwards() -> Vec<Forward> {
    let track = track();
    vec![
        Forward::Announce {
            track: track.clone(),
            by: 42,
            mode: Mode::Datagram,
            priority: 7,
            up: false,
        },
        Forward::Subscribe {
            request: 5,
            track,
            from: Start::Latest,
        },
        Forward::Unsubscribe { request: 5 },
        Forward::Forwarded {
            request: 5,
            latest: Some(11),
        },
        Forward::Ended {
            request: 5,
            reason: End::PublisherGone,
        },
        Forward::Refused {
            request: 5,
            code: Code::NotGranted,
        },
        Forward::Count {
            request: 5,
            count: 2,
        },
    ]
}

#[test]
fn every_message_keeps_its_bytes() {
    let header = GroupHeader {
        alias: 7,
        group: 9,
        priority: 128,
    };
    let datagram = DatagramHeader {
        alias: 7,
        group: 9,
        object: 3,
    };
    check("control", &abi::encode(&controls()));
    check("forward", &abi::encode(&forwards()));
    check(
        "lane_meta",
        &abi::encode(&[LaneMeta::Control, LaneMeta::Group]),
    );
    check("group_header", &abi::encode(&header));
    check("datagram_header", &abi::encode(&datagram));
    check(
        "ws",
        &abi::encode(&vec![
            WsFrame::Control(Control::GoAway { in_ms: 500 }),
            WsFrame::Group {
                header,
                objects: vec![vec![1, 2, 3], vec![]],
                end: true,
            },
            WsFrame::Datagram {
                header: datagram,
                payload: vec![4, 5],
            },
        ]),
    );
    check(
        "proof_preimage",
        &proof_preimage(&[1; 32], &[4; 32], &[3; 32], &[2; 32]),
    );
}

#[test]
fn every_message_round_trips() {
    for control in controls() {
        assert_eq!(
            abi::decode::<Control>(&abi::encode(&control)).unwrap(),
            control
        );
    }
    for forward in forwards() {
        assert_eq!(
            abi::decode::<Forward>(&abi::encode(&forward)).unwrap(),
            forward
        );
    }
}

#[test]
fn every_code_has_a_word_of_its_own() {
    let codes = [
        Code::NoAccount,
        Code::BadProof,
        Code::NotGranted,
        Code::Unknown,
        Code::Duplicate,
        Code::TooLarge,
        Code::Malformed,
    ];
    let mut words: Vec<&str> = codes.iter().map(|code| code.as_str()).collect();
    words.sort_unstable();
    words.dedup();
    assert_eq!(words.len(), codes.len());
}
