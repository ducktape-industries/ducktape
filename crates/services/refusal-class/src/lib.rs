//! The classes a refusal token names, and the rule for minting one.
//!
//! `sdk::refusal` re-exports every constant here and owns the frame a refusal
//! crosses the component boundary in; this crate holds only the words, so a
//! crate that must not link sdk names the same ones.
//!
//! The rule: a token names the CLASS of failure, which is the same as naming
//! how a caller recovers. Two refusals share a token exactly when a caller
//! recovers from them the same way. The sentence names the specific thing (the
//! id, the cap, the state). The test for a new token: would a second call
//! site, refusing a different thing with a different sentence, reuse this
//! word? If not, it is a sentence in snake_case, not a token. A token never
//! contains the module's name (the route already says which module answered)
//! and never contains the subject (`board_full` is `capacity` + "Board storage
//! limit reached.").
//!
//! Canonical classes (constants in this module; use the constant, never the
//! literal):
//!
//! | constant | token | the caller recovers by |
//! |---|---|---|
//! | `NOT_FOUND` | `not_found` | naming a thing that exists (id, key, path, account, sibling module). |
//! | `ALREADY_EXISTS` | `already_exists` | creating under a different id, or treating the create as done. Includes a request or operation id reused with DIFFERENT work: retrying that id can never succeed. |
//! | `STALE` | `stale` | re-reading and retrying: base revision, cursor, claim or turn the caller sent is behind the module's. |
//! | `WRONG_STATE` | `wrong_state` | changing the thing's state first: it exists but is archived, paused, deleted, detached, not drained, sealed. |
//! | `INVALID_INPUT` | `invalid_input` | fixing the request: malformed, empty, out of range, undecodable, breaks a static rule; retrying unchanged can never succeed. |
//! | `CAPACITY` | `capacity` | sending less or removing something: a count or size bound of the store, the request or one dispatch's work budget is hit. |
//! | `NOT_YET` | `not_yet` | waiting: the same request, unchanged, succeeds after a point the sentence names in the module's own clock (a view, a height, a deadline). |
//! | `EXHAUSTED` | `exhausted` | nothing: a monotonic counter (revision, cursor, sequence, marker) cannot advance again; permanent. |
//! | `UNAUTHORIZED` | `unauthorized` | acting as someone else: the actor may not do this to this thing. |
//! | `UNSUPPORTED` | `unsupported` | configuring: the module or this deployment does not provide the op (a sibling not wired, a feature off). |
//! | `CORRUPT` | `corrupt` | an operator: stored state or an index failed an invariant; not caused by the request. Never mapped to `not_found`. |
//! | `UNEXPECTED_REPLY` | `unexpected_reply` | an operator: a sibling module answered a shape or value this module does not accept; the sibling's owner looks. |
//!
//! Host-reserved tokens, produced only by wasm-host / module-sdk at the
//! boundary, never by a module: `trap` (the guest trapped; sentence is the
//! trap), `unframed_refusal` (a peer refused with a string that is not
//! framed). Keep them as constants too (`TRAP`, `UNFRAMED_REFUSAL`).
//!
//! Who may mint what. The frame carries no refuser, so a caller attributes a
//! refusal to the module it addressed and may act on its sentence (a canvas
//! shows a boards `stale` sentence as the other writer's words). That is safe
//! only because the classes about the MODULE'S OWN STATE — `not_found`,
//! `already_exists`, `stale`, `wrong_state`, `not_yet`, `unauthorized` — are
//! minted by the addressed module and by nothing in front of it: no host hop
//! (node admission, routing, `wasm-host`) ever answers with one. A host hop
//! refuses with a host-reserved token, with a host-specific token of its own,
//! or with a class about the request and the machinery (`invalid_input`,
//! `capacity`, `unsupported`, `corrupt`), whose recovery is the same whoever
//! says it. The inverse holds too: a module never produces a host-reserved
//! token.
//!
//! Domain classes: allowed only when no canonical class matches AND the
//! caller's recovery differs from every row above; named for the recovery, not
//! the subject; no module name; must be reusable by a second site. A module
//! that needs one puts the constant in its own `<id>-wire` crate next to its
//! request types so the view that branches on it links the same constant.
//! Expect few (modules counted 58 candidates; most will fold into the table
//! once looked at as recovery rather than subject).

#![no_std]

/// naming a thing that exists (id, key, path, account, sibling module).
pub const NOT_FOUND: &str = "not_found";
/// creating under a different id, or treating the create as done.
pub const ALREADY_EXISTS: &str = "already_exists";
/// re-reading and retrying: what the caller sent is behind the module.
pub const STALE: &str = "stale";
/// changing the thing's state first: it exists, in a state that refuses this.
pub const WRONG_STATE: &str = "wrong_state";
/// fixing the request: retrying it unchanged can never succeed.
pub const INVALID_INPUT: &str = "invalid_input";
/// sending less or removing something: a count, size or work bound is hit.
pub const CAPACITY: &str = "capacity";
/// waiting: the same request succeeds after a point the sentence names.
pub const NOT_YET: &str = "not_yet";
/// nothing: a monotonic counter cannot advance again; permanent.
pub const EXHAUSTED: &str = "exhausted";
/// acting as someone else: the actor may not do this to this thing.
pub const UNAUTHORIZED: &str = "unauthorized";
/// configuring: the module or this deployment does not provide the op.
pub const UNSUPPORTED: &str = "unsupported";
/// an operator: stored state or an index failed an invariant.
pub const CORRUPT: &str = "corrupt";
/// an operator: a sibling module answered a shape or value this one refuses.
pub const UNEXPECTED_REPLY: &str = "unexpected_reply";
/// host-reserved: the guest trapped; the sentence is the trap.
pub const TRAP: &str = "trap";
/// host-reserved: a peer refused with a string that is not framed.
pub const UNFRAMED_REFUSAL: &str = "unframed_refusal";

#[cfg(test)]
mod tests {
    use super::*;

    /// two constants spelling one token would merge two recoveries into one.
    #[test]
    fn no_two_classes_share_a_token() {
        let all = [
            NOT_FOUND,
            ALREADY_EXISTS,
            STALE,
            WRONG_STATE,
            INVALID_INPUT,
            CAPACITY,
            NOT_YET,
            EXHAUSTED,
            UNAUTHORIZED,
            UNSUPPORTED,
            CORRUPT,
            UNEXPECTED_REPLY,
            TRAP,
            UNFRAMED_REFUSAL,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
