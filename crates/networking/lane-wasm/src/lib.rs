//! The wasmtime embedding of the `ducktape:lane` world (`wit/lane.wit`): a
//! [`LaneGuest`] loads a module's realtime component, hands it its bound
//! lanes once ([`Config`]), and steps it through the [`LaneMachine`]
//! boundary — one [`Event`] in, the [`Effect`]s to perform in order out. A
//! media executor drives it on its own OS thread and performs the effects
//! against the data plane and the client sockets; the guest owns both wire
//! framings (mesh datagrams and client frames) and the host copies bytes.
//!
//! The envelope is off-consensus: fuel per step (a runaway guest traps
//! instead of wedging the plane) and one import (`host.log`). A trap, an
//! exhausted budget, or an effect that violates the contract (a peer that
//! is not 32 bytes) is a [`StepError::Trap`]: the instance's state is
//! unknown from then on, the executor discards it and closes every session
//! it held. It never touches the node or another plane.
//!
//! [`StubGuest`] is the native double of the call guest's fan-out — audio
//! to the roster, control to the room — for the executor's tests.

use std::collections::BTreeMap;

pub use data_plane::{FlowId, PeerId};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config as EngineConfig, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        world: "media",
        path: "wit",
    });
}

use bindings::Media;
use bindings::ducktape::lane::{host, types};

/// Fuel per step. Two orders above a 20 ms audio tick's real cost (a few
/// speakers of Opus decode and one encode) and small against a runaway:
/// exhaustion traps in milliseconds.
pub const STEP_FUEL: u64 = 200_000_000;

/// A declared lane id (`LaneDecl.id`).
pub type Lane = u8;
/// One admitted client socket, host-assigned.
pub type Session = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

/// One input to a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Tick,
    Datagram {
        lane: Lane,
        peer: PeerId,
        bytes: Vec<u8>,
    },
    ClientFrame {
        session: Session,
        frame: Frame,
    },
    Roster {
        session: Session,
        peers: Vec<PeerId>,
    },
    SessionOpened {
        session: Session,
        channel: String,
    },
    SessionClosed {
        session: Session,
    },
}

/// One output of a step; the host performs them in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    LaneSend {
        lane: Lane,
        peer: PeerId,
        bytes: Vec<u8>,
    },
    ClientSend {
        session: Session,
        frame: Frame,
    },
    /// The peers admitted on `(lane, flow)` from now on: default deny, the
    /// whole set every time.
    SetRoster {
        lane: Lane,
        flow: FlowId,
        peers: Vec<PeerId>,
    },
    OpenFlow {
        lane: Lane,
        flow: FlowId,
        max_queued: u32,
    },
    CloseFlow {
        lane: Lane,
        flow: FlowId,
    },
    Log {
        level: tracing::Level,
        message: String,
    },
    /// End a session; `reason` is the client's one closing text frame.
    Close {
        session: Session,
        reason: String,
    },
}

/// A lane the host bound for the module, under the name it declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneBinding {
    pub name: String,
    pub id: Lane,
}

/// What a guest learns once, before its first step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub self_peer: PeerId,
    pub lanes: Vec<LaneBinding>,
}

/// Why a step produced nothing usable. The instance is not stepped again.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The guest trapped, ran out of fuel, or returned an effect outside
    /// the contract.
    #[error("lane guest trapped: {0}")]
    Trap(String),
}

/// Why a guest could not be brought up.
#[derive(Debug, thiserror::Error)]
pub enum GuestError {
    /// The component did not load, link, instantiate, or survive `init`.
    #[error("lane component: {0}")]
    Component(String),
    /// The guest refused the config it was handed.
    #[error("lane guest refused the config: {0}")]
    Init(String),
}

/// The boundary the executor drives, whichever side of it the logic lives.
pub trait LaneMachine {
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError>;
}

// ---- marshalling: the Rust shapes above <-> the WIT's canonical-ABI twins

impl From<Frame> for types::Frame {
    fn from(frame: Frame) -> Self {
        match frame {
            Frame::Text(text) => types::Frame::Text(text),
            Frame::Binary(bytes) => types::Frame::Binary(bytes),
        }
    }
}

impl From<types::Frame> for Frame {
    fn from(frame: types::Frame) -> Self {
        match frame {
            types::Frame::Text(text) => Frame::Text(text),
            types::Frame::Binary(bytes) => Frame::Binary(bytes),
        }
    }
}

impl From<Event> for types::Event {
    fn from(event: Event) -> Self {
        match event {
            Event::Tick => types::Event::Tick,
            Event::Datagram { lane, peer, bytes } => types::Event::Datagram(types::Datagram {
                lane,
                peer: peer.0.to_vec(),
                bytes,
            }),
            Event::ClientFrame { session, frame } => {
                types::Event::ClientFrame(types::ClientFrame {
                    session,
                    frame: frame.into(),
                })
            }
            Event::Roster { session, peers } => types::Event::Roster(types::Roster {
                session,
                peers: peers.iter().map(|peer| peer.0.to_vec()).collect(),
            }),
            Event::SessionOpened { session, channel } => {
                types::Event::SessionOpened(types::SessionOpened { session, channel })
            }
            Event::SessionClosed { session } => types::Event::SessionClosed(session),
        }
    }
}

impl From<Config> for types::Config {
    fn from(config: Config) -> Self {
        types::Config {
            self_peer: config.self_peer.0.to_vec(),
            lanes: config
                .lanes
                .into_iter()
                .map(|lane| types::LaneBinding {
                    name: lane.name,
                    id: lane.id,
                })
                .collect(),
        }
    }
}

fn peer_from_wire(bytes: Vec<u8>) -> Result<PeerId, StepError> {
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StepError::Trap(format!("peer of {} bytes", bytes.len())))?;
    Ok(PeerId(key))
}

fn peers_from_wire(peers: Vec<Vec<u8>>) -> Result<Vec<PeerId>, StepError> {
    peers.into_iter().map(peer_from_wire).collect()
}

fn level_from_wire(level: host::Level) -> tracing::Level {
    match level {
        host::Level::Trace => tracing::Level::TRACE,
        host::Level::Debug => tracing::Level::DEBUG,
        host::Level::Info => tracing::Level::INFO,
        host::Level::Warn => tracing::Level::WARN,
        host::Level::Error => tracing::Level::ERROR,
    }
}

impl TryFrom<types::Effect> for Effect {
    type Error = StepError;

    fn try_from(effect: types::Effect) -> Result<Self, StepError> {
        Ok(match effect {
            types::Effect::LaneSend(send) => Effect::LaneSend {
                lane: send.lane,
                peer: peer_from_wire(send.peer)?,
                bytes: send.bytes,
            },
            types::Effect::ClientSend(send) => Effect::ClientSend {
                session: send.session,
                frame: send.frame.into(),
            },
            types::Effect::SetRoster(roster) => Effect::SetRoster {
                lane: roster.lane,
                flow: FlowId::from_raw(roster.flow),
                peers: peers_from_wire(roster.peers)?,
            },
            types::Effect::OpenFlow(open) => Effect::OpenFlow {
                lane: open.lane,
                flow: FlowId::from_raw(open.flow),
                max_queued: open.max_queued,
            },
            types::Effect::CloseFlow(close) => Effect::CloseFlow {
                lane: close.lane,
                flow: FlowId::from_raw(close.flow),
            },
            types::Effect::Log(line) => Effect::Log {
                level: level_from_wire(line.level),
                message: line.message,
            },
            types::Effect::Close(close) => Effect::Close {
                session: close.session,
                reason: close.reason,
            },
        })
    }
}

// ---- the wasmtime envelope

/// What the host side of the boundary holds for the guest: nothing but the
/// import it may call.
struct HostState;

/// `types` carries no functions; the linker still wants the impl.
impl types::Host for HostState {}

impl host::Host for HostState {
    fn log(&mut self, level: host::Level, target: String, message: String) {
        match level {
            host::Level::Trace => {
                tracing::trace!(target: "ducktape::lane", guest_target = %target, "{message}")
            }
            host::Level::Debug => {
                tracing::debug!(target: "ducktape::lane", guest_target = %target, "{message}")
            }
            host::Level::Info => {
                tracing::info!(target: "ducktape::lane", guest_target = %target, "{message}")
            }
            host::Level::Warn => {
                tracing::warn!(target: "ducktape::lane", guest_target = %target, "{message}")
            }
            host::Level::Error => {
                tracing::error!(target: "ducktape::lane", guest_target = %target, "{message}")
            }
        }
    }
}

/// One initialized guest inside one component instance, alive for the
/// plane's life or until its first trap.
pub struct LaneGuest {
    store: Store<HostState>,
    world: Media,
    step_fuel: u64,
}

impl LaneGuest {
    /// Load `component`, link `host.log`, and run `init` with `config`.
    pub fn new(component: &[u8], config: Config) -> Result<Self, GuestError> {
        Self::with_fuel(component, config, STEP_FUEL)
    }

    /// [`LaneGuest::new`] with an explicit per-step fuel budget; `init`
    /// runs under the default one.
    pub fn with_fuel(component: &[u8], config: Config, step_fuel: u64) -> Result<Self, GuestError> {
        let engine = Engine::new(&engine_config()).map_err(component_err)?;
        let component = Component::from_binary(&engine, component).map_err(component_err)?;
        let mut linker = Linker::new(&engine);
        Media::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(component_err)?;
        let mut store = Store::new(&engine, HostState);
        store.set_fuel(STEP_FUEL).map_err(component_err)?;
        let world = Media::instantiate(&mut store, &component, &linker).map_err(component_err)?;
        world
            .call_init(&mut store, &config.into())
            .map_err(component_err)?
            .map_err(GuestError::Init)?;
        Ok(Self {
            store,
            world,
            step_fuel,
        })
    }
}

impl LaneMachine for LaneGuest {
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError> {
        self.store.set_fuel(self.step_fuel).map_err(trap)?;
        let effects = self
            .world
            .call_step(&mut self.store, &event.into(), now_ms)
            .map_err(trap)?;
        effects.into_iter().map(Effect::try_from).collect()
    }
}

/// The envelope: the component model and fuel metering, nothing else.
fn engine_config() -> EngineConfig {
    let mut config = EngineConfig::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config
}

fn component_err(err: impl std::fmt::Display) -> GuestError {
    GuestError::Component(err.to_string())
}

fn trap(err: impl std::fmt::Display) -> StepError {
    StepError::Trap(err.to_string())
}

// ---- the native double

/// Per-sender datagram queue on the voice flow, the pre-#2216 hub's bound.
const VOICE_FLOW_QUEUE: u32 = 128;

/// The voice flow of a channel — the same derivation both sides of the
/// mesh use, so a stub on one node talks to a guest on another.
pub fn voice_flow(channel: &str) -> FlowId {
    FlowId::derive(format!("voice-channel:{channel}").as_bytes())
}

struct StubSession {
    channel: String,
    roster: Vec<PeerId>,
}

/// The call guest's fan-out without its codecs: a client's binary frame
/// goes to its roster on the voice lane unchanged, a voice datagram goes to
/// every session whose roster holds the sender, a client's text frame goes
/// to the other sessions of its channel. One flow per channel, opened by
/// the first session and closed by the last.
pub struct StubGuest {
    self_peer: PeerId,
    voice_lane: Lane,
    sessions: BTreeMap<Session, StubSession>,
}

impl StubGuest {
    /// Refuses a config with no lane named `voice`, as the call guest would.
    pub fn new(config: Config) -> Result<Self, GuestError> {
        let voice_lane = config
            .lanes
            .iter()
            .find(|lane| lane.name == "voice")
            .map(|lane| lane.id)
            .ok_or_else(|| GuestError::Init("no lane named voice".into()))?;
        Ok(Self {
            self_peer: config.self_peer,
            voice_lane,
            sessions: BTreeMap::new(),
        })
    }

    fn sessions_in(&self, channel: &str) -> impl Iterator<Item = (&Session, &StubSession)> {
        self.sessions
            .iter()
            .filter(move |(_, session)| session.channel == channel)
    }

    fn channel_roster(&self, channel: &str) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = self
            .sessions_in(channel)
            .flat_map(|(_, session)| session.roster.iter().copied())
            .collect();
        peers.sort();
        peers.dedup();
        peers
    }

    fn open(&mut self, session: Session, channel: String) -> Vec<Effect> {
        let first_in_channel = self.sessions_in(&channel).next().is_none();
        let open_flow = Effect::OpenFlow {
            lane: self.voice_lane,
            flow: voice_flow(&channel),
            max_queued: VOICE_FLOW_QUEUE,
        };
        self.sessions.insert(
            session,
            StubSession {
                channel,
                roster: Vec::new(),
            },
        );
        if first_in_channel {
            vec![open_flow]
        } else {
            Vec::new()
        }
    }

    fn close(&mut self, session: Session) -> Vec<Effect> {
        let Some(closed) = self.sessions.remove(&session) else {
            return Vec::new();
        };
        let last_in_channel = self.sessions_in(&closed.channel).next().is_none();
        if last_in_channel {
            vec![Effect::CloseFlow {
                lane: self.voice_lane,
                flow: voice_flow(&closed.channel),
            }]
        } else {
            vec![Effect::SetRoster {
                lane: self.voice_lane,
                flow: voice_flow(&closed.channel),
                peers: self.channel_roster(&closed.channel),
            }]
        }
    }

    fn roster(&mut self, session: Session, peers: Vec<PeerId>) -> Vec<Effect> {
        let self_peer = self.self_peer;
        let Some(state) = self.sessions.get_mut(&session) else {
            return Vec::new();
        };
        state.roster = peers
            .into_iter()
            .filter(|peer| *peer != self_peer)
            .collect();
        let channel = state.channel.clone();
        vec![Effect::SetRoster {
            lane: self.voice_lane,
            flow: voice_flow(&channel),
            peers: self.channel_roster(&channel),
        }]
    }

    fn client_frame(&self, session: Session, frame: Frame) -> Vec<Effect> {
        let Some(state) = self.sessions.get(&session) else {
            return Vec::new();
        };
        match frame {
            Frame::Binary(bytes) => state
                .roster
                .iter()
                .map(|peer| Effect::LaneSend {
                    lane: self.voice_lane,
                    peer: *peer,
                    bytes: bytes.clone(),
                })
                .collect(),
            Frame::Text(text) => self
                .sessions_in(&state.channel)
                .filter(|(other, _)| **other != session)
                .map(|(other, _)| Effect::ClientSend {
                    session: *other,
                    frame: Frame::Text(text.clone()),
                })
                .collect(),
        }
    }

    fn datagram(&self, lane: Lane, peer: PeerId, bytes: Vec<u8>) -> Vec<Effect> {
        let on_voice_lane = lane == self.voice_lane;
        if !on_voice_lane {
            return Vec::new();
        }
        self.sessions
            .iter()
            .filter(|(_, session)| session.roster.contains(&peer))
            .map(|(session, _)| Effect::ClientSend {
                session: *session,
                frame: Frame::Binary(bytes.clone()),
            })
            .collect()
    }
}

impl LaneMachine for StubGuest {
    fn step(&mut self, event: Event, _now_ms: u64) -> Result<Vec<Effect>, StepError> {
        Ok(match event {
            Event::Tick => Vec::new(),
            Event::Datagram { lane, peer, bytes } => self.datagram(lane, peer, bytes),
            Event::ClientFrame { session, frame } => self.client_frame(session, frame),
            Event::Roster { session, peers } => self.roster(session, peers),
            Event::SessionOpened { session, channel } => self.open(session, channel),
            Event::SessionClosed { session } => self.close(session),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(octet: u8) -> PeerId {
        PeerId([octet; 32])
    }

    fn stub() -> StubGuest {
        StubGuest::new(Config {
            self_peer: peer(1),
            lanes: vec![LaneBinding {
                name: "voice".into(),
                id: 2,
            }],
        })
        .unwrap()
    }

    fn step(stub: &mut StubGuest, event: Event) -> Vec<Effect> {
        stub.step(event, 0).unwrap()
    }

    #[test]
    fn a_config_without_a_voice_lane_is_refused() {
        let err = StubGuest::new(Config {
            self_peer: peer(1),
            lanes: Vec::new(),
        })
        .err()
        .unwrap();
        assert!(matches!(err, GuestError::Init(_)), "{err}");
    }

    #[test]
    fn audio_goes_to_the_roster_minus_self_and_back_to_who_holds_the_sender() {
        let mut stub = stub();
        let opened = step(
            &mut stub,
            Event::SessionOpened {
                session: 7,
                channel: "room".into(),
            },
        );
        assert_eq!(
            opened,
            vec![Effect::OpenFlow {
                lane: 2,
                flow: voice_flow("room"),
                max_queued: VOICE_FLOW_QUEUE
            }]
        );
        let admitted = step(
            &mut stub,
            Event::Roster {
                session: 7,
                peers: vec![peer(1), peer(2), peer(3)],
            },
        );
        assert_eq!(
            admitted,
            vec![Effect::SetRoster {
                lane: 2,
                flow: voice_flow("room"),
                peers: vec![peer(2), peer(3)]
            }]
        );

        let up = step(
            &mut stub,
            Event::ClientFrame {
                session: 7,
                frame: Frame::Binary(vec![1, 9]),
            },
        );
        assert_eq!(
            up,
            vec![
                Effect::LaneSend {
                    lane: 2,
                    peer: peer(2),
                    bytes: vec![1, 9]
                },
                Effect::LaneSend {
                    lane: 2,
                    peer: peer(3),
                    bytes: vec![1, 9]
                },
            ]
        );

        let down = step(
            &mut stub,
            Event::Datagram {
                lane: 2,
                peer: peer(3),
                bytes: vec![1, 8],
            },
        );
        assert_eq!(
            down,
            vec![Effect::ClientSend {
                session: 7,
                frame: Frame::Binary(vec![1, 8])
            }]
        );
        let stranger = step(
            &mut stub,
            Event::Datagram {
                lane: 2,
                peer: peer(9),
                bytes: vec![1, 8],
            },
        );
        assert!(
            stranger.is_empty(),
            "a peer outside every roster is dropped"
        );
        let other_lane = step(
            &mut stub,
            Event::Datagram {
                lane: 3,
                peer: peer(3),
                bytes: vec![1, 8],
            },
        );
        assert!(other_lane.is_empty(), "only the voice lane is echoed");
    }

    #[test]
    fn control_fans_out_to_the_channel_and_one_flow_serves_every_session() {
        let mut stub = stub();
        step(
            &mut stub,
            Event::SessionOpened {
                session: 1,
                channel: "room".into(),
            },
        );
        let second = step(
            &mut stub,
            Event::SessionOpened {
                session: 2,
                channel: "room".into(),
            },
        );
        assert!(second.is_empty(), "the flow is already open");
        step(
            &mut stub,
            Event::SessionOpened {
                session: 3,
                channel: "other".into(),
            },
        );

        let control = step(
            &mut stub,
            Event::ClientFrame {
                session: 1,
                frame: Frame::Text("{\"type\":\"beacon\"}".into()),
            },
        );
        assert_eq!(
            control,
            vec![Effect::ClientSend {
                session: 2,
                frame: Frame::Text("{\"type\":\"beacon\"}".into())
            }]
        );

        let first_gone = step(&mut stub, Event::SessionClosed { session: 1 });
        assert_eq!(
            first_gone,
            vec![Effect::SetRoster {
                lane: 2,
                flow: voice_flow("room"),
                peers: Vec::new()
            }]
        );
        let last_gone = step(&mut stub, Event::SessionClosed { session: 2 });
        assert_eq!(
            last_gone,
            vec![Effect::CloseFlow {
                lane: 2,
                flow: voice_flow("room")
            }]
        );
    }

    #[test]
    fn an_effect_with_a_malformed_peer_is_a_trap() {
        let effect = types::Effect::LaneSend(types::LaneSend {
            lane: 2,
            peer: vec![0; 31],
            bytes: Vec::new(),
        });
        let err = Effect::try_from(effect).err().unwrap();
        assert!(
            matches!(err, StepError::Trap(ref reason) if reason.contains("31")),
            "{err}"
        );
    }
}
