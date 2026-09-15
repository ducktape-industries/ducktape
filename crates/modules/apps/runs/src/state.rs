use super::{
    AgentSession, BTreeMap, DelegationState, DelegationStatus, Digest, Error,
    MAX_ACTIONS_PER_SESSION, MAX_DELEGATIONS_PER_RUN, MAX_REQUEST_ID_BYTES, PendingState,
    RUN_KEY_SEPARATOR, RunOrigin, SESSION_KEY_LEN, Sha256, StateRoot, WireSink, delegation_id_for,
    dispatch_id_for,
};
use sdk::codec;

// ---- canonical encoding -------------------------------------------------------
// u64-le counts, sorted keys, every field in declaration order: u64-le length
// prefixes for byte strings, single-byte discriminants for enums, a 0/1 tag
// byte for options, u64-le integers (via the shared `sdk::codec` writers). this
// is the exact preimage [`Module::root`] hashes, so a snapshot and the root that
// must authenticate it cannot drift. no version byte: this decoder accepts only
// the current encoding.

fn put_opt_string(out: &mut Vec<u8>, opt: &Option<String>) {
    codec::push_opt_str(out, opt.as_deref());
}

/// the committed sink, as a length-prefixed JSON blob — same shape as the
/// wire (`WireSink` is `runs`' own contract, not a wider committed-state one),
/// but always present here (never optional): every pending entry commits a
/// sink at dispatch, `Chain` included.
fn put_sink(out: &mut Vec<u8>, sink: &WireSink) {
    codec::push_bytes(
        out,
        &serde_json::to_vec(sink).expect("committed sink serializes"),
    );
}

fn take_sink(cur: &mut codec::Cursor) -> Result<WireSink, String> {
    serde_json::from_slice(&take_lp_bytes(cur)?)
        .map_err(|error| format!("snapshot sink failed to decode: {error}"))
}

fn put_origin(out: &mut Vec<u8>, origin: &RunOrigin) {
    match origin {
        RunOrigin::External(key) => {
            out.push(0);
            codec::push_bytes(out, key);
        }
        RunOrigin::Module(module) => {
            out.push(1);
            codec::push_bytes(out, module.as_bytes());
        }
        RunOrigin::System => out.push(2),
        RunOrigin::Program(account) => {
            out.push(3);
            out.extend_from_slice(&account.to_le_bytes());
        }
    }
}

pub(super) fn encode_committed(
    action_requests: &crate::receipts::Records,
    next_action_item: u64,
    pending: &BTreeMap<String, PendingState>,
    sessions: &BTreeMap<String, AgentSession>,
    delegations: &BTreeMap<String, DelegationState>,
    models: &BTreeMap<String, crate::ModelRecord>,
) -> Vec<u8> {
    let mut out = Vec::new();

    codec::push_bytes(&mut out, &sdk::wire::encode(action_requests));
    out.extend_from_slice(&next_action_item.to_le_bytes());

    out.extend_from_slice(&(pending.len() as u64).to_le_bytes());
    for (dispatch_id, p) in pending {
        codec::push_bytes(&mut out, dispatch_id.as_bytes());
        out.extend_from_slice(&p.account.to_le_bytes());
        out.extend_from_slice(&p.generation.to_le_bytes());
        codec::push_bytes(&mut out, &sdk::wire::encode(&p.cause));
        codec::push_bytes(&mut out, p.run_id.as_bytes());
        codec::push_bytes(&mut out, p.agent_id.as_bytes());
        codec::push_bytes(&mut out, p.workspace_agent_id.as_bytes());
        put_opt_string(&mut out, &p.delegation_id);
        codec::push_bytes(&mut out, p.channel_id.as_bytes());
        out.extend_from_slice(&p.anchor_seq.to_le_bytes());
        codec::push_opt_u64(&mut out, p.thread_root);
        put_opt_string(&mut out, &p.job_id);
        out.extend_from_slice(&p.job_claim_height.to_le_bytes());
        put_origin(&mut out, &p.requester);
        put_sink(&mut out, &p.sink);
        out.extend_from_slice(&p.created_at.to_le_bytes());
    }

    // the live agent sessions, keyed by run id. the action counter is the
    // spent budget AND the deterministic id salt, so it is committed like
    // every other field — a validator that replayed a different count would
    // mint different ids.
    out.extend_from_slice(&(sessions.len() as u64).to_le_bytes());
    for (run_id, s) in sessions {
        codec::push_bytes(&mut out, run_id.as_bytes());
        codec::push_bytes(&mut out, s.agent_id.as_bytes());
        codec::push_bytes(&mut out, &s.session_key);
        codec::push_bytes(&mut out, &s.lease.holder);
        out.extend_from_slice(&u64::from(s.lease.attempt).to_le_bytes());
        out.extend_from_slice(&s.opened_at.to_le_bytes());
        out.extend_from_slice(&u64::from(s.actions).to_le_bytes());
    }

    out.extend_from_slice(&(delegations.len() as u64).to_le_bytes());
    for (delegation_id, delegation) in delegations {
        codec::push_bytes(&mut out, delegation_id.as_bytes());
        codec::push_bytes(
            &mut out,
            &serde_json::to_vec(delegation).expect("delegation state serializes"),
        );
    }

    codec::push_bytes(&mut out, &sdk::wire::encode(models));
    out
}

/// the state-based commitment over the committed maps — shared by `root()`
/// and `install()` so the verification a snapshot must pass is definitionally
/// the same algorithm the live module answers with.
pub(super) fn committed_root(
    action_requests: &crate::receipts::Records,
    next_action_item: u64,
    pending: &BTreeMap<String, PendingState>,
    sessions: &BTreeMap<String, AgentSession>,
    delegations: &BTreeMap<String, DelegationState>,
    models: &BTreeMap<String, crate::ModelRecord>,
) -> StateRoot {
    StateRoot(
        Sha256::digest(encode_committed(
            action_requests,
            next_action_item,
            pending,
            sessions,
            delegations,
            models,
        ))
        .into(),
    )
}

// ---- canonical decoding (UNTRUSTED input) ---------------------------------
// bounds are validated against the remaining input BEFORE any allocation,
// keys must be strictly ascending (one encoding per state, uniqueness for
// free), unknown discriminants/tags and trailing bytes are rejected. never
// panics on malformed input.

// primitives delegate to the shared `sdk::codec::Cursor` (each accessor
// bounds-checks before it reads); the Cursor error is mapped to this module's
// `String` decode contract. the module-specific readers thread one `Cursor`
// through the whole decode.

fn take_byte(cur: &mut codec::Cursor, what: &str) -> Result<u8, String> {
    cur.byte(what).map_err(|e| e.to_string())
}

fn take_u64(cur: &mut codec::Cursor) -> Result<u64, String> {
    cur.u64("snapshot u64").map_err(|e| e.to_string())
}

fn take_lp_bytes(cur: &mut codec::Cursor) -> Result<Vec<u8>, String> {
    cur.bytes("snapshot bytes")
        .map(<[u8]>::to_vec)
        .map_err(|e| e.to_string())
}

fn take_lp_string(cur: &mut codec::Cursor) -> Result<String, String> {
    cur.string("snapshot string").map_err(|e| e.to_string())
}

fn take_opt_u64(cur: &mut codec::Cursor) -> Result<Option<u64>, String> {
    cur.opt_u64("snapshot opt u64").map_err(|e| e.to_string())
}

fn take_opt_string(cur: &mut codec::Cursor) -> Result<Option<String>, String> {
    match take_byte(cur, "snapshot opt tag")? {
        0 => Ok(None),
        1 => Ok(Some(take_lp_string(cur)?)),
        t => Err(format!("snapshot has unknown option tag {t}")),
    }
}

fn take_origin(cur: &mut codec::Cursor) -> Result<RunOrigin, String> {
    match take_byte(cur, "snapshot origin discriminant")? {
        0 => Ok(RunOrigin::External(take_lp_bytes(cur)?)),
        1 => Ok(RunOrigin::Module(take_lp_string(cur)?)),
        2 => Ok(RunOrigin::System),
        3 => Ok(RunOrigin::Program(take_u64(cur)?)),
        d => Err(format!("snapshot has unknown origin discriminant {d}")),
    }
}

/// a section count, bounded by what the remaining input could possibly hold
/// given each entry's minimum encoded size — rejected before the loop builds
/// anything (`Cursor::bound`).
fn take_count(cur: &mut codec::Cursor, min_entry_bytes: u64, what: &str) -> Result<u64, String> {
    let count = take_u64(cur)?;
    cur.bound(count, min_entry_bytes, what)
        .map_err(|e| e.to_string())?;
    Ok(count)
}

/// enforce strictly-ascending map keys while inserting.
fn insert_ascending<V>(map: &mut BTreeMap<String, V>, key: String, value: V) -> Result<(), String> {
    if let Some((last, _)) = map.iter().next_back()
        && last.as_str() >= key.as_str()
    {
        return Err("snapshot keys not strictly ascending".into());
    }
    map.insert(key, value);
    Ok(())
}

pub(super) fn contains_run_separator(value: &str) -> bool {
    value.contains(RUN_KEY_SEPARATOR)
}

pub(super) fn reject_run_separator(field: &str, value: &str) -> Result<(), Error> {
    if contains_run_separator(value) {
        return Err(Error::Module(format!(
            "{field} must not contain the reserved unit separator"
        )));
    }
    Ok(())
}

/// a decoded pending entry must derive exactly its own key: the dispatch id
/// is the hex sha256 of the run id its fields produce, and the field shapes
/// must match the chat/job keyspace they claim.
fn validate_decoded_pending(dispatch_id: &str, p: &PendingState) -> Result<(), String> {
    if contains_run_separator(&p.agent_id) {
        return Err("snapshot agent_id contains reserved unit separator".into());
    }
    if p.run_id.is_empty() {
        return Err("snapshot pending run id is empty".into());
    }
    if contains_run_separator(&p.workspace_agent_id) {
        return Err("snapshot workspace agent id contains reserved unit separator".into());
    }
    match &p.job_id {
        Some(job_id) => {
            if contains_run_separator(job_id) {
                return Err("snapshot job_id contains reserved unit separator".into());
            }
            if !p.channel_id.is_empty() || p.anchor_seq != 0 || p.thread_root.is_some() {
                return Err("snapshot job entry carries chat coordinates".into());
            }
        }
        None => {
            if p.job_claim_height != 0 {
                return Err("snapshot chat entry has non-zero job claim height".into());
            }
            if contains_run_separator(&p.channel_id) {
                return Err("snapshot channel_id contains reserved unit separator".into());
            }
        }
    }
    if dispatch_id != dispatch_id_for(&p.run_id()) {
        return Err("snapshot dispatch id does not match its run fields".into());
    }
    Ok(())
}

fn validate_decoded_delegations(
    pending: &BTreeMap<String, PendingState>,
    delegations: &BTreeMap<String, DelegationState>,
) -> Result<(), String> {
    let mut roots = BTreeMap::<&str, usize>::new();
    for (id, state) in delegations {
        let view = &state.view;
        if id != &view.delegation_id
            || id != &delegation_id_for(&view.caller_run_id, &view.request_id)
        {
            return Err("snapshot delegation id does not match its caller request".into());
        }
        if view.request_id.is_empty()
            || view.request_id.len() > MAX_REQUEST_ID_BYTES
            || contains_run_separator(&view.request_id)
        {
            return Err("snapshot delegation request id is invalid".into());
        }
        if view.callee_agent_id != state.request.agent_id {
            return Err("snapshot delegation callee does not match its request".into());
        }
        if !pending.contains_key(&dispatch_id_for(&view.root_run_id)) {
            return Err("snapshot delegation root is not in flight".into());
        }
        if view.status == DelegationStatus::Pending {
            let child = pending
                .get(&dispatch_id_for(&view.callee_run_id))
                .ok_or_else(|| "snapshot pending delegation has no callee run".to_string())?;
            if child.delegation_id.as_deref() != Some(id.as_str()) {
                return Err("snapshot callee run points at a different delegation".into());
            }
        }
        if view.status == DelegationStatus::Pending {
            *roots.entry(&view.root_run_id).or_default() += 1;
        }
    }
    if roots.values().any(|count| *count > MAX_DELEGATIONS_PER_RUN) {
        return Err("snapshot delegation tree exceeds its concurrency limit".into());
    }
    for pending_run in pending.values() {
        let Some(id) = pending_run.delegation_id.as_deref() else {
            continue;
        };
        let edge = delegations
            .get(id)
            .ok_or_else(|| "snapshot delegated run names no call edge".to_string())?;
        if edge.view.status != DelegationStatus::Pending
            || edge.view.callee_run_id != pending_run.run_id
        {
            return Err("snapshot delegated run does not match its pending call edge".into());
        }
    }
    Ok(())
}

/// a decoded session must be one a live module could have staged: the key IS
/// the run id, the key length is the one this module admits, and the counter is
/// within the budget the action lane enforces. `pending` is the run's own
/// existence check — a session may never outlive its run, so a snapshot whose
/// session names no in-flight run is not one any honest node could produce.
fn validate_decoded_session(
    pending: &BTreeMap<String, PendingState>,
    s: &AgentSession,
) -> Result<(), String> {
    if s.session_key.len() != SESSION_KEY_LEN {
        return Err("snapshot session key is not a 32-byte ed25519 key".into());
    }
    if s.lease.holder.is_empty() {
        return Err("snapshot session names no lease holder".into());
    }
    if contains_run_separator(&s.agent_id) {
        return Err("snapshot session agent_id contains reserved unit separator".into());
    }
    if s.actions > MAX_ACTIONS_PER_SESSION {
        return Err("snapshot session has spent more than its action budget".into());
    }
    let entry = pending
        .get(&dispatch_id_for(&s.run_id))
        .ok_or_else(|| "snapshot session names no in-flight run".to_string())?;
    if entry.agent_id != s.agent_id {
        return Err("snapshot session agent does not match its run".into());
    }
    Ok(())
}

type Committed = (
    crate::receipts::Records,
    u64,
    BTreeMap<String, PendingState>,
    BTreeMap<String, AgentSession>,
    BTreeMap<String, DelegationState>,
    BTreeMap<String, crate::ModelRecord>,
);

pub(super) fn decode_committed(bytes: &[u8]) -> Result<Committed, String> {
    // Each pending entry includes identity generation, causal provenance,
    // model/workspace coordinates, requester and the committed sink. The
    // minimum excludes variable bodies, which each decoder checks below.
    const MIN_PENDING_BYTES: u64 =
        8 + 8 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 8 + 8 + 1 + 1 + 8 + 1 + 8 + 8;
    const MIN_SESSION_BYTES: u64 = 8 + 8 + 8 + 8 + 8 + 8 + 8;
    const MIN_DELEGATION_BYTES: u64 = 8 + 8;

    let mut cur = codec::Cursor::new(bytes);
    let action_requests = sdk::wire::decode(&take_lp_bytes(&mut cur)?)?;
    let next_action_item = take_u64(&mut cur)?;

    let mut pending: BTreeMap<String, PendingState> = BTreeMap::new();
    let count = take_count(&mut cur, MIN_PENDING_BYTES, "pending")?;
    for _ in 0..count {
        let dispatch_id = take_lp_string(&mut cur)?;
        let account = take_u64(&mut cur)?;
        let generation = take_u64(&mut cur)?;
        let cause = sdk::wire::decode(&take_lp_bytes(&mut cur)?)?;
        let run_id = take_lp_string(&mut cur)?;
        let agent_id = take_lp_string(&mut cur)?;
        let workspace_agent_id = take_lp_string(&mut cur)?;
        let delegation_id = take_opt_string(&mut cur)?;
        let channel_id = take_lp_string(&mut cur)?;
        let anchor_seq = take_u64(&mut cur)?;
        let thread_root = take_opt_u64(&mut cur)?;
        let job_id = take_opt_string(&mut cur)?;
        let job_claim_height = take_u64(&mut cur)?;
        let requester = take_origin(&mut cur)?;
        let sink = take_sink(&mut cur)?;
        let created_at = take_u64(&mut cur)?;
        let entry = PendingState {
            account,
            generation,
            cause,
            run_id,
            agent_id,
            workspace_agent_id,
            delegation_id,
            channel_id,
            anchor_seq,
            thread_root,
            job_id,
            job_claim_height,
            requester,
            sink,
            created_at,
        };
        validate_decoded_pending(&dispatch_id, &entry)?;
        insert_ascending(&mut pending, dispatch_id, entry)?;
    }

    let mut sessions: BTreeMap<String, AgentSession> = BTreeMap::new();
    let count = take_count(&mut cur, MIN_SESSION_BYTES, "session")?;
    for _ in 0..count {
        let run_id = take_lp_string(&mut cur)?;
        let agent_id = take_lp_string(&mut cur)?;
        let session_key = take_lp_bytes(&mut cur)?;
        let holder = take_lp_bytes(&mut cur)?;
        let attempt = u32::try_from(take_u64(&mut cur)?)
            .map_err(|_| "snapshot session attempt exceeds u32".to_string())?;
        let opened_at = take_u64(&mut cur)?;
        let actions = u32::try_from(take_u64(&mut cur)?)
            .map_err(|_| "snapshot session action count exceeds u32".to_string())?;
        let session = AgentSession {
            run_id: run_id.clone(),
            agent_id,
            session_key,
            lease: crate::ExecutionLease { holder, attempt },
            opened_at,
            actions,
        };
        validate_decoded_session(&pending, &session)?;
        insert_ascending(&mut sessions, run_id, session)?;
    }

    let mut delegations: BTreeMap<String, DelegationState> = BTreeMap::new();
    let count = take_count(&mut cur, MIN_DELEGATION_BYTES, "delegation")?;
    for _ in 0..count {
        let id = take_lp_string(&mut cur)?;
        let state: DelegationState = serde_json::from_slice(&take_lp_bytes(&mut cur)?)
            .map_err(|error| format!("snapshot delegation failed to decode: {error}"))?;
        insert_ascending(&mut delegations, id, state)?;
    }
    validate_decoded_delegations(&pending, &delegations)?;
    super::conversations::validate_records(&action_requests)?;

    let models: BTreeMap<String, crate::ModelRecord> =
        sdk::wire::decode(&take_lp_bytes(&mut cur)?)?;
    for (id, record) in &models {
        if id != &record.agent_id || record.account == 0 {
            return Err("invalid model record".into());
        }
    }
    if cur.remaining() != 0 {
        return Err("snapshot has trailing bytes".into());
    }
    Ok((
        action_requests,
        next_action_item,
        pending,
        sessions,
        delegations,
        models,
    ))
}
