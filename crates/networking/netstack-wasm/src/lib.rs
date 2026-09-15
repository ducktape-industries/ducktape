//! The wasmtime embedding of the `ducktape:netstack` world (the machine
//! crate's `wit/netstack.wit`): a [`NetstackGuest`] loads the component,
//! configures one long-lived machine inside it, and steps it through the
//! same [`NetstackMachine`] boundary the native machine implements — the
//! executor never learns which it drives.
//!
//! The envelope is off-consensus: fuel per step (a runaway guest traps
//! instead of wedging the plane) and no ambient imports (the guest sees
//! exactly `host.sign`, `host.identity`, and `host.log`). A trap, an
//! exhausted budget, or an undecodable wire value is a [`StepError::Fault`]:
//! the guest's state is unknown from then on and the executor stops the
//! plane. It never substitutes another protocol implementation.
//!
//! A guest can also start from a snapshot ([`NetstackGuest::restore`]) and
//! hand one out ([`NetstackMachine::snapshot`]): the same wire value the
//! native machine takes and gives, which is what lets a plane swap
//! backends mid-epoch without touching a tunnel. Snapshot bytes are opaque
//! to this host: the candidate guest decides whether it can restore them.
//! A guest state-layout change can refuse replacement without changing the
//! host Event/Effect ABI. There is no implicit migration or fresh-state retry.

use netstack_machine::wire;
use netstack_machine::{Effect, Event, MachineConfig, NetstackMachine, StepError};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};
use wireguard::IdentitySigner;

mod bindings {
    wasmtime::component::bindgen!({
        world: "netstack",
        path: "../netstack-machine/wit",
    });
}

use bindings::Netstack;
use bindings::ducktape::netstack::host;

/// Fuel per step. Generous against the machine's real cost
/// (a five-member boot step is a few million instructions) and small
/// against a runaway: exhaustion traps in milliseconds, not minutes.
pub const STEP_FUEL: u64 = 2_000_000_000;

/// What the host side of the boundary holds for the guest.
struct HostState {
    signer: Box<dyn IdentitySigner>,
}

impl host::Host for HostState {
    fn sign(&mut self, namespace: Vec<u8>, message: Vec<u8>) -> Vec<u8> {
        self.signer
            .sign_message(&namespace, &message)
            .as_ref()
            .to_vec()
    }

    fn identity(&mut self) -> Vec<u8> {
        self.signer.identity().as_ref().to_vec()
    }

    fn log(&mut self, level: host::Level, target: String, message: String) {
        match level {
            host::Level::Trace => {
                tracing::trace!(target: "ducktape::reachability", guest_target = %target, "{message}")
            }
            host::Level::Debug => {
                tracing::debug!(target: "ducktape::reachability", guest_target = %target, "{message}")
            }
            host::Level::Info => {
                tracing::info!(target: "ducktape::reachability", guest_target = %target, "{message}")
            }
            host::Level::Warn => {
                tracing::warn!(target: "ducktape::reachability", guest_target = %target, "{message}")
            }
            host::Level::Error => {
                tracing::error!(target: "ducktape::reachability", guest_target = %target, "{message}")
            }
        }
    }
}

/// Why a guest could not be brought up.
#[derive(Debug, thiserror::Error)]
pub enum GuestError {
    /// The component did not load, link, instantiate, or survive its
    /// configure call.
    #[error("netstack component: {0}")]
    Component(String),
    /// The guest refused the config it was handed.
    #[error("netstack guest refused the config: {0}")]
    Configure(String),
    /// The guest refused the snapshot it was handed: another contract,
    /// another identity, or bytes that are not a snapshot.
    #[error("netstack guest refused the snapshot: {0}")]
    Restore(String),
}

/// One configured machine inside one component instance, alive for the
/// plane's life.
pub struct NetstackGuest {
    store: Store<HostState>,
    world: Netstack,
    step_fuel: u64,
}

impl NetstackGuest {
    /// Load `component`, link the host imports over `signer`, and configure
    /// the machine inside it.
    pub fn new(
        component: &[u8],
        signer: Box<dyn IdentitySigner>,
        config: MachineConfig,
    ) -> Result<Self, GuestError> {
        Self::with_fuel(component, signer, config, STEP_FUEL)
    }

    /// [`NetstackGuest::new`] with an explicit per-step fuel budget; the
    /// configure call runs under the default one.
    pub fn with_fuel(
        component: &[u8],
        signer: Box<dyn IdentitySigner>,
        config: MachineConfig,
        step_fuel: u64,
    ) -> Result<Self, GuestError> {
        let mut guest = Self::instantiate(component, signer, step_fuel)?;
        guest
            .world
            .call_configure(&mut guest.store, &wire::encode_config(&config))
            .map_err(component_err)?
            .map_err(GuestError::Configure)?;
        Ok(guest)
    }

    /// A guest continuing from `snapshot` — the wire snapshot any machine
    /// of this contract took under the same identity — under `step_fuel`
    /// per step; the restore call runs under the default budget.
    pub fn restore(
        component: &[u8],
        signer: Box<dyn IdentitySigner>,
        config: MachineConfig,
        snapshot: &[u8],
        step_fuel: u64,
    ) -> Result<Self, GuestError> {
        let mut guest = Self::instantiate(component, signer, step_fuel)?;
        guest
            .world
            .call_restore(&mut guest.store, &wire::encode_config(&config), snapshot)
            .map_err(component_err)?
            .map_err(GuestError::Restore)?;
        Ok(guest)
    }

    /// Load, link, and instantiate the component — no machine inside yet.
    fn instantiate(
        component: &[u8],
        signer: Box<dyn IdentitySigner>,
        step_fuel: u64,
    ) -> Result<Self, GuestError> {
        let engine = Engine::new(&engine_config()).map_err(component_err)?;
        let component = Component::from_binary(&engine, component).map_err(component_err)?;
        let mut linker = Linker::new(&engine);
        Netstack::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(component_err)?;
        let mut store = Store::new(&engine, HostState { signer });
        store.set_fuel(STEP_FUEL).map_err(component_err)?;
        let world =
            Netstack::instantiate(&mut store, &component, &linker).map_err(component_err)?;
        Ok(Self {
            store,
            world,
            step_fuel,
        })
    }
}

impl NetstackMachine for NetstackGuest {
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError> {
        self.store.set_fuel(self.step_fuel).map_err(fault)?;
        let bytes = self
            .world
            .call_step(&mut self.store, &wire::encode_event(&event), now_ms)
            .map_err(fault)?
            .map_err(StepError::Fault)?;
        let outcome = wire::decode_step(&bytes).map_err(fault)?;
        outcome.map_err(StepError::Protocol)
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, StepError> {
        self.store.set_fuel(self.step_fuel).map_err(fault)?;
        self.world
            .call_snapshot(&mut self.store)
            .map_err(fault)?
            .map_err(StepError::Fault)
    }
}

/// The envelope: the component model and fuel metering, nothing else —
/// the guest's determinism obligation is trace identity with the native
/// machine, which the sans-I/O contract already provides.
fn engine_config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config
}

fn component_err(err: impl std::fmt::Display) -> GuestError {
    GuestError::Component(err.to_string())
}

fn fault(err: impl std::fmt::Display) -> StepError {
    StepError::Fault(err.to_string())
}
