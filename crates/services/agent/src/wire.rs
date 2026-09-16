//! the node ↔ agent-daemon protocol: four commands down, four events up.
//!
//! Both directions ride the ONE ws connection the daemon dials at
//! `/v1/ws` — the same localhost surface the CLI uses. The daemon always
//! dials; the node never dials the daemon. That is what keeps a service plug
//! unprivileged: it needs no listening port, no address in the node's config,
//! and a node that has never heard from it simply has no interactive plane.
//!
//! ## why the session id is minted by the NODE
//!
//! The node owns the id because the id is also the correlation token: it names
//! the ws topic (`term:<id>`) subscribers are already attached to, it keys the
//! node's per-session metadata, and it lets an output frame that races the
//! create reply still find its session. A daemon-minted id would need a second
//! correlation field and a window where output has nowhere to land.
//!
//! ## what does NOT cross this boundary
//!
//! No key material and no credential secret. [`Credential`] is the *resolved
//! record* — a name, a duckdns authority, the local browser-gateway `via`, a
//! PUBLIC seal key and the account pubkey the grant is checked against. The
//! secret itself never leaves the lender's airlock; the daemon reconstructs a
//! `provider_host::AirlockConfig` from these public facts and the broker dials
//! for a sealed session exactly as the in-node path did.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// node → daemon. One variant per thing the node can ask of a pty.
///
/// `deny_unknown_fields`, like every type on this boundary: there is no live
/// network and no compat obligation, so a frame carrying a field this build
/// does not know is a SKEW REPORT, not something to tolerate. Tolerating it
/// silently drops whatever the other side thought it was saying — and on
/// [`Create`] that would mean running a session without a restriction the
/// sender believed it had imposed.
///
/// What the daemon DOES with a refused decode differs by direction, and only
/// one direction is finished. An [`Event`] the node cannot decode is answered
/// with a `BadFrame` naming the field. A [`Command`] the DAEMON cannot decode
/// is only dropped, so a skewed `TermCreate` leaves the node's create waiting
/// — see `classify` in `bin/node/src/agent/link.rs` for why that is left, and
/// what closing it looks like.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// spawn a session under the node-minted `session` id.
    TermCreate(Create),
    /// write raw bytes to a live session's pty. `data_b64` is base64 of the
    /// bytes — never the bytes themselves, and never logged either way.
    TermInput { session: String, data_b64: String },
    /// a window-size change on a live session.
    TermResize {
        session: String,
        cols: u16,
        rows: u16,
    },
    /// end a session now. Idempotent; an unknown id is a no-op.
    TermClose { session: String },
    /// attach a participant's collaboration input to ONE local provider
    /// session, named by the opaque `device` label the network already records.
    /// Answered by [`Event::MsgBound`] or [`Event::MsgBindRefused`].
    MsgBind(Bind),
    /// release a binding. `generation` must be the one currently held, so a
    /// stale attachment cannot detach the device that replaced it.
    MsgUnbind {
        conversation: String,
        participant: String,
        generation: u64,
    },
    /// place one immutable, already-admitted message into a bound session.
    /// Answered by one or more [`Event::MsgDelivery`] frames.
    ///
    /// Boxed: a `Deliver` carries a body, and an enum is as large as its
    /// largest variant everywhere it goes — on the link's lane, in the
    /// `Result` [`crate::messaging::route`] returns, and in every `TermInput`
    /// that shares this type.
    MsgDeliver(Box<Deliver>),
    /// the agreed network clock has advanced to `network_now`.
    ///
    /// A daemon owns no clock — deliberately, because expiry is a network fact
    /// and a laptop's wall clock is not one. Without this it could only ever
    /// judge a deadline against the value frozen onto the frame at admission,
    /// which goes stale the moment a message waits: a message queued behind an
    /// offline provider for a day would still look fresh. The node pushes the
    /// agreed value as it advances, and the daemon takes the larger of the two.
    MsgTime { network_now: u64 },
    /// re-report every delivery state this daemon durably holds for one
    /// binding, oldest sequence first. Answered by one [`Event::MsgDelivery`]
    /// per tracked item.
    ///
    /// A receipt is a fact only this daemon has: the node cannot re-derive one,
    /// and nothing re-sends it, so a receipt lost between here and the node's
    /// collaboration half is lost for good — the network would read `Queued`
    /// forever for a message a provider accepted. This is the reconciliation
    /// that closes that, and the journal is what makes it possible: the states
    /// are on disk, so a replay reports what was OBSERVED rather than what
    /// anyone remembers.
    ///
    /// It offers nothing to a provider and changes nothing here. It is a read
    /// of durable state, and it is idempotent: the receiving side refuses a
    /// transition already made.
    MsgReplay {
        conversation: String,
        participant: String,
    },
}

/// everything the daemon needs to spawn one session. The node has already
/// decided admission (who may create, with whose credential, at what limits);
/// this is the decision's output, not its input.
///
/// EVERY field is required. Nothing here defaults, deliberately: each of the
/// two that used to would have failed OPEN if a sender omitted it — an absent
/// `credential` reads as "run on the operator's own locally-resolved
/// credential", and absent `limits` as "the provider's defaults" — so a skewed
/// sender's silence would have widened this session's authority or its
/// resource ceiling. A decision's output must be stated, not inferred.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Create {
    /// the node-minted session id — also the workdir name and the ws topic.
    pub session: String,
    /// the provider tag to resolve (`claude`, `codex`, a test provider).
    pub provider: String,
    /// `true` runs the restricted (read-only, non-prompting) argv — the shared
    /// session's command-lane shape. `false` is the full solo TUI.
    pub restricted: bool,
    /// cpu/mem ceilings for the sandbox. An EMPTY map means the provider's
    /// defaults — and must be written as one, never omitted.
    pub limits: BTreeMap<String, u64>,
    /// a lent credential resolved from committed state, or `null` for a session
    /// running on the operator's own locally-resolved credential. `null` is a
    /// statement the sender makes; it is not what silence means.
    ///
    /// `deserialize_with` is what makes that true. Serde lets an `Option` field
    /// go MISSING even with no `#[serde(default)]` — absent decodes to `None`,
    /// which here is the fail-open "use the operator's own credential". Naming
    /// a deserializer suppresses that fallback, so an omitted field is a
    /// `missing field` error like every other.
    #[serde(deserialize_with = "Option::deserialize")]
    pub credential: Option<Credential>,
}

/// a consensus-resolved credential record, in transit to the daemon's broker.
/// A field-for-field mirror of `provider_host::ResolvedCredential` — mirrored
/// rather than re-exported so this protocol stays a stable, inspectable shape
/// that a third-party plug can implement without linking provider-host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub name: String,
    pub kind: CredentialKind,
    /// the owner's duckdns handle (`airlock.<handle>.duck`).
    pub authority: String,
    /// the local node's browser-gateway base the request routes through.
    pub via: String,
    /// the owner's PUBLIC seal key, pinned as the broker's trust anchor.
    pub seal_pk: [u8; 32],
}

/// which vendor a credential is for — the one airlock vocabulary, serialized
/// snake_case on this wire exactly as the lender serializes it.
pub use provider_host::CredentialKind;

/// attach one participant's collaboration input to a local provider session.
///
/// `device` is the SAME opaque label the collaboration module records on its
/// binding — and it is deliberately all this frame carries about the target.
/// Which local session that label names is the daemon's own business: it
/// resolves the label through its local attachment map
/// (`messaging::Attachments`), which no node reads and nothing serializes onto
/// a network payload. A provider session id, thread id, socket path or token
/// therefore never reaches even this localhost link, let alone the mesh.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    pub conversation: String,
    pub participant: String,
    /// this attachment's generation. Strictly increasing per
    /// (conversation, participant): a bind at or below the generation already
    /// held is refused [`BindRefusal::StaleGeneration`], so two devices cannot
    /// both claim one participant's input.
    pub generation: u64,
    /// the opaque device label to resolve locally. Never a path or session id.
    pub device: String,
}

/// one immutable, already-admitted message to place into a bound session.
///
/// The network decided admission, authority and ordering before this exists.
/// Nothing here is re-authorized by the daemon and nothing here claims a task:
/// [`Deliver::task`] is a REFERENCE the model is told about, never a claim the
/// daemon makes on the sender's behalf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Deliver {
    pub conversation: String,
    /// the RECIPIENT participant, whose binding this delivery selects.
    pub participant: String,
    /// this message's position in the conversation's committed event sequence.
    /// With `conversation` it is the receipt key the module acknowledges on.
    pub seq: u64,
    /// the recipient BINDING generation this delivery is for. A delivery
    /// naming a generation the daemon no longer holds is fenced, so a message
    /// aimed at a replaced attachment never reaches the device that replaced
    /// it.
    pub binding_generation: u64,
    /// the chat message id, stable across every retry of this delivery. Echoed
    /// on every receipt, so a reader can tell which send it answers.
    pub message_id: String,
    /// the sender participant, as the authenticated envelope resolved it. It
    /// reaches the model inside a wrapper, as peer-supplied content — never as
    /// an instruction with the standing of its operator.
    pub sender: String,
    pub kind: Kind,
    pub task: Option<TaskRef>,
    /// the conversation sequence this replies to, if any.
    pub reply_to: Option<u64>,
    pub body: String,
    pub references: Vec<Reference>,
    /// the agreed network clock value this message stops being deliverable at.
    ///
    /// Deliberately UNITLESS here: the agreed clock is a logical one
    /// (`sdk::Env::consensus_time`), so the daemon must never convert it,
    /// interpret it as milliseconds, or compare it against a laptop's wall
    /// clock. It compares it against `network_now` below and nothing else.
    pub expires_at: u64,
    /// the agreed network clock as of admission, in the same unit as
    /// `expires_at`. Sending it is what lets a daemon with no clock of its own
    /// decide expiry without ever inventing one.
    pub network_now: u64,
    /// may steer an ACTIVE turn where the adapter supports it. Unsupported
    /// steering stays visibly queued; it never falls back to keystrokes.
    pub urgent: bool,
}

/// what a message is for. Mirrors the collaboration module's `MessageKind`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Notice,
    Question,
    TaskRequest,
    TaskUpdate,
    Result,
}

/// the task a message is about, and the attempt the sender believed was
/// current. The daemon carries it into the wrapper; it never claims it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    pub id: String,
    pub expected_attempt: u64,
}

/// one reference a message carries: the collaboration module's CLOSED set,
/// decoded here rather than translated.
///
/// A `{kind, value}` pair would re-open at this hop exactly what the module
/// closed at admission — `file:///tmp/private`, a mutable branch name, and
/// every scheme nobody thought of — because this is the last decoder before a
/// body is written into somebody's session. There is no arm for those, so the
/// class is unreachable instead of filtered, and a frame carrying one fails to
/// decode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reference {
    /// a Forge commit by its immutable object id. A branch or tag name moves,
    /// so it is not a reference and has no arm.
    Commit { repo: String, commit: String },
    /// content-addressed bytes, by lowercase hex sha256.
    Blob { hash: String },
    /// a scoped `duck://` link. Relaying one grants the recipient nothing: it
    /// fetches under its own authority or not at all.
    Duck { url: String },
}

/// one recipient's delivery state, exactly the collaboration module's
/// `DeliveryState` minus `Stored` — which is the network's own admission fact
/// and never something a daemon reports.
///
/// `AdapterAccepted` means THE PROVIDER'S INPUT INTERFACE accepted it. It does
/// not mean the model read it, understood it, acted on it, or claimed a task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// durably queued by this daemon, not yet offered to a provider.
    Queued,
    /// the provider's input interface accepted it, and said so itself.
    AdapterAccepted,
    /// a provider or local approval barrier is holding it. Exposed, not
    /// overridden.
    Held,
    /// the provider refused it, for a nameable reason.
    Refused,
    /// its deadline passed on the agreed clock before it was accepted.
    Expired,
    /// the daemon cannot establish whether the input was accepted. The honest
    /// answer whenever the selected interface supplies no acceptance signal —
    /// never upgraded by a write completing or a process exiting 0.
    DeliveryUnknown,
}

/// why a bind was refused. A stable snake_case token on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindRefusal {
    /// a binding at or above this generation is already held: the attachment
    /// this one would replace is the current one, or newer.
    StaleGeneration,
    /// no local attachment carries that device label. The operator attaches a
    /// device on the machine that owns the session; the network never names it.
    UnknownDevice,
    /// the label resolves, but the session behind it cannot be reached (no
    /// registry entry, a dead process, an unreadable key).
    SessionUnreachable,
    /// this daemon already holds its ceiling of bindings.
    AtCapacity,
}

/// what a bound session says it can do. The node publishes this so a sender
/// learns, before it sends, that (say) this binding will never report
/// acceptance — so `DeliveryUnknown` from it is the expected answer and not a
/// fault to chase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// input may be offered while a turn is running.
    pub accepts_while_busy: bool,
    /// an idle session can be woken by a question or task request.
    pub wakes_idle: bool,
    /// the interface supplies an acceptance signal of its own. When false,
    /// every write that draws no explicit refusal settles `DeliveryUnknown`.
    pub reports_acceptance: bool,
    /// an active turn can be steered, under an expected-turn precondition.
    pub steers_active_turn: bool,
}

/// daemon → node. The lifecycle of a session, as the process that owns it sees
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// the pty is live. Answers exactly one [`Command::TermCreate`].
    TermCreated { session: String },
    /// the create failed, for one nameable reason. Answers exactly one
    /// [`Command::TermCreate`].
    TermRefused {
        session: String,
        reason: Refusal,
        detail: String,
    },
    /// One chunk of pty output, base64. The pump can have a read in flight
    /// during teardown; receivers discard output after accepting TermEnded
    /// for that session.
    TermOutput { session: String, chunk_b64: String },
    /// the session is over: the child exited, an explicit close landed, or the
    /// wall-clock ceiling fired. Exactly one per created session, emitted by
    /// whichever path actually removed it, so a close racing an EOF cannot
    /// double-terminate.
    TermEnded { session: String },
    /// the binding is live, and this is what it can do.
    MsgBound {
        conversation: String,
        participant: String,
        generation: u64,
        capabilities: Capabilities,
    },
    /// the bind was refused, for one nameable reason.
    MsgBindRefused {
        conversation: String,
        participant: String,
        generation: u64,
        reason: BindRefusal,
    },
    /// one recipient's delivery state changed.
    ///
    /// Self-describing on purpose: `conversation` + `seq` is the module's
    /// receipt key, and `sender` + `message_id` name WHICH send this answers,
    /// so a reader correlating its own outstanding sends never mixes two up.
    /// `binding_generation` is the attachment that produced it — the module
    /// refuses one from a generation older than the record's own, so a
    /// returning stale device cannot overwrite the current state.
    MsgDelivery {
        conversation: String,
        participant: String,
        seq: u64,
        binding_generation: u64,
        sender: String,
        message_id: String,
        state: State,
        /// a stable snake_case token, or `null`. Never prose, never a path,
        /// never a token, never a body excerpt.
        reason: Option<String>,
    },
}

/// why a create refused. A stable snake_case token on the wire: the node maps
/// it to an HTTP status and the mesh maps it to a refusal reason, so the
/// 503-vs-`spawn_failed` diagnosis ladder keeps its rungs across the process
/// boundary.
///
/// There is deliberately no `no_sandbox` variant. A daemon with no runnable
/// sandbox refuses to start at all, so "no sandbox" can only ever mean "no
/// agent daemon is attached to this node" — a fact only the node can observe,
/// and which it answers without asking anyone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// the daemon's concurrent-session cap is reached.
    AtCapacity,
    /// no provider in this daemon's set serves the requested tag.
    UnknownProvider,
    /// the interactive spawn itself failed (guest artifacts absent, no
    /// `/dev/kvm`, the guest never dialled back, …).
    SpawnFailed,
}

/// a session id is 16 lowercase hex — the shape the node mints. Checked on
/// arrival at the daemon, because the id becomes a directory name there, and by
/// the mesh term plane before a grain reaches a ring. Lives here rather than in
/// either consumer: the id is this protocol's, so its validity rule is too.
pub fn valid_session(session: &str) -> bool {
    session.len() == 16
        && session.bytes().all(|byte| {
            byte.is_ascii_digit() || byte.is_ascii_lowercase() && byte.is_ascii_hexdigit()
        })
}

impl Refusal {
    /// the stable token — what the mesh refusal reason and the logs carry.
    pub fn token(self) -> &'static str {
        match self {
            Refusal::AtCapacity => "at_capacity",
            Refusal::UnknownProvider => "unknown_provider",
            Refusal::SpawnFailed => "spawn_failed",
        }
    }
}

impl BindRefusal {
    /// the stable token the logs carry.
    pub fn token(self) -> &'static str {
        match self {
            BindRefusal::StaleGeneration => "stale_generation",
            BindRefusal::UnknownDevice => "unknown_device",
            BindRefusal::SessionUnreachable => "session_unreachable",
            BindRefusal::AtCapacity => "at_capacity",
        }
    }
}

impl State {
    /// the stable token the logs carry — the same spelling as the wire.
    pub fn token(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::AdapterAccepted => "adapter_accepted",
            State::Held => "held",
            State::Refused => "refused",
            State::Expired => "expired",
            State::DeliveryUnknown => "delivery_unknown",
        }
    }

    /// whether this state is terminal — nothing later may move it.
    ///
    /// The spec's machine is
    /// `Queued -> AdapterAccepted | Held | Refused | Expired | DeliveryUnknown`,
    /// `Held -> AdapterAccepted | Refused | Expired`, and
    /// `DeliveryUnknown -> AdapterAccepted | Expired`. So a hold being released
    /// and an unknown being RECONCILED are both legal later moves, while an
    /// accepted, refused or expired record is finished. `DeliveryUnknown`
    /// leaving is legal only on evidence: this daemon reaches
    /// `AdapterAccepted` from a provider's own acceptance signal and from
    /// nothing else, so "not terminal" is not a licence to guess.
    pub fn terminal(self) -> bool {
        match self {
            State::Queued | State::Held | State::DeliveryUnknown => false,
            State::AdapterAccepted | State::Refused | State::Expired => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_tokens_are_the_ones_the_mesh_already_publishes() {
        // these three strings are a wire contract: `term_plane`'s guest surfaces
        // them verbatim as `host refused: <reason>: <detail>`, and the pty CLI's
        // diagnosis ladder reads them. Renaming one is a wire change.
        assert_eq!(Refusal::AtCapacity.token(), "at_capacity");
        assert_eq!(Refusal::UnknownProvider.token(), "unknown_provider");
        assert_eq!(Refusal::SpawnFailed.token(), "spawn_failed");
    }

    #[test]
    fn a_command_round_trips_through_json() {
        let create = Command::TermCreate(Create {
            session: "abc".into(),
            provider: "claude".into(),
            restricted: true,
            limits: BTreeMap::from([("cpu".to_string(), 2)]),
            credential: None,
        });
        let text = serde_json::to_string(&create).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&text).unwrap(), create);
        // the tag is what the node's ws writer and a third-party plug agree on.
        assert!(text.contains(r#""op":"term_create""#), "{text}");
    }

    #[test]
    fn an_event_round_trips_through_json() {
        let ended = Event::TermEnded {
            session: "abc".into(),
        };
        let text = serde_json::to_string(&ended).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), ended);
        assert!(text.contains(r#""op":"term_ended""#), "{text}");
    }

    #[test]
    fn a_session_id_is_sixteen_lowercase_hex() {
        assert!(valid_session("0123456789abcdef"));
        // the id becomes a directory name on the daemon: nothing that could
        // walk out of the workdir root may pass.
        for bad in [
            "",
            "0123456789abcde",   // short
            "0123456789abcdef0", // long
            "0123456789ABCDEF",  // upper
            "../../etc/passwd",
            "0123456789abcde/",
        ] {
            assert!(!valid_session(bad), "must reject {bad:?}");
        }
    }

    /// Every skew this protocol can see is a NAMED decode error. There is no
    /// live network, so nothing here may be tolerant: an unknown field and an
    /// absent field both stop the frame instead of quietly becoming a default.
    ///
    /// This is the whole justification the build gate's deletion rests on — the
    /// gate was a whole-connection version check standing in for per-frame
    /// decoding, and per-frame decoding only replaces it if it actually
    /// refuses. Before this, an added field decoded `Ok` and was dropped: a
    /// spend cap added to [`Credential`] would have been silently discarded and
    /// the session run without it.
    #[test]
    fn skew_in_either_direction_is_a_named_decode_error() {
        let full = r#"{"op":"term_create","session":"a","provider":"claude","restricted":false,"limits":{},"credential":null}"#;
        serde_json::from_str::<Command>(full).expect("the current shape decodes");

        // a NEWER sender's extra field — on the command, on the create, and on
        // a lent credential (where dropping one is dropping a restriction).
        //
        // `cred` must be EXACTLY the current shape and nothing more. It once
        // carried a since-deleted `account` field, and serde reports the FIRST
        // unknown field it meets: the assertion below went on passing while the
        // `spend_cap` arm — the one this test exists for — stopped running
        // entirely. Deleting `"spend_cap":10` from the composed string is the
        // check that it still does.
        let cred = r#""credential":{"name":"n","kind":"claude","authority":"a","via":"v","seal_pk":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]"#;
        for newer in [
            r#"{"op":"term_create","session":"a","provider":"claude","restricted":false,"limits":{},"credential":null,"future":1}"#.to_string(),
            r#"{"op":"term_close","session":"a","force":true}"#.to_string(),
            format!(r#"{{"op":"term_create","session":"a","provider":"claude","restricted":false,"limits":{{}},{cred},"spend_cap":10}}}}"#),
        ] {
            let error = serde_json::from_str::<Command>(&newer)
                .expect_err(&format!("an unknown field must refuse: {newer}"));
            assert!(error.to_string().contains("unknown field"), "{error}");
        }

        // an OLDER sender's omission. Both of these used to decode to a
        // fail-open default: no credential (the operator's own), no limits.
        for older in [
            r#"{"op":"term_create","session":"a","provider":"claude","restricted":false,"limits":{}}"#,
            r#"{"op":"term_create","session":"a","provider":"claude","restricted":false,"credential":null}"#,
        ] {
            let error = serde_json::from_str::<Command>(older)
                .expect_err(&format!("a missing field must refuse: {older}"));
            assert!(error.to_string().contains("missing field"), "{error}");
        }

        // and the daemon → node direction, same rule.
        let event = serde_json::from_str::<Event>(r#"{"op":"term_ended","session":"a","code":0}"#)
            .expect_err("an unknown field must refuse on an event too");
        assert!(event.to_string().contains("unknown field"), "{event}");
    }

    fn a_deliver() -> Deliver {
        Deliver {
            conversation: "conv-1".into(),
            participant: "p-recipient".into(),
            seq: 7,
            binding_generation: 3,
            message_id: "m-7".into(),
            sender: "p-sender".into(),
            kind: Kind::Question,
            task: Some(TaskRef {
                id: "job-9".into(),
                expected_attempt: 4,
            }),
            reply_to: Some(5),
            body: "does the review cover the migration?".into(),
            references: vec![Reference::Commit {
                repo: "ducktape".into(),
                commit: "deadbeef".into(),
            }],
            expires_at: 1_200,
            network_now: 1_000,
            urgent: false,
        }
    }

    #[test]
    fn a_messaging_command_round_trips_through_json() {
        for command in [
            Command::MsgBind(Bind {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
                generation: 3,
                device: "laptop-a".into(),
            }),
            Command::MsgUnbind {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
                generation: 3,
            },
            Command::MsgDeliver(Box::new(a_deliver())),
            Command::MsgTime { network_now: 1_000 },
            Command::MsgReplay {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
            },
        ] {
            let text = serde_json::to_string(&command).expect("encodes");
            assert_eq!(serde_json::from_str::<Command>(&text).unwrap(), command);
        }
    }

    #[test]
    fn a_messaging_event_round_trips_through_json() {
        for event in [
            Event::MsgBound {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
                generation: 3,
                capabilities: Capabilities {
                    accepts_while_busy: true,
                    wakes_idle: true,
                    reports_acceptance: false,
                    steers_active_turn: false,
                },
            },
            Event::MsgBindRefused {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
                generation: 1,
                reason: BindRefusal::StaleGeneration,
            },
            Event::MsgDelivery {
                conversation: "conv-1".into(),
                participant: "p-recipient".into(),
                seq: 7,
                binding_generation: 3,
                sender: "p-sender".into(),
                message_id: "m-7".into(),
                state: State::DeliveryUnknown,
                reason: Some("no_acceptance_signal".into()),
            },
        ] {
            let text = serde_json::to_string(&event).expect("encodes");
            assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
        }
    }

    /// The binding frame carries an OPAQUE device label and nothing else about
    /// the target. Which local session that label names is resolved on the
    /// device that owns it, so no provider session id, thread id, socket path
    /// or token is on this link at all — let alone on a network payload.
    ///
    /// This is a shape assertion, not a comment: adding a `session_id` or
    /// `socket` field to [`Bind`] to "make attach easier" is the mistake it
    /// exists to fail.
    #[test]
    fn a_bind_frame_names_a_device_label_and_nothing_about_the_session() {
        let text = serde_json::to_string(&Command::MsgBind(Bind {
            conversation: "conv-1".into(),
            participant: "p-recipient".into(),
            generation: 3,
            device: "laptop-a".into(),
        }))
        .expect("encodes");
        let fields: serde_json::Value = serde_json::from_str(&text).expect("decodes as json");
        // sorted, because what is under test is WHICH fields exist and not the
        // order serde_json happens to hold them in.
        let mut keys: Vec<&str> = fields
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["conversation", "device", "generation", "op", "participant"],
            "the bind frame grew a field: {text}"
        );
        for leak in ["session_id", "thread", "socket", "token", "path", "pid"] {
            assert!(
                !text.contains(leak),
                "a bind frame must not carry {leak}: {text}"
            );
        }
    }

    /// The reference set is closed at this hop too, and closed the same way the
    /// module closes it: by having no arm, not by filtering a string.
    ///
    /// This is the last decoder before a body is written into somebody's
    /// session, so a `{kind, value}` pair here would re-open the whole class
    /// the module made unreachable — a `file://` path, a mutable branch alias,
    /// a scheme nobody thought of. A frame carrying one does not decode.
    #[test]
    fn a_reference_this_daemon_cannot_name_does_not_decode() {
        for forged in [
            r#"{"local_path":{"path":"/tmp/private"}}"#,
            r#"{"commit":{"repo":"ducktape","branch":"main"}}"#,
            r#"{"blob":{"hash":"abc","extra":"x"}}"#,
            r#"{"duck":{"url":"file:///tmp/private","grant":"read"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Reference>(forged).is_err(),
                "this reference must not decode: {forged}"
            );
        }
        // and the three the module admits still do.
        for named in [
            r#"{"commit":{"repo":"ducktape","commit":"deadbeef"}}"#,
            r#"{"blob":{"hash":"abc"}}"#,
            r#"{"duck":{"url":"duck://conv/1"}}"#,
        ] {
            serde_json::from_str::<Reference>(named)
                .unwrap_or_else(|error| panic!("{named} must decode: {error}"));
        }
    }

    /// A receipt names the send it answers — both halves. `conversation`+`seq`
    /// is the module's receipt key; `sender`+`message_id` is what a reader
    /// correlates its own outstanding sends against.
    #[test]
    fn a_receipt_names_which_send_it_answers() {
        let text = serde_json::to_string(&Event::MsgDelivery {
            conversation: "conv-1".into(),
            participant: "p-recipient".into(),
            seq: 7,
            binding_generation: 3,
            sender: "p-sender".into(),
            message_id: "m-7".into(),
            state: State::Held,
            reason: Some("provider_hold".into()),
        })
        .expect("encodes");
        for named in [
            r#""conversation":"conv-1""#,
            r#""seq":7"#,
            r#""binding_generation":3"#,
            r#""sender":"p-sender""#,
            r#""message_id":"m-7""#,
            r#""state":"held""#,
        ] {
            assert!(text.contains(named), "a receipt must carry {named}: {text}");
        }
    }

    #[test]
    fn the_delivery_tokens_are_the_states_the_module_records() {
        // these spellings are the wire contract with the collaboration
        // module's `DeliveryState`; renaming one is a wire change.
        assert_eq!(State::Queued.token(), "queued");
        assert_eq!(State::AdapterAccepted.token(), "adapter_accepted");
        assert_eq!(State::Held.token(), "held");
        assert_eq!(State::Refused.token(), "refused");
        assert_eq!(State::Expired.token(), "expired");
        assert_eq!(State::DeliveryUnknown.token(), "delivery_unknown");
        // and the json spelling is the same one, so a log line and a frame
        // never disagree about what happened.
        for state in [
            State::Queued,
            State::AdapterAccepted,
            State::Held,
            State::Refused,
            State::Expired,
            State::DeliveryUnknown,
        ] {
            let text = serde_json::to_string(&state).expect("encodes");
            assert_eq!(text, format!("\"{}\"", state.token()));
        }
    }

    #[test]
    fn only_accepted_refused_and_expired_are_terminal() {
        // the spec's machine lets a hold be released and an unknown be
        // reconciled; treating either as finished would strand it forever.
        assert!(!State::Queued.terminal());
        assert!(!State::Held.terminal());
        assert!(!State::DeliveryUnknown.terminal());
        assert!(State::AdapterAccepted.terminal());
        assert!(State::Refused.terminal());
        assert!(State::Expired.terminal());
    }
}
