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

#[cfg(test)]
mod tests {
    use super::*;

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
