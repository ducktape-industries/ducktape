//! the modules registry's public wire surface — types only.
//!
//! modules is the MODULE CODE coordination plane, folded into ONE root-hashed
//! `root()`: per hot-swappable module, the ACTIVE 32-byte code hash plus at
//! most one pending `ScheduledSwap`; governance authorizes a
//! register/schedule/cancel, each validator emits `SwapReady` once the target
//! bytes are verified-resident, and the boundary `Advance` tick activates every
//! swap whose `activation_height` has been reached AND whose readiness latched
//! R=n. the code BYTES are out-of-band (content-addressed by the 32-byte
//! hash); this module is the consensus commitment to WHICH code is active,
//! never the bytes.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// the minimum lead (in blocks) between the scheduling block and a swap's
/// `activation_height`, so `H` is strictly in every node's future — long enough
/// to fetch + verify the out-of-band bytes before the boundary.
pub const MIN_SWAP_LEAD: u64 = 3;

/// the genesis-constant module id the modules registry registers under. the host
/// reads it to reconcile running code against the committed active hashes and
/// inject the boundary `Advance`; governance addresses its authorized
/// follow-ups here.
pub const DEFAULT_MODULES_ID: &str = "modules";

/// the length of a code hash: sha256 over the component bytes.
pub const CODE_HASH_LEN: usize = 32;

/// what a registry entry's artifact IS — fixed at admission (`seed`,
/// `RegisterModule`, `ScheduleRegister`) and never changed by a swap: a
/// swap that wants another kind is a new id, like a key-layout change. the
/// kind steers who verifies the bytes (a validator runs a `Module`'s core;
/// a `View` has no core and is verified as a view alone) and who seats
/// them (the host boundary seats modules; the app seats views). the
/// artifact frame carries the same tag, and the two must agree.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// consensus code with an optional mapper and an optional view.
    Module,
    /// a view and its assets, no consensus code and no state.
    View,
}

/// one genesis seed: the `modules` genesis-config table maps each id to
/// its kind and its initial deployment hash.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub kind: Kind,
    /// the 32-byte sha256 of the deployment frame.
    pub code_hash: Vec<u8>,
    /// the data-plane lanes this module founds the network with. Defaulted
    /// because most modules declare none and spelling `lanes: []` for every
    /// one of them is noise — not a compat shim: an absent list and an empty
    /// list mean the same thing, today and always.
    #[serde(default)]
    pub lanes: Vec<LaneDecl>,
}

// ---- the module-code path shapes --------------------------------------------

/// coordinates of a scheduled code swap for one module. **at most one** is ever
/// pending per module.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledSwap {
    pub name: String,
    pub activation_height: u64,
    /// the 32-byte sha256 of the target component bytes.
    pub code_hash: Vec<u8>,
    /// validator pubkeys that verified the target BYTES locally and signaled
    /// (`ModulesMsg::SwapReady`), strictly increasing. committed state, in
    /// the root like everything else here.
    pub readiness: Vec<Vec<u8>>,
    /// the block whose signal made `readiness` cover the whole boundary
    /// member set (R = n, evaluated at signal time) — LATCHED, never cleared.
    /// a swap never activates onto a validator set that has not demonstrably
    /// received the bytes; a member admitted AFTER the latch heals through
    /// the fetch lane (fail-closed backstop) rather than blocking the swap.
    /// the HEIGHT, not a flag, because the latch in block `L` is visible to
    /// the drain only from `L+1` (it reads committed `L`), so a replay of
    /// block `L` itself must still run the old code — see
    /// [`ScheduledSwap::armed_at`].
    pub ready_at: Option<u64>,
}

impl ScheduledSwap {
    /// THE arm predicate — the one every reader applies (the drain's
    /// `Advance` injection, the registry's `Advance` flip, the `ArmedAt`
    /// query, [`code_at`]): readiness latched STRICTLY before `height` (the
    /// drain at `height` reads the committed end of `height - 1`, so a latch
    /// in `height` is not yet its business) and the activation floor reached.
    pub fn armed_at(&self, height: u64) -> bool {
        let latched_before = self.ready_at.is_some_and(|latched| latched < height);
        let floor_reached = self.activation_height <= height;
        latched_before && floor_reached
    }

    /// STALE at `height`: the activation height is reached and `ready_at` was
    /// never latched. nothing armed this swap and [`ScheduledSwap::armed_at`]
    /// cannot fire for it here — it is a dead designation, not a swap in
    /// flight, so governance may REPLACE it with a new schedule or cancel it.
    ///
    /// without this, a pending no validator can ever signal — a component that
    /// is not a `ducktape:module`, so no readiness probe can load it — is the
    /// network's designation for life: `ScheduleSwap` refuses while a pending
    /// exists and `CancelSwap` refuses once the activation height is reached.
    pub fn stale_at(&self, height: u64) -> bool {
        let past_due = self.activation_height <= height;
        let never_latched = self.ready_at.is_none();
        past_due && never_latched
    }
}

/// one activation: `code_hash` became the module's running code FOR block
/// `height` — the block whose boundary `Advance` flipped it (a `RegisterModule`
/// records its own block; a genesis seed records 0). appended, never
/// rewritten: the registry is disk-durable and reopens AHEAD of a crash-restart
/// replay, so only a history can answer "which code sealed block h" once the
/// swap that replaced it has landed. committed state, in the root.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub height: u64,
    /// the 32-byte sha256 of the component bytes.
    pub code_hash: Vec<u8>,
}

/// the readable per-module projection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCode {
    pub module_id: String,
    pub kind: Kind,
    /// the last activation's hash — a projection of `history`, empty only for
    /// an admission that has not reached its boundary.
    pub active_code_hash: Vec<u8>,
    pub pending: Option<ScheduledSwap>,
    /// every activation in block order.
    pub history: Vec<Activation>,
}

/// the code designated for block `height` — what a node must RUN to apply it.
/// a pending swap armed at `height` ([`ScheduledSwap::armed_at`]) wins: that
/// is the live pre-flip read, the boundary of the very block that flips it,
/// and the flip then records the same `(height, code_hash)`. otherwise the
/// latest activation at or before `height` — the replay read against a
/// registry that is already AHEAD (a pending whose readiness latched in the
/// tip block is NOT armed for the tip, so those blocks replay on the old
/// code exactly as they ran). a module whose first activation is later than
/// `height` seats its first code (it has no ops before it). `None` is a
/// module registered but never activated.
pub fn code_at(entry: &ModuleCode, height: u64) -> Option<&[u8]> {
    let armed = entry.pending.as_ref().filter(|p| p.armed_at(height));
    if let Some(p) = armed {
        return Some(&p.code_hash);
    }
    let sealed_at_or_before = entry.history.iter().rev().find(|a| a.height <= height);
    let seat = sealed_at_or_before.or_else(|| entry.history.first());
    seat.map(|a| a.code_hash.as_slice())
}

/// one armed swap the host must realize: swap `module_id`'s registry code to
/// `code_hash` at the boundary.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArmedSwap {
    pub module_id: String,
    pub code_hash: Vec<u8>,
}

// ---- data-plane lanes -------------------------------------------------------

/// Lane ids the registry never admits: the node's own kernel planes. They are
/// fixed in the binary because a node must serve them BEFORE it can read any
/// registry — state sync is how it catches up, and the code plane is how it
/// fetches the very module that would declare a lane. Nothing bootstraps them.
pub const RESERVED_LANE_IDS: &[u8] = &[1, 6];

/// The highest declarable lane id, and the reason is arithmetic rather than
/// taste. A lane's two overlay ports are `45800 + id` (stream) and
/// `45900 + id` (datagram) — ranges only 100 apart. At id 100 a lane's STREAM
/// port IS lane 0's DATAGRAM port, so two lanes would silently share a socket
/// instead of failing a bind. 99 is where the ranges stay disjoint. Six
/// compile-time variants could never reach it; a declared id can.
pub const MAX_LANE_ID: u8 = 99;

// THE lane declaration shapes live in `module-artifact`, because the ARTIFACT
// FRAME is where a module declares them — the frame is what a deployment
// ships and what its hash covers. The registry commits what the frame said;
// it does not get a second, independent definition of the same thing.
pub use module_artifact::{
    LaneDecl, LanePacing, LaneStream, MAX_LANE_NAME_BYTES, lane_name_is_well_formed,
};

/// The lane table's readable entry: a declaration plus who owns it. The table
/// is the ONE place a lane's fields live; a module's record does not repeat
/// them, so there is no second copy to disagree.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaneRecord {
    pub id: u8,
    pub module_id: String,
    /// unique within `module_id` — what the host binds by.
    pub name: String,
    pub stream: Option<LaneStream>,
}

// ---- the wire surface -------------------------------------------------------

/// what an ingested op DOES to the modules registry. the ORIGIN is the
/// authority, not the variant: schedule/cancel/register are
/// governance/system-authored, `SwapReady` is validator-authored, and `Advance`
/// is the system-injected boundary tick.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesMsg {
    /// install a module's INITIAL active code hash (genesis/bootstrap). rejects a
    /// re-register of a known module — code changes go through `ScheduleSwap`.
    /// `Origin::Module("governance") | System` only.
    RegisterModule {
        module_id: String,
        kind: Kind,
        code_hash: Vec<u8>,
        /// the data-plane lanes this module brings. Empty for a module that
        /// needs none, which is most of them.
        lanes: Vec<LaneDecl>,
    },
    /// schedule a height-gated code swap for a registered module.
    /// `Origin::Module | System` only.
    ScheduleSwap {
        name: String,
        module_id: String,
        activation_height: u64,
        code_hash: Vec<u8>,
    },
    /// schedule the ADMISSION of a brand-new module post-genesis: creates the
    /// entry with an EMPTY active hash and this pending, readiness-latched
    /// initial code. the module has no running code until the boundary realizes
    /// the swap; the host instantiates it from the fetched bytes at activation.
    /// cancelling before the boundary removes the entry entirely.
    /// `Origin::Module | System` only.
    ScheduleRegister {
        name: String,
        module_id: String,
        kind: Kind,
        activation_height: u64,
        code_hash: Vec<u8>,
        /// declared with the admission, so a cancel before the boundary takes
        /// the lanes with the entry and frees their ids again.
        lanes: Vec<LaneDecl>,
    },
    /// clear a pending swap before its boundary. `Origin::Module | System` only.
    CancelSwap { name: String, module_id: String },
    /// a validator records that it HOLDS (verified locally) the pending swap's
    /// component bytes. `Origin::External(validator)` only, member-gated against
    /// the valset; the last covering signal latches the swap `ready`.
    /// `code_hash` names the BYTES this validator verified: a swap name is
    /// reusable across schedules, so a signal that outlives the pending it was
    /// written for must be refused rather than credited to whatever hash now
    /// wears the name.
    SwapReady {
        name: String,
        module_id: String,
        code_hash: Vec<u8>,
    },

    /// the system-injected boundary tick, keyed on `env.height`: activate every
    /// armed code swap. `Origin::System` only.
    Advance,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesQuery {
    /// active + pending code for every registered module.
    ModuleStatus,
    /// the swaps ARMED at `height` (`ScheduledSwap::armed_at`: readiness latched
    /// before `height` AND the activation floor reached). the host reads this at
    /// the boundary to know which registry modules to swap and to which code hash.
    ArmedAt { height: u64 },
    /// every declared data-plane lane, ascending by id. the host reads this to
    /// know which lanes to bind and with what pacing and backlog — it does not
    /// learn that from its own binary.
    Lanes,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesReply {
    ModuleStatus { modules: Vec<ModuleCode> },
    ArmedAt { swaps: Vec<ArmedSwap> },
    Lanes { lanes: Vec<LaneRecord> },
}

pub fn encode_msg(m: &ModulesMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<ModulesMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &ModulesQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<ModulesQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &ModulesReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<ModulesReply, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_msg(m: ModulesMsg) {
        assert_eq!(decode_msg(&encode_msg(&m)).unwrap(), m);
    }

    #[test]
    fn msg_query_reply_round_trip_every_variant() {
        rt_msg(ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: Kind::Module,
            code_hash: vec![1u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        });
        rt_msg(ModulesMsg::ScheduleSwap {
            name: "swap-hello".into(),
            module_id: "hello".into(),
            activation_height: 10,
            code_hash: vec![2u8; CODE_HASH_LEN],
        });
        rt_msg(ModulesMsg::ScheduleRegister {
            name: "admit-kanban".into(),
            module_id: "kanban".into(),
            kind: Kind::Module,
            activation_height: 10,
            code_hash: vec![5u8; CODE_HASH_LEN],
            // a lane rides the admission that brings it
            lanes: vec![LaneDecl {
                id: 7,
                name: "board_sync".into(),
                stream: Some(LaneStream {
                    pacing: LanePacing::Shared,
                    accept_backlog: 32,
                }),
            }],
        });
        rt_msg(ModulesMsg::ScheduleRegister {
            name: "admit-home".into(),
            module_id: "home".into(),
            kind: Kind::View,
            activation_height: 10,
            code_hash: vec![6u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        });
        rt_msg(ModulesMsg::CancelSwap {
            name: "swap-hello".into(),
            module_id: "hello".into(),
        });
        rt_msg(ModulesMsg::SwapReady {
            name: "swap-hello".into(),
            module_id: "hello".into(),
            code_hash: vec![2u8; CODE_HASH_LEN],
        });
        rt_msg(ModulesMsg::Advance);

        for q in [
            ModulesQuery::ModuleStatus,
            ModulesQuery::ArmedAt { height: 9 },
            ModulesQuery::Lanes,
        ] {
            assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        }

        let lanes = ModulesReply::Lanes {
            lanes: vec![
                // the media shape: no stream half at all
                LaneRecord {
                    id: 2,
                    module_id: "chat".into(),
                    name: "voice".into(),
                    stream: None,
                },
                LaneRecord {
                    id: 5,
                    module_id: "agent".into(),
                    name: "telemetry".into(),
                    stream: Some(LaneStream {
                        pacing: LanePacing::Local {
                            bulk_bytes_per_sec: 24 * 1024 * 1024,
                            bulk_burst_bytes: 512 * 1024,
                        },
                        accept_backlog: 16,
                    }),
                },
            ],
        };
        assert_eq!(decode_reply(&encode_reply(&lanes)).unwrap(), lanes);

        let r = ModulesReply::ModuleStatus {
            modules: vec![ModuleCode {
                module_id: "hello".into(),
                kind: Kind::Module,
                active_code_hash: vec![1u8; CODE_HASH_LEN],
                pending: None,
                history: vec![Activation {
                    height: 0,
                    code_hash: vec![1u8; CODE_HASH_LEN],
                }],
            }],
        };
        assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
    }

    #[test]
    fn kind_is_snake_case_on_the_wire() {
        assert_eq!(sdk::wire::encode(&Kind::Module), br#""module""#);
        assert_eq!(sdk::wire::encode(&Kind::View), br#""view""#);
        assert_eq!(borsh::to_vec(&Kind::Module).unwrap(), [0]);
        assert_eq!(borsh::to_vec(&Kind::View).unwrap(), [1]);
        let seed = Seed {
            kind: Kind::View,
            code_hash: vec![7u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        };
        assert_eq!(
            sdk::wire::decode::<Seed>(&sdk::wire::encode(&seed)).unwrap(),
            seed
        );
        // a seed that omits `lanes` entirely still decodes, and means none —
        // the genesis table spells the field only for a module that has one.
        let bare: Seed = sdk::wire::decode(br#"{"kind":"view","code_hash":[]}"#).unwrap();
        assert!(bare.lanes.is_empty());
    }

    #[test]
    fn a_lane_name_is_a_bounded_snake_case_token() {
        for good in ["voice", "video", "gateway", "telemetry", "run_output2"] {
            assert!(lane_name_is_well_formed(good), "{good}");
        }
        for bad in ["", "Voice", "voice-lane", "voice lane", "보이스", "v.1"] {
            assert!(!lane_name_is_well_formed(bad), "{bad:?}");
        }
        assert!(lane_name_is_well_formed(&"a".repeat(MAX_LANE_NAME_BYTES)));
        assert!(!lane_name_is_well_formed(
            &"a".repeat(MAX_LANE_NAME_BYTES + 1)
        ));
    }
}
