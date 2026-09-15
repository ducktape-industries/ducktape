//! the agent module's public wire surface — types only.
//!
//! a PROGRAM ACCOUNT is a keyless account identity founds for a controller
//! account and this module executes ([`identity::Control::Program`]). a
//! BINDING ties one program account to one [`Program`]; its REVISION counts
//! the programs the account has been bound to. the account's AUTHORITY is
//! identity's control record — executor, standing, generation — read at every
//! start and every resumption and never copied: a transfer, a standing change
//! or a revocation acts the moment identity records it. an INVOCATION is one
//! program's handling of one attribution change delivered to its account: it
//! is keyed by the account and the change's plane-wide `seq`, runs under the
//! revision and generation it started at, advances step by step, holds at
//! most one outstanding request, and resumes when that request's outcome is
//! delivered back as data — or ends [`Status::Aborted`] when that authority
//! moved while it waited.
//!
//! ## programs
//!
//! a program is a finite list of [`Step`]s, data on the wire, never text. a
//! step is evaluated against the invocation's FRAME: the triggering change,
//! the causal context it was delivered under, the account itself, and every
//! name an earlier step bound. control only ever moves FORWARD — a branch or a
//! failure continuation names a later step (or the end) — so one invocation
//! visits each step at most once and evaluation is bounded by the program the
//! controller submitted. a persistent cycle is expressed the way the network
//! expresses everything: as a call whose effects attribute the account again,
//! each reaction being its own invocation that the program's predicates may
//! end.
//!
//! ## values and references
//!
//! a [`Value`] is a JSON template. rendering it produces the target's real
//! `sdk::wire` JSON — a [`Value::Ref`] splices a frame value in as a JSON
//! value, never as substituted text or bytes. replies, outputs and stamps are
//! decoded back into JSON explicitly ([`Decode`]); a non-integer number
//! anywhere is refused, so no evaluation ever depends on floating point.
//!
//! ## failure
//!
//! the program owns failure. a call that did not apply binds its
//! [`CallResult`] and continues at the step's failure continuation, which may
//! inspect the reason, recover with another call, [`Step::Report`] to a
//! recipient it chooses, or finish; with [`Continuation::Unhandled`] the
//! invocation ends [`Status::Failed`] with the outcome recorded. a fault of
//! the program itself (an unresolvable reference, an undecodable reply, a
//! frame the store cannot hold) is a [`ProgramFault`] recorded the same way.
//! nothing here undoes an earlier successful call or the change that invoked
//! the program: every invocation state is queryable and none is retried.

use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub use attribution::Reason;
pub use dispatch::{Attempt, CallOutcome, Refusal};
pub use sdk::AccountNumber;
use sdk::{CallId, Cause, ItemRef, ModuleId};

/// the JSON a rendered value, a decoded reply, or a frame binding is.
pub type JsonValue = serde_json::Value;

/// the frame root naming the triggering [`attribution::Change`], as JSON.
/// `change.cause` is the context the change was RECORDED under — which call
/// or delivery produced it.
pub const REF_CHANGE: &str = "change";
/// the frame root naming the [`sdk::Cause`] the change was DELIVERED under:
/// the host's delivery of attribution's item, sharing the change's root. a
/// cause reads in the sdk's own wire spelling — `"Direct"`, or
/// `{"Chain": {"root": {"Item" | "Call": ..}, "hop": {"Delivery" | "Call" |
/// "Completion": ..}}}` — so `["cause", "Chain", "root", "Call"]` is defined
/// exactly when the chain started at a call.
pub const REF_CAUSE: &str = "cause";
/// the frame root naming the program account's number.
pub const REF_ACCOUNT: &str = "account";
/// the frame roots a program may not bind over.
pub const RESERVED_ROOTS: [&str; 3] = [REF_CHANGE, REF_CAUSE, REF_ACCOUNT];

/// the object kind of every report a program emits: the attribution source
/// is `(agent, "report", "<account>/<seq>/<step>")` — one object per report
/// step of one invocation, so two reports can never overwrite each other.
pub const REPORT_KIND: &str = "report";
/// the revision every report object carries: a report object is written
/// exactly once, at its step, by its invocation.
pub const REPORT_REVISION: u64 = 1;

// ---- programs --------------------------------------------------------------------

/// the reaction program bound to a program account.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub steps: Vec<Step>,
}

/// one step of a program. `bind` names the result in the frame for later
/// steps; a target index names a LATER step, or `steps.len()` for the end.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// read a sibling module: `query` renders to the module's query wire,
    /// the reply is decoded as JSON and bound. a sibling that answers with
    /// an error faults the invocation.
    Query {
        module: ModuleId,
        query: Value,
        bind: String,
    },
    /// queue a call at `module` as the program account: `msg` renders to the
    /// module's op wire. the invocation waits for the call's completion,
    /// binds its [`CallResult`], and continues at the next step when it
    /// applied, else at `on_failure`.
    Call {
        module: ModuleId,
        msg: Value,
        bind: String,
        decode: Decode,
        on_failure: Continuation,
    },
    /// run a dispatch recipe (off-chain work through the dispatch plane):
    /// `payload` renders to the recipe's input. the invocation waits for the
    /// judged result, binds its [`DispatchResult`], and continues at the next
    /// step when it succeeded, else at `on_failure`.
    Dispatch {
        recipe_id: String,
        payload: Value,
        bind: String,
        decode: Decode,
        on_failure: Continuation,
    },
    /// continue at `then` when `test` holds, else at `or`.
    Branch { test: Predicate, then: u64, or: u64 },
    /// attribute a report to `recipient` (rendered: an account number) for
    /// `reason`, with `detail` rendered as the relation's payload. emitted in
    /// the same unit as the step, as this module's own attribution source.
    Report {
        recipient: Value,
        reason: Reason,
        detail: Value,
    },
    /// end the invocation.
    Finish,
}

/// how a call's output or a dispatch's result bytes become the frame's JSON.
/// a call's assigned stamp is always the target's own wire (`sdk::wire`
/// JSON), whatever encoding its output declares.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Decode {
    /// the bytes are `sdk::wire` JSON (empty reads as `null`).
    Json,
    /// the bytes are UTF-8 text, bound as a JSON string.
    Text,
    /// the bytes are opaque, bound in JSON's byte form — an array of
    /// numbers, the form [`Value::Bytes`] renders to — so a later step
    /// carries them into a call verbatim. never read as text.
    Bytes,
}

/// where an invocation continues when a call or dispatch did not apply.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Continuation {
    /// a later step: the program handles the failure there.
    Step(u64),
    /// no handler: the invocation ends [`Status::Failed`] with the outcome.
    Unhandled,
}

/// a JSON template. every variant but [`Value::Ref`] renders to itself; a
/// number is an integer within `i64::MIN..=u64::MAX`, the range JSON wire
/// numbers hold without floating point.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Value {
    Null,
    Bool(bool),
    Number(i128),
    Text(String),
    /// renders as JSON's byte form: an array of numbers.
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    /// a path into the frame: a root ([`REF_CHANGE`], [`REF_CAUSE`],
    /// [`REF_ACCOUNT`], or a bound name) followed by object keys and array
    /// indexes. an unresolvable path is a [`ProgramFault::Unresolved`].
    Ref(Vec<String>),
}

/// a test over the frame.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    /// both sides render to the same JSON value.
    Equals {
        left: Value,
        right: Value,
    },
    /// the value renders to something other than `null`; a reference that
    /// resolves to nothing is simply not defined, never a fault.
    Defined(Value),
    Not(Box<Predicate>),
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
}

// ---- the frame's view of an outcome ---------------------------------------------

/// what a [`Step::Call`] binds: the call's outcome with the target's declared
/// output and assigned stamp decoded into JSON.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallResult {
    Applied {
        output: JsonValue,
        assigned: JsonValue,
    },
    Rejected {
        reason: String,
    },
    Refused(Refusal),
    Unrepresentable {
        attempted: Attempt,
    },
}

/// what a [`Step::Dispatch`] binds: the judged result decoded into JSON, or
/// the failure the dispatch plane reported.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchResult {
    Completed { output: JsonValue },
    Failed { reason: String },
}

// ---- invocation state ---------------------------------------------------------------

/// the one request an invocation is waiting on.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Outstanding {
    Call(CallId),
    Dispatch { dispatch_id: String },
}

/// a fault of the program itself, at the step that hit it.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramFault {
    /// a reference into the frame resolved to nothing.
    Unresolved { path: Vec<String> },
    /// a reply, output or stamp was not the JSON its decoding declares.
    Undecodable { what: String, detail: String },
    /// a query step's sibling answered with an error.
    Query { module: ModuleId, error: String },
    /// a report's recipient did not render to an account number.
    Recipient { rendered: String },
    /// a value could not be rendered to JSON.
    Unrenderable { detail: String },
    /// the invocation's frame outgrew the store's value bound; the bindings
    /// were dropped so the invocation stays recorded.
    FrameTooLarge { bytes: u64 },
}

/// why an invocation ended without finishing.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Failure {
    /// a call did not apply and the step had no failure continuation.
    UnhandledCall(CallOutcome),
    /// a dispatch failed and the step had no failure continuation.
    UnhandledDispatch {
        reason: String,
    },
    Program(ProgramFault),
}

/// why an invocation ended without its program deciding: what the check of
/// its authority found when the answer it waited on arrived — or, for
/// [`Abort::Unbound`] and [`Abort::Replaced`], what a read finds while it
/// still waits.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Abort {
    /// the account is no longer bound here.
    Unbound,
    /// the account was bound to another program since.
    Replaced,
    /// identity revoked the account.
    Revoked,
    /// the account's standing is suspended.
    Suspended,
    /// the account's control record moved since the invocation started: a
    /// transfer, or any other mutation of it.
    StaleGeneration,
}

/// where an invocation is. an aborted invocation is one whose authority
/// moved while it waited: written with its reason when the answer it waited
/// on arrives (no step of its program runs on that answer), and read as
/// aborted before that when its binding is gone or replaced. its facts stay
/// readable either way.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    Running { step: u64, awaiting: Outstanding },
    Finished { at_step: u64 },
    Failed { step: u64, failure: Failure },
    Aborted { at_step: u64, reason: Abort },
}

/// one binding as queries expose it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingView {
    pub account: AccountNumber,
    pub program: Program,
    /// how many programs the account was bound to before this one: 0 at
    /// provisioning, one more at every replacement.
    pub revision: u64,
}

/// one invocation as queries expose it. the triggering change is not copied
/// here: it is attribution's record `seq`, readable there.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationView {
    pub account: AccountNumber,
    pub seq: u64,
    /// the binding revision the invocation runs.
    pub revision: u64,
    /// identity's control generation the invocation started under: the
    /// authority every call it queues is admitted at.
    pub generation: u64,
    /// the attribution queue item that delivered the change.
    pub item: ItemRef,
    /// the causal context the change was delivered under.
    pub cause: Cause,
    pub status: Status,
    /// every name the program bound, as the JSON its references see.
    pub bindings: BTreeMap<String, JsonValue>,
}

/// one entry of an account's invocation listing: the invocation and its
/// position in that listing, the `after` cursor that continues it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationEntry {
    pub at: u64,
    pub invocation: InvocationView,
}

// ---- ops ------------------------------------------------------------------------------

/// the ops an account submits: a key-held account by signed frame, a program
/// account through a call its executor queued. the acting account is the
/// controller of what it provisions and must be the current controller of
/// what it replaces or unbinds — identity's record, read at execution.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentMsg {
    /// found a program account named `name` for the acting account and bind
    /// `program` to it, in one unit: identity founds the account and answers
    /// this module in the same unit, so the account and its binding commit
    /// together or not at all. stamps [`AgentAssigned::Provisioned`].
    Provision {
        /// Idempotent within the authenticated controller account. Identical
        /// retries return the original account; conflicting reuse is refused.
        request_id: String,
        name: String,
        program: Program,
    },
    /// Explicitly deliver one controller-authorized initialization event to
    /// a program. Its durable request identity prevents boot/retry replay.
    Initialize {
        account: AccountNumber,
        request_id: String,
    },
    /// bind a new program to `account`. the account's standing is re-set,
    /// which advances its generation: every call queued under the old
    /// program is refused at execution and every running invocation of it
    /// ends aborted.
    Replace {
        account: AccountNumber,
        program: Program,
    },
    /// remove `account`'s binding and suspend it. its queued calls are
    /// refused at execution; its running invocations end aborted; no change
    /// invokes it again.
    Unbind { account: AccountNumber },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentQuery {
    Initialization {
        account: AccountNumber,
        request_id: String,
    },
    Provision {
        controller: AccountNumber,
        request_id: String,
    },
    Binding {
        account: AccountNumber,
    },
    Invocation {
        account: AccountNumber,
        seq: u64,
    },
    /// the invocations of one account in starting order, after cursor
    /// `after` (`0` reads from the first). `at` is the account's own
    /// ordinal, starting at 1.
    Invocations {
        account: AccountNumber,
        after: u64,
        limit: u64,
    },
}

// a reply is built once per query and encoded at once, never held in bulk,
// so the invocation view's size costs nothing that boxing would earn back.
#[allow(
    clippy::large_enum_variant,
    reason = "this public serde wire enum keeps ergonomic unboxed payload variants"
)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentReply {
    Initialization(Option<InitializationReceipt>),
    Provision(Option<ProvisionReceipt>),
    Binding(Option<BindingView>),
    Invocation(Option<InvocationView>),
    Invocations(Vec<InvocationEntry>),
}

/// the stamp an op declares through `set_assigned`: the value this module
/// assigned while applying it, which exists nowhere in the op payload.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentAssigned {
    /// the account identity founded for a [`AgentMsg::Provision`], bound.
    Provisioned { account: AccountNumber },
    Initialized {
        account: AccountNumber,
        request_id: String,
    },
}

/// Committed evidence of an idempotent account installation. The digest binds
/// its original name and program, not the account's later binding or controller.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionReceipt {
    pub account: AccountNumber,
    pub request_digest: [u8; 32],
}

#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InitializationReceipt {
    pub account: AccountNumber,
    pub controller: AccountNumber,
    pub request_id: String,
    pub binding_revision: u64,
}

pub const INITIALIZATION_KIND: &str = "initialization";
pub const INITIALIZATION_REASON: &str = "initialize";
pub const MAX_PROVISION_REQUEST_ID_BYTES: usize = 64;

// ---- codecs ---------------------------------------------------------------------------

pub fn encode_msg(m: &AgentMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<AgentMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &AgentQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<AgentQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &AgentReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<AgentReply, String> {
    sdk::wire::decode(b)
}
pub fn encode_assigned(a: &AgentAssigned) -> Vec<u8> {
    sdk::wire::encode(a)
}
pub fn decode_assigned(b: &[u8]) -> Result<AgentAssigned, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_program() -> Program {
        Program {
            steps: vec![
                Step::Query {
                    module: "chat".into(),
                    query: Value::Map(BTreeMap::from([(
                        "channel".into(),
                        Value::Map(BTreeMap::from([(
                            "channel_id".into(),
                            Value::Text("general".into()),
                        )])),
                    )])),
                    bind: "chan".into(),
                },
                Step::Call {
                    module: "chat".into(),
                    msg: Value::Map(BTreeMap::from([
                        (
                            "channel".into(),
                            Value::Ref(vec!["chan".into(), "id".into()]),
                        ),
                        ("bytes".into(), Value::Bytes(vec![1, 2, 3])),
                        ("count".into(), Value::Number(-3)),
                        ("big".into(), Value::Number(u64::MAX as i128)),
                        ("flag".into(), Value::Bool(true)),
                        ("nothing".into(), Value::Null),
                        ("list".into(), Value::List(vec![Value::Text("a".into())])),
                    ])),
                    bind: "posted".into(),
                    decode: Decode::Json,
                    on_failure: Continuation::Step(3),
                },
                Step::Dispatch {
                    recipe_id: "summarize".into(),
                    payload: Value::Ref(vec!["posted".into(), "applied".into(), "output".into()]),
                    bind: "summary".into(),
                    decode: Decode::Text,
                    on_failure: Continuation::Unhandled,
                },
                Step::Branch {
                    test: Predicate::All(vec![
                        Predicate::Defined(Value::Ref(vec!["posted".into(), "rejected".into()])),
                        Predicate::Not(Box::new(Predicate::Any(vec![Predicate::Equals {
                            left: Value::Ref(vec![REF_CHANGE.into(), "reason".into()]),
                            right: Value::Text("mention".into()),
                        }]))),
                    ]),
                    then: 4,
                    or: 5,
                },
                Step::Report {
                    recipient: Value::Ref(vec![
                        REF_CHANGE.into(),
                        "actor".into(),
                        "account".into(),
                    ]),
                    reason: Reason::Report,
                    detail: Value::Ref(vec!["posted".into(), "rejected".into(), "reason".into()]),
                },
                Step::Finish,
            ],
        }
    }

    #[test]
    fn program_round_trips_both_codecs() {
        let program = sample_program();
        let wire = sdk::wire::encode(&program);
        assert_eq!(sdk::wire::decode::<Program>(&wire).unwrap(), program);
        let record = borsh::to_vec(&program).unwrap();
        assert_eq!(borsh::from_slice::<Program>(&record).unwrap(), program);
    }

    /// the wire spelling programs are written in: snake_case tags, unit
    /// variants as bare strings, a reference as a path list.
    #[test]
    fn program_wire_spelling_is_pinned() {
        let program = Program {
            steps: vec![
                Step::Branch {
                    test: Predicate::Equals {
                        left: Value::Ref(vec!["change".into(), "reason".into()]),
                        right: Value::Text("mention".into()),
                    },
                    then: 1,
                    or: 2,
                },
                Step::Call {
                    module: "chat".into(),
                    msg: Value::Map(BTreeMap::from([("n".into(), Value::Number(7))])),
                    bind: "posted".into(),
                    decode: Decode::Json,
                    on_failure: Continuation::Unhandled,
                },
                Step::Finish,
            ],
        };
        assert_eq!(
            String::from_utf8(sdk::wire::encode(&program)).unwrap(),
            r#"{"steps":[{"branch":{"test":{"equals":{"left":{"ref":["change","reason"]},"right":{"text":"mention"}}},"then":1,"or":2}},{"call":{"module":"chat","msg":{"map":{"n":{"number":7}}},"bind":"posted","decode":"json","on_failure":"unhandled"}},"finish"]}"#
        );
    }

    #[test]
    fn msg_query_reply_assigned_round_trip() {
        let program = sample_program();
        for m in [
            AgentMsg::Provision {
                request_id: "bot".into(),
                name: "bot".into(),
                program: program.clone(),
            },
            AgentMsg::Replace {
                account: 2,
                program: program.clone(),
            },
            AgentMsg::Unbind { account: 2 },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)).unwrap(), m);
        }
        for q in [
            AgentQuery::Binding { account: 2 },
            AgentQuery::Invocation { account: 2, seq: 9 },
            AgentQuery::Invocations {
                account: 2,
                after: 0,
                limit: 16,
            },
        ] {
            assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        }
        let call_id = CallId {
            requester: "agent".into(),
            invocation: "2/9".into(),
            step: 1,
        };
        let view = InvocationView {
            account: 2,
            seq: 9,
            revision: 0,
            generation: 0,
            item: ItemRef {
                source: "attribution".into(),
                item: 4,
            },
            cause: Cause::Direct,
            status: Status::Running {
                step: 1,
                awaiting: Outstanding::Call(call_id),
            },
            bindings: BTreeMap::from([(
                "posted".into(),
                serde_json::to_value(CallResult::Rejected {
                    reason: "no".into(),
                })
                .unwrap(),
            )]),
        };
        for r in [
            AgentReply::Binding(Some(BindingView {
                account: 2,
                program,
                revision: 3,
            })),
            AgentReply::Invocation(Some(view.clone())),
            AgentReply::Invocations(vec![InvocationEntry {
                at: 1,
                invocation: InvocationView {
                    status: Status::Failed {
                        step: 2,
                        failure: Failure::Program(ProgramFault::FrameTooLarge { bytes: 9 }),
                    },
                    ..view
                },
            }]),
        ] {
            assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
        }
        let assigned = AgentAssigned::Provisioned { account: 2 };
        assert_eq!(
            decode_assigned(&encode_assigned(&assigned)).unwrap(),
            assigned
        );
    }

    /// the authority vocabulary spells on the wire as dispatch spells its
    /// refusals, and the byte decoding as a program writes it.
    #[test]
    fn authority_and_decoding_spell_on_the_wire() {
        let aborted = serde_json::to_value(Status::Aborted {
            at_step: 1,
            reason: Abort::StaleGeneration,
        })
        .unwrap();
        assert_eq!(
            aborted,
            serde_json::json!({"aborted": {"at_step": 1, "reason": "stale_generation"}})
        );
        for (reason, spelled) in [
            (Abort::Unbound, "unbound"),
            (Abort::Replaced, "replaced"),
            (Abort::Revoked, "revoked"),
            (Abort::Suspended, "suspended"),
            (Abort::StaleGeneration, "stale_generation"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), spelled);
        }
        assert_eq!(serde_json::to_value(Decode::Bytes).unwrap(), "bytes");
    }

    /// the frame's outcome views spell their tags exactly like the dispatch
    /// outcome they decode, so a program inspects `<bind>.rejected.reason`
    /// whichever plane the failure came from.
    #[test]
    fn outcome_views_are_tagged_like_their_outcomes() {
        let rejected = serde_json::to_value(CallResult::Rejected {
            reason: "no".into(),
        })
        .unwrap();
        assert_eq!(rejected["rejected"]["reason"], "no");
        let refused = serde_json::to_value(CallResult::Refused(Refusal::StaleGeneration)).unwrap();
        assert_eq!(refused["refused"], "stale_generation");
        let failed = serde_json::to_value(DispatchResult::Failed {
            reason: "timeout".into(),
        })
        .unwrap();
        assert_eq!(failed["failed"]["reason"], "timeout");
    }
}
