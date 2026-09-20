//! The envelope over a real component: the echo guest
//! (`fixture-guest/`, built into `fixtures/echo.media.wasm` by
//! `fixtures/build.sh`) mirrors every event as its twin effect, so one pass
//! over the variants proves the lowering and lifting through the canonical
//! ABI; then the behaviors only the envelope can show — a refused init and
//! a fuel trap.

use lane_wasm::{
    Config, Effect, Event, FlowId, Frame, GuestError, LaneBinding, LaneGuest, LaneMachine, PeerId,
    StepError,
};

/// The committed fixture — a test pins bytes on purpose.
const ECHO: &[u8] = include_bytes!("fixtures/echo.media.wasm");

fn peer(octet: u8) -> PeerId {
    PeerId([octet; 32])
}

fn config() -> Config {
    Config {
        self_peer: peer(1),
        lanes: vec![LaneBinding {
            name: "voice".into(),
            id: 2,
        }],
    }
}

fn echo() -> LaneGuest {
    LaneGuest::new(ECHO, config()).expect("the echo component loads")
}

#[test]
fn every_event_crosses_and_comes_back_as_its_twin_effect() {
    let mut guest = echo();
    let cases: Vec<(Event, Vec<Effect>)> = vec![
        (Event::Tick, Vec::new()),
        (
            Event::Datagram {
                lane: 2,
                peer: peer(5),
                bytes: vec![1, 2, 3],
            },
            vec![Effect::LaneSend {
                lane: 2,
                peer: peer(5),
                bytes: vec![1, 2, 3],
            }],
        ),
        (
            Event::ClientFrame {
                session: 9,
                frame: Frame::Text("{\"type\":\"beacon\"}".into()),
            },
            vec![Effect::ClientSend {
                session: 9,
                frame: Frame::Text("{\"type\":\"beacon\"}".into()),
            }],
        ),
        (
            Event::ClientFrame {
                session: 9,
                frame: Frame::Binary(vec![0x01, 0xff]),
            },
            vec![Effect::ClientSend {
                session: 9,
                frame: Frame::Binary(vec![0x01, 0xff]),
            }],
        ),
        (
            Event::Roster {
                session: 9,
                peers: vec![peer(5), peer(6)],
            },
            vec![Effect::SetRoster {
                lane: 2,
                flow: FlowId::from_raw(9),
                peers: vec![peer(5), peer(6)],
            }],
        ),
        (
            Event::SessionOpened {
                session: 9,
                channel: "room".into(),
            },
            vec![
                Effect::OpenFlow {
                    lane: 2,
                    flow: FlowId::from_raw(9),
                    max_queued: 128,
                },
                Effect::Log {
                    level: tracing::Level::INFO,
                    message: "room".into(),
                },
            ],
        ),
        (
            Event::SessionClosed { session: 9 },
            vec![
                Effect::CloseFlow {
                    lane: 2,
                    flow: FlowId::from_raw(9),
                },
                Effect::Close {
                    session: 9,
                    reason: "closed".into(),
                },
            ],
        ),
    ];
    for (event, want) in cases {
        let got = guest.step(event.clone(), 1_000).unwrap();
        assert_eq!(got, want, "{event:?}");
    }
}

#[test]
fn a_refused_init_is_named() {
    let err = LaneGuest::new(
        ECHO,
        Config {
            self_peer: peer(1),
            lanes: Vec::new(),
        },
    )
    .err()
    .unwrap();
    assert!(
        matches!(&err, GuestError::Init(reason) if reason == "no lanes"),
        "{err}"
    );
}

/// A guest that never returns exhausts its step budget: a trap, never a
/// hang — and the next instance over the same bytes steps normally.
#[test]
fn an_exhausted_step_budget_is_a_trap_and_a_fresh_guest_steps() {
    let mut guest = echo();
    let err = guest.step(Event::Tick, u64::MAX).unwrap_err();
    assert!(matches!(err, StepError::Trap(_)), "{err}");

    let mut fresh = echo();
    assert!(fresh.step(Event::Tick, 1_000).unwrap().is_empty());
}

/// The budget is per step, not per instance: a step's fuel is fresh even
/// after a heavy one.
#[test]
fn the_budget_is_reset_every_step() {
    let mut guest = LaneGuest::with_fuel(ECHO, config(), 100_000).unwrap();
    for _ in 0..50 {
        guest.step(Event::Tick, 1_000).unwrap();
    }
}
