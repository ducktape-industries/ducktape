//! The netstack machine's frozen lifecycle suite. [`harness`] is the pure
//! world every scenario runs in; [`scenarios`] are the lifecycles
//! themselves, each pinned as a golden event→effect trace under
//! `fixtures/<scenario>.trace`. The trace IS the specification: a behavior
//! change is a PR that regenerates it (`UPDATE_TRACES=1 cargo test -p
//! netstack-scenarios`), and the review reads the fixture diff.
//!
//! The suite is generic over the [`Backend`] that builds each node's
//! machine, so ONE fixture set binds every backend: the native machine (this
//! crate's tests), the wasm guest (`netstack-wasm`'s), and every crossing
//! between them a mid-life swap makes. A backend whose trace differs by a
//! byte fails exactly the way a behavior change does.

pub mod harness;
pub mod scenarios;

use netstack_machine::{Machine, MachineConfig, NetstackMachine};
use wireguard::IdentitySigner;

/// A machine restored from a snapshot another machine took: identity,
/// config, and the snapshot bytes in, a running machine out.
type RestoreFn = fn(Box<dyn IdentitySigner>, MachineConfig, &[u8]) -> Box<dyn NetstackMachine>;

/// How a lane builds a node's machine: fresh from its identity and config,
/// or continuing from a snapshot another machine took. Two functions
/// because a swap crosses backends — the machine that steps down and the
/// one that takes over need not be the same kind, and the trace must not
/// be able to tell.
#[derive(Clone, Copy)]
pub struct Backend {
    pub build: fn(Box<dyn IdentitySigner>, MachineConfig) -> Box<dyn NetstackMachine>,
    pub restore: RestoreFn,
}

/// The native machine, building and restoring.
pub const NATIVE: Backend = Backend {
    build: native_build,
    restore: native_restore,
};

pub fn native_build(
    signer: Box<dyn IdentitySigner>,
    config: MachineConfig,
) -> Box<dyn NetstackMachine> {
    Box::new(Machine::new(signer, config))
}

pub fn native_restore(
    signer: Box<dyn IdentitySigner>,
    config: MachineConfig,
    snapshot: &[u8],
) -> Box<dyn NetstackMachine> {
    Box::new(
        Machine::restore(signer, config, snapshot)
            .expect("a scenario restores the snapshot it just took"),
    )
}

/// One `#[test]` per scenario, each run on `$backend` against the shared
/// fixture. Invoked once per backend, in that backend's test crate; the
/// native lane's lint test keeps this list equal to [`scenarios`]'s.
#[macro_export]
macro_rules! suite {
    ($backend:expr) => {
        $crate::suite!(@each $backend;
            boot_two_members,
            boot_three_members,
            boot_five_members,
            boot_relay_through_hub,
            member_restart_mapping_kept,
            member_restart_mapping_lost,
            nat_rebind_readvertises,
            nat_rebind_live_readvertises,
            cutover_keeps_unchanged_peers,
            cutover_to_reduced_mesh,
            coordinator_dark,
            first_fanout_lost_both_sides,
            partition_across_cutover_heals_on_reconnect,
            handshake_lost_at_each_stage,
            duplicated_delivery_is_tolerated,
            standby_prewarm_then_promotion,
            cutover_keeps_endpoint_less_standby,
            slow_resolver_does_not_stall,
            join_direct_invite,
            join_coordinated_invite,
            backend_swap_mid_epoch,
        );
    };
    (@each $backend:expr; $($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                $crate::scenarios::$name($backend);
            }
        )*
    };
}
