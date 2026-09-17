//! Every id a run mints, derived and nothing else.
//!
//! These are wire because they are DERIVATIONS, not state: the daemon and the
//! module must arrive at the same run id, dispatch id and message id from the
//! same inputs, and a replaying validator must arrive there again. No host
//! randomness, no wall clock — `sha2` and `format!`.

use sha2::{Digest, Sha256};

use crate::RESERVED_ID_SEPARATOR;

/// reserved delimiter separating run-key fields — the registry rejects agent
/// ids carrying it ([`RESERVED_ID_SEPARATOR`]), so run keys stay unambiguous.
pub const RUN_KEY_SEPARATOR: char = RESERVED_ID_SEPARATOR;

/// the turn-claim key: first creation in consensus order wins.
pub fn run_id_for(channel_id: &str, anchor_seq: u64, agent_id: &str) -> String {
    format!(
        "chat{RUN_KEY_SEPARATOR}{channel_id}{RUN_KEY_SEPARATOR}{anchor_seq}{RUN_KEY_SEPARATOR}{agent_id}"
    )
}

pub fn page_run_id_for(thread_id: &str, ordinal: u64, agent_id: &str) -> String {
    format!(
        "page{RUN_KEY_SEPARATOR}{thread_id}{RUN_KEY_SEPARATOR}{ordinal}{RUN_KEY_SEPARATOR}{agent_id}"
    )
}

/// the turn-claim key for a job-backed run.
pub fn job_run_id_for(job_id: &str, agent_id: &str, claim_height: u64) -> String {
    format!(
        "job{RUN_KEY_SEPARATOR}{job_id}{RUN_KEY_SEPARATOR}{agent_id}{RUN_KEY_SEPARATOR}{claim_height}"
    )
}

/// canonical pin over submitted job-spec bytes — the jobs event's `spec_hash`.
pub fn job_spec_hash(spec: &[u8]) -> Vec<u8> {
    Sha256::digest(spec).to_vec()
}

/// The chat message id of a run's reply. Hash the internal run key so its
/// reserved separators and arbitrary suffixes cannot enter the public id space.
pub fn reply_message_id(run_id: &str) -> String {
    format!("agent/{}", dispatch_id_for(run_id))
}

/// the chat message id of an agent's `chat.post_message` — distinct from
/// [`reply_message_id`] (the run's ONE reply) and per-slot unique, so the id is
/// free by construction unless a submitter squatted it (which the emit probe
/// catches). the slot is the action's lane slot: its index in the delivered
/// response, or `s{n}` for the nth action of the run's session.
pub fn post_message_id(run_id: &str, slot: &str) -> String {
    format!("agent/{}/post/{slot}", dispatch_id_for(run_id))
}

/// the dispatch-plane id of a run's dispatch. run ids carry the reserved
/// `\x1f` separator the dispatch module rejects in caller-chosen ids, so the
/// dispatch id is the run id's hex sha256 — fixed-width, always within the
/// dispatch id cap; the pending map is keyed by it.
pub fn dispatch_id_for(run_id: &str) -> String {
    hex(&Sha256::digest(run_id.as_bytes()))
}

/// Stable idempotency key for one caller-scoped agent call.
pub fn delegation_id_for(caller_run_id: &str, request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ducktape/delegation/v1\0");
    digest.update(caller_run_id.as_bytes());
    digest.update([0]);
    digest.update(request_id.as_bytes());
    hex(&digest.finalize())
}

/// A delegated run is not another chat turn. Give it a distinct run id keyed
/// by the call edge so the same peer may be called more than once in one turn.
pub fn delegated_run_id_for(delegation_id: &str, callee_agent_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ducktape/delegated-run/v1\0");
    digest.update(delegation_id.as_bytes());
    digest.update([0]);
    digest.update(callee_agent_id.as_bytes());
    format!("delegate/{}", hex(&digest.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub const MAX_AGENT_ID_LEN: usize = 63;

/// an agent id must be a legal DNS label: lowercase ASCII `[a-z0-9-]`, 1..=63
/// bytes, no leading/trailing hyphen. the id IS the agent's address — forge
/// attributes its commits to `<agent_id>@agents.duck` (`agents` is reserved in
/// duckdns, see `RESERVED_ROOT_LABELS`), so an id that is not a label cannot
/// round-trip. deliberately a COPY of duckdns's `validate_handle` shape rule
/// rather than a call into it: two consensus modules must not share an
/// admission rule that either could silently move (duckdns's reserved-label
/// list is its own business — an agent may be called `net`). the tests pin the
/// two rules to the same shape.
pub fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    if agent_id.is_empty() {
        return Err("agent_id must not be empty".into());
    }
    if agent_id.len() > MAX_AGENT_ID_LEN {
        return Err(format!(
            "agent_id exceeds {MAX_AGENT_ID_LEN} bytes: {} bytes",
            agent_id.len()
        ));
    }
    if agent_id.starts_with('-') || agent_id.ends_with('-') {
        return Err(format!(
            "agent_id must not start or end with a hyphen: {agent_id:?}"
        ));
    }
    if !agent_id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "agent_id must be a DNS label (lowercase [a-z0-9-]): {agent_id:?}"
        ));
    }
    Ok(())
}
