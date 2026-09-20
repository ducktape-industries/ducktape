//! invite/membership/governance/upgrade scenarios against the sim's opt-in
//! `--with-valset` genesis: the whole admitMember/promoteMember/demoteMember
//! console flow plus the /upgrade skill's arm/abort semantics, driven
//! deterministically over noded's exact /v1 wire — no live 3-validator roll.
//!
//! every op here is authored as a REAL 32-byte ed25519 key via the sim's
//! `hex:` origin escape (governance keys its ballots on `Origin::External`,
//! and raw pubkey bytes are not valid UTF-8, so the JSON-string origin lane
//! cannot express them otherwise). every rejection asserted is the REAL module
//! refusing; the sim adds only WHEN blocks commit, and the logical clock
//! (`consensus_time = EPOCH + height*1000ms`) makes voting deadlines walkable.

mod harness;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use harness::Sim;
use serde_json::{Value, json};
use simnode::module_contracts::governance::invite::{
    INVITE_GRANT_NAMESPACE, INVITE_NONCE_LEN, InviteToken, sign_join_proof,
};
use std::path::Path;

type Ed = PrivateKey;

/// the network binding governance verifies invite tokens against — the sim's
/// `--invite-binding` default (`Governance::with_invite_binding(b"sim")`).
const BINDING: &[u8] = b"sim";

// ── ceremony helpers ────────────────────────────────────

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// a validator's submit origin: the `hex:` escape resolving to its raw pubkey,
/// so the module sees `Origin::External(<32 bytes>)` — a real electorate member.
fn origin(k: &Ed) -> String {
    format!("hex:{}", hex(k.public_key().as_ref()))
}

/// spawn an `--auto --with-valset` sim seeded with three genesis validators
/// (from_seed 1..=3), the default `"sim"` binding.
fn governed(storage: &Path) -> (Sim, Vec<Ed>) {
    let validators: Vec<Ed> = (1..=3u64).map(Ed::from_seed).collect();
    let hexes = validators
        .iter()
        .map(|k| hex(k.public_key().as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    let sim = Sim::spawn(storage, &["--auto", "--with-valset", &hexes]);
    (sim, validators)
}

/// a far-future expiry for tokens whose test is not about expiry.
const FAR_FUTURE: u64 = 4_102_444_800; // 2100-01-01

/// mint a BEARER invite token: the issuer's signature over the grant
/// preimage `binding ‖ nonce ‖ expiry` in the grant namespace —
/// minting IS the admission decision. the preimage is deliberately RE-STATED
/// here rather than calling into governance: if the signed shape ever
/// drifts, this suite fails instead of following along.
fn mint_as(
    issuer: &Ed,
    binding: &[u8],
    nonce: [u8; INVITE_NONCE_LEN],
    expires_unix_secs: u64,
) -> InviteToken {
    let msg = [binding, nonce.as_slice(), &expires_unix_secs.to_le_bytes()].concat();
    InviteToken {
        issuer: issuer.public_key(),
        nonce,
        expires_unix_secs,
        sig: issuer.sign(INVITE_GRANT_NAMESPACE, &msg),
    }
}

/// the common shape: a bearer invite, far-future expiry.
fn mint(issuer: &Ed, binding: &[u8], nonce: [u8; INVITE_NONCE_LEN]) -> InviteToken {
    mint_as(issuer, binding, nonce, FAR_FUTURE)
}

/// the `GovMsg::Redeem` wire op — all raw bytes, mirroring the lobby announce.
fn redeem(token: &InviteToken, joiner: Vec<u8>, proof: Vec<u8>) -> Value {
    json!({ "redeem": {
        "issuer": token.issuer.as_ref().to_vec(),
        "nonce": token.nonce.to_vec(),
        "token_sig": token.sig.as_ref().to_vec(),
        "joiner": joiner,
        "proof": proof,
        "expires_unix_secs": token.expires_unix_secs,
    }})
}

/// propose → vote-approve from every voter → execute; returns the execute
/// receipt. voting all provided members past the threshold makes the proposal
/// decidable early, so execute settles before the deadline.
fn pass(sim: &Sim, voters: &[&Ed], id: &str, action: Value, period: u64) -> Value {
    sim.submit_ok(
        "governance",
        json!({ "propose": { "proposal_id": id, "action": action, "voting_period": period } }),
        Some(origin(voters[0]).as_str()),
    );
    for v in voters {
        sim.submit_ok(
            "governance",
            json!({ "vote": { "proposal_id": id, "approve": true } }),
            Some(origin(v).as_str()),
        );
    }
    sim.submit_ok(
        "governance",
        json!({ "execute": { "proposal_id": id } }),
        Some(origin(voters[0]).as_str()),
    )
}

/// does a sorted key list (a Validators/Residents reply array) contain `key`?
fn has_key(list: &Value, key: &[u8]) -> bool {
    let want = json!(key.to_vec());
    list.as_array().is_some_and(|a| a.contains(&want))
}

/// the submitter's ballot on a proposal view, if cast.
fn ballot(proposal: &Value, voter: &[u8]) -> Option<bool> {
    proposal["votes"].as_array()?.iter().find_map(|pair| {
        let pair = pair.as_array()?;
        (pair[0] == json!(voter.to_vec())).then(|| pair[1].as_bool())?
    })
}

/// one filler block — advances the logical clock without touching membership.
fn filler(sim: &Sim) {
    sim.submit_ok(
        "dispatch",
        serde_json::json!({ "nudge": {} }),
        Some("filler"),
    );
}

/// walk filler blocks until the committed tip reaches `height`.
fn walk_to(sim: &Sim, height: u64) {
    while sim.status()["height"].as_u64().unwrap_or(0) < height {
        filler(sim);
    }
}

// ── B1: the redeem happy path ───────────────────────────

#[test]
fn a_redeemed_invite_grants_residency_in_its_own_block_and_audits_the_admission() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());

    // a genesis validator mints; a fresh key joins. the RELAYING origin is a
    // non-member string — redeem is self-authenticating (the token authorizes
    // the admission, not the node carrying it: handle_redeem requires only an
    // external submitter, never membership).
    let issuer = &validators[0];
    let joiner = Ed::from_seed(10);
    let nonce = [7u8; INVITE_NONCE_LEN];
    let token = mint(issuer, BINDING, nonce);
    let proof = sign_join_proof(&joiner, BINDING, &token);
    let receipt = sim.submit_ok(
        "governance",
        redeem(
            &token,
            joiner.public_key().as_ref().to_vec(),
            proof.as_ref().to_vec(),
        ),
        Some("relay-node"),
    );

    // the grant rode the redeem's OWN block: the receipt height is the chain tip.
    let height = receipt["height"].as_u64().expect("receipt height");
    assert_eq!(
        sim.status()["height"].as_u64(),
        Some(height),
        "the ValsetMsg::Grant follow-up committed in the redeem's block"
    );

    // the joiner now holds resident standing…
    let residents = sim.query("valset", json!("residents"));
    assert!(
        has_key(&residents["residents"], joiner.public_key().as_ref()),
        "joiner is a resident: {residents}"
    );

    // …and the point read by nonce records who invited whom, at what height
    // (the redeemed set has no enumeration — it is point records alone).
    let redemptions = sim.query(
        "governance",
        json!({ "redemption": { "nonce": nonce.to_vec() } }),
    );
    let row = &redemptions["redemption"];
    assert_eq!(row["nonce"], json!(nonce.to_vec()), "row: {row}");
    assert_eq!(row["joiner"], json!(joiner.public_key().as_ref().to_vec()));
    assert_eq!(row["issuer"], json!(issuer.public_key().as_ref().to_vec()));
    assert_eq!(row["height"].as_u64(), Some(height));
}

// ── B2: single-use — THE invite-security property ───────

#[test]
fn a_nonce_redeems_exactly_once_even_for_a_different_joiner() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];
    let nonce = [9u8; INVITE_NONCE_LEN];

    // first redemption admits joiner-one via its own token.
    let joiner_one = Ed::from_seed(20);
    let token_one = mint(issuer, BINDING, nonce);
    let proof_one = sign_join_proof(&joiner_one, BINDING, &token_one);
    sim.submit_ok(
        "governance",
        redeem(
            &token_one,
            joiner_one.public_key().as_ref().to_vec(),
            proof_one.as_ref().to_vec(),
        ),
        Some("relay"),
    );

    // the issuer re-mints the SAME nonce for a SECOND target — every check
    // shy of the nonce set passes (valid sig, matching target, valid proof),
    // isolating the exactly-once property from the target lock.
    let joiner_two = Ed::from_seed(21);
    let token_two = mint(issuer, BINDING, nonce);
    let proof_two = sign_join_proof(&joiner_two, BINDING, &token_two);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &token_two,
            joiner_two.public_key().as_ref().to_vec(),
            proof_two.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("invite already redeemed"),
        "single-use: {error}"
    );
}

// ── B3: binding mismatch — cross-network replay dies ────

#[test]
fn a_token_minted_for_another_network_does_not_verify() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];

    // minted over a DIFFERENT binding than this network's "sim": the token
    // signature never verifies here, so a replay across networks is refused.
    let joiner = Ed::from_seed(30);
    let token = mint(issuer, b"other-net", [1u8; INVITE_NONCE_LEN]);
    let proof = sign_join_proof(&joiner, b"other-net", &token);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &token,
            joiner.public_key().as_ref().to_vec(),
            proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("invite token signature does not verify for this network"),
        "binding mismatch: {error}"
    );
}

// ── B4: non-member issuer — a stranger cannot invite ────

#[test]
fn a_token_from_a_non_member_issuer_is_refused() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, _validators) = governed(storage.path());

    // seed 99 is not in the genesis set: its token verifies cryptographically
    // (correct binding, correct proof) but the issuer is no current member.
    let stranger = Ed::from_seed(99);
    let joiner = Ed::from_seed(40);
    let token = mint(&stranger, BINDING, [2u8; INVITE_NONCE_LEN]);
    let proof = sign_join_proof(&joiner, BINDING, &token);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &token,
            joiner.public_key().as_ref().to_vec(),
            proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("the inviting member is no longer part of this network"),
        "non-member issuer: {error}"
    );
}

// ── B5: proof-of-possession forgery ─────────────────────

#[test]
fn a_blob_holder_cannot_redeem_under_a_key_that_never_asked_to_join() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];

    // the announced joiner key is joiner-one (the token's own target), but
    // the proof was signed by joiner-two — whoever holds a leaked token
    // cannot bind it to a key whose secret they do not hold.
    let joiner_one = Ed::from_seed(50);
    let joiner_two = Ed::from_seed(51);
    let token = mint(issuer, BINDING, [3u8; INVITE_NONCE_LEN]);
    let forged_proof = sign_join_proof(&joiner_two, BINDING, &token);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &token,
            joiner_one.public_key().as_ref().to_vec(),
            forged_proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("joiner proof-of-possession does not verify"),
        "forged proof: {error}"
    );
}

// ── B5d: expiry is NOT a consensus check — pin the absence ──

#[test]
fn consensus_admits_a_wall_clock_expired_token_expiry_lives_at_the_doorbells() {
    // consensus_time is BLOCK HEIGHT on this chain: no deterministic wall
    // clock exists in-consensus, so handle_redeem deliberately does NOT
    // check expiry. enforcement lives at the joiner's decode and at every
    // gating member's wall clock (lobby + intro doorbells), with single-use
    // bounding the residual window. this test pins the ABSENCE so a future
    // "fix" re-adding a vacuous height-vs-seconds comparison fails loudly.
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];

    let joiner = Ed::from_seed(55);
    let token = mint_as(
        issuer,
        BINDING,
        [8u8; INVITE_NONCE_LEN],
        1, // 1970 — expired on any wall clock
    );
    let proof = sign_join_proof(&joiner, BINDING, &token);
    sim.submit_ok(
        "governance",
        redeem(
            &token,
            joiner.public_key().as_ref().to_vec(),
            proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    let residents = sim.query("valset", json!("residents"));
    assert!(
        has_key(&residents["residents"], joiner.public_key().as_ref()),
        "consensus admitted the expired token — the doorbells own expiry: {residents}"
    );
}

// ── B5e: a BEARER invite — first valid proof wins, exactly once ──

#[test]
fn a_bearer_invite_grants_resident_standing_to_the_first_redeemer_only() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];

    // bearer: NO target lock — the token names no key at mint time.
    let token = mint_as(issuer, BINDING, [10u8; INVITE_NONCE_LEN], FAR_FUTURE);

    // key A — any key at all — takes the grant with its own proof.
    let a = Ed::from_seed(70);
    let a_proof = sign_join_proof(&a, BINDING, &token);
    sim.submit_ok(
        "governance",
        redeem(
            &token,
            a.public_key().as_ref().to_vec(),
            a_proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    let residents = sim.query("valset", json!("residents"));
    assert!(
        has_key(&residents["residents"], a.public_key().as_ref()),
        "first redeemer holds resident standing: {residents}"
    );

    // key B presents the SAME blob with its OWN valid proof: the nonce is
    // spent — single-use first-wins is the bearer containment story.
    let b = Ed::from_seed(71);
    let b_proof = sign_join_proof(&b, BINDING, &token);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &token,
            b.public_key().as_ref().to_vec(),
            b_proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("already redeemed"),
        "single-use first-wins: {error}"
    );
    let residents = sim.query("valset", json!("residents"));
    assert!(
        !has_key(&residents["residents"], b.public_key().as_ref()),
        "the loser gained nothing: {residents}"
    );
}

// ── B6: the staged-admission ladder ─────────────────────

#[test]
fn a_resident_is_promoted_to_validator_then_demoted_by_governance() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let issuer = &validators[0];
    let joiner = Ed::from_seed(60);
    let joiner_key = joiner.public_key().as_ref().to_vec();

    // redeem grants residency (the pre-promotion tier).
    let token = mint(issuer, BINDING, [4u8; INVITE_NONCE_LEN]);
    let proof = sign_join_proof(&joiner, BINDING, &token);
    sim.submit_ok(
        "governance",
        redeem(&token, joiner_key.clone(), proof.as_ref().to_vec()),
        Some("relay"),
    );
    assert!(
        has_key(
            &sim.query("valset", json!("residents"))["residents"],
            &joiner_key
        ),
        "joiner starts as a resident"
    );

    // AddValidator: two of three genesis validators carry the ballot (threshold
    // is 3/2+1 = 2). the passing Join PROMOTES — one boundary seats the
    // validator AND clears the resident standing.
    pass(
        &sim,
        &[&validators[0], &validators[1]],
        "promote-joiner",
        json!({ "add_validator": { "key": joiner_key.clone() } }),
        1_000_000,
    );
    assert!(
        has_key(
            &sim.query("valset", json!("validators"))["validators"],
            &joiner_key
        ),
        "joiner promoted into the quorum"
    );
    assert!(
        !has_key(
            &sim.query("valset", json!("residents"))["residents"],
            &joiner_key
        ),
        "and left the resident tier in the same boundary"
    );

    // RemoveValidator: the electorate is now four, so the threshold is
    // 4/2+1 = 3 — the three genesis validators demote the joiner.
    pass(
        &sim,
        &[&validators[0], &validators[1], &validators[2]],
        "demote-joiner",
        json!({ "remove_validator": { "key": joiner_key.clone() } }),
        1_000_000,
    );
    assert!(
        !has_key(
            &sim.query("valset", json!("validators"))["validators"],
            &joiner_key
        ),
        "joiner demoted out of the quorum"
    );

    // removed-after-minting (B4's other half): a token the now-removed joiner
    // mints dies on the CURRENT-membership check — an ex-member's invites lapse.
    let latecomer = Ed::from_seed(61);
    let stale = mint(&joiner, BINDING, [5u8; INVITE_NONCE_LEN]);
    let late_proof = sign_join_proof(&latecomer, BINDING, &stale);
    let error = sim.submit_rejected(
        "governance",
        redeem(
            &stale,
            latecomer.public_key().as_ref().to_vec(),
            late_proof.as_ref().to_vec(),
        ),
        Some("relay"),
    );
    assert!(
        error.contains("the inviting member is no longer part of this network"),
        "a removed member's outstanding invites die with it: {error}"
    );
}

// ── B7: the governance lifecycle ────────────────────────

#[test]
fn governance_gates_the_electorate_tracks_votes_and_honors_the_deadline() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let signal_action = json!({ "signal": { "text": "ship it" } });

    // electorate gating: a Propose from a non-member origin is refused at the
    // door — only the frozen electorate may open a ballot.
    let outsider = Ed::from_seed(70);
    let error = sim.submit_rejected(
        "governance",
        json!({ "propose": { "proposal_id": "intruder", "action": signal_action, "voting_period": 1000 } }),
        Some(origin(&outsider).as_str()),
    );
    assert!(
        error.contains("not a validator-set member node"),
        "electorate gating: {error}"
    );

    // a ballot can be changed while voting is open: approve, then flip to no.
    sim.submit_ok(
        "governance",
        json!({ "propose": { "proposal_id": "flip", "action": json!({ "signal": { "text": "x" } }), "voting_period": 1_000_000 } }),
        Some(origin(&validators[0]).as_str()),
    );
    sim.submit_ok(
        "governance",
        json!({ "vote": { "proposal_id": "flip", "approve": true } }),
        Some(origin(&validators[0]).as_str()),
    );
    let view = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "flip" } }),
    );
    assert_eq!(
        ballot(&view["proposal"], validators[0].public_key().as_ref()),
        Some(true),
        "ballot recorded as yes: {view}"
    );
    sim.submit_ok(
        "governance",
        json!({ "vote": { "proposal_id": "flip", "approve": false } }),
        Some(origin(&validators[0]).as_str()),
    );
    let view = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "flip" } }),
    );
    assert_eq!(
        ballot(&view["proposal"], validators[0].public_key().as_ref()),
        Some(false),
        "the ballot flipped to no (last-write-wins): {view}"
    );

    // the tally rule in validator mode is Threshold{ required_yes = total/2+1 };
    // with three validators that is 2. one yes is NOT early-decidable…
    sim.submit_ok(
        "governance",
        json!({ "propose": { "proposal_id": "early", "action": json!({ "signal": { "text": "y" } }), "voting_period": 1_000_000 } }),
        Some(origin(&validators[0]).as_str()),
    );
    sim.submit_ok(
        "governance",
        json!({ "vote": { "proposal_id": "early", "approve": true } }),
        Some(origin(&validators[0]).as_str()),
    );
    let error = sim.submit_rejected(
        "governance",
        json!({ "execute": { "proposal_id": "early" } }),
        Some(origin(&validators[0]).as_str()),
    );
    assert!(
        error.contains("not decidable yet") && error.contains("yes=1") && error.contains("total=3"),
        "one yes is short of the threshold and no ballot has closed: {error}"
    );

    // …a second yes reaches the threshold: with 2 of 3 already yes, no
    // remaining ballot can reverse it, so execute settles EARLY.
    sim.submit_ok(
        "governance",
        json!({ "vote": { "proposal_id": "early", "approve": true } }),
        Some(origin(&validators[1]).as_str()),
    );
    sim.submit_ok(
        "governance",
        json!({ "execute": { "proposal_id": "early" } }),
        Some(origin(&validators[0]).as_str()),
    );
    let view = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "early" } }),
    );
    assert_eq!(
        view["proposal"]["status"], "passed",
        "early passage: {view}"
    );

    // a vote after settle is refused — the ballot is closed.
    let error = sim.submit_rejected(
        "governance",
        json!({ "vote": { "proposal_id": "early", "approve": false } }),
        Some(origin(&validators[2]).as_str()),
    );
    assert!(
        error.contains("proposal is settled"),
        "post-settle vote: {error}"
    );

    // the LOGICAL clock is the deadline clock: a short-period proposal with a
    // lone yes cannot pass early, and only settles once the deadline elapses.
    let hd = sim.status()["height"].as_u64().unwrap();
    sim.submit_ok(
        "governance",
        json!({ "propose": { "proposal_id": "deadline", "action": json!({ "signal": { "text": "z" } }), "voting_period": 5000 } }),
        Some(origin(&validators[0]).as_str()),
    );
    sim.submit_ok(
        "governance",
        json!({ "vote": { "proposal_id": "deadline", "approve": true } }),
        Some(origin(&validators[0]).as_str()),
    );
    let error = sim.submit_rejected(
        "governance",
        json!({ "execute": { "proposal_id": "deadline" } }),
        Some(origin(&validators[0]).as_str()),
    );
    assert!(
        error.contains("not decidable yet"),
        "before the deadline a lone yes cannot settle: {error}"
    );
    // walk the clock well past the 5-block voting period, then it settles —
    // rejected, since one yes never met the threshold of two.
    walk_to(&sim, hd + 10);
    sim.submit_ok(
        "governance",
        json!({ "execute": { "proposal_id": "deadline" } }),
        Some(origin(&validators[0]).as_str()),
    );
    let view = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "deadline" } }),
    );
    assert_eq!(
        view["proposal"]["status"], "rejected",
        "settled at the deadline without a majority: {view}"
    );
}

// ── B7 (share-mode door checks) ─────────────────────────

#[test]
fn share_governance_refuses_actions_that_precede_an_adoption() {
    // the account-share ladder's entry guards, without founding any identity
    // account: adopting shares must name existing Identity accounts, and both
    // SetShares and enabling share mode require an adopted registry first.
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let v0 = origin(&validators[0]);

    let error = sim.submit_rejected(
        "governance",
        json!({ "propose": { "proposal_id": "adopt", "voting_period": 1000, "action": {
            "adopt_shares": { "allocations": [ { "account_id": 99, "shares": 10 } ] }
        }}}),
        Some(v0.as_str()),
    );
    assert!(
        error.contains("share allocation names no existing Identity account"),
        "adopt names a phantom account: {error}"
    );

    let error = sim.submit_rejected(
        "governance",
        json!({ "propose": { "proposal_id": "setshares", "voting_period": 1000, "action": {
            "set_shares": { "account_id": 99, "shares": 5 }
        }}}),
        Some(v0.as_str()),
    );
    assert!(
        error.contains("adopt governance shares before changing them"),
        "set_shares before adopt: {error}"
    );

    let error = sim.submit_rejected(
        "governance",
        json!({ "propose": { "proposal_id": "setmode", "voting_period": 1000, "action": {
            "set_share_mode": { "enabled": true }
        }}}),
        Some(v0.as_str()),
    );
    assert!(
        error.contains("configure governance shares before enabling share mode"),
        "enable share mode before adopt: {error}"
    );
}

// ── B9: leave / self-removal ────────────────────────────

#[test]
fn a_validator_requests_its_own_removal_and_the_remaining_members_approve() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let leaver = &validators[2];
    let leaver_key = leaver.public_key().as_ref().to_vec();

    // self-removal is expressed as a RemoveValidator proposal for one's own key
    // (there is no dedicated path). the ballot opens pending.
    sim.submit_ok(
        "governance",
        json!({ "propose": { "proposal_id": "self-leave", "voting_period": 1_000_000, "action": {
            "remove_validator": { "key": leaver_key.clone() }
        }}}),
        Some(origin(leaver).as_str()),
    );
    let view = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "self-leave" } }),
    );
    assert_eq!(
        view["proposal"]["status"], "open",
        "self-removal is pending: {view}"
    );

    // the REMAINING members approve (the leaver need not vote) — two yes meets
    // the threshold of two, and execute settles the departure.
    for v in &validators[..2] {
        sim.submit_ok(
            "governance",
            json!({ "vote": { "proposal_id": "self-leave", "approve": true } }),
            Some(origin(v).as_str()),
        );
    }
    sim.submit_ok(
        "governance",
        json!({ "execute": { "proposal_id": "self-leave" } }),
        Some(origin(&validators[0]).as_str()),
    );

    let validators_now = sim.query("valset", json!("validators"));
    assert!(
        !has_key(&validators_now["validators"], &leaver_key),
        "the leaver is gone: {validators_now}"
    );
    assert!(
        has_key(
            &validators_now["validators"],
            validators[0].public_key().as_ref()
        ) && has_key(
            &validators_now["validators"],
            validators[1].public_key().as_ref()
        ),
        "the remaining members keep the quorum: {validators_now}"
    );
}

// ── Bonus: the module-origin gate ───────────────────────

#[test]
fn a_direct_external_membership_op_is_refused_by_the_module_origin_gate() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());
    let intruder = Ed::from_seed(80);
    let key = intruder.public_key().as_ref().to_vec();

    // valset accepts membership ops only from the GOVERNANCE module's own
    // follow-up or a system origin — never a raw external submit (even a
    // well-formed key), and never another module's follow-up.
    let error = sim.submit_rejected(
        "valset",
        json!({ "join": { "key": key.clone() } }),
        Some(origin(&validators[0]).as_str()),
    );
    assert!(
        error.contains("valset membership changes only via governance"),
        "direct Join: {error}"
    );
    let error = sim.submit_rejected(
        "valset",
        json!({ "grant": { "key": key } }),
        Some(origin(&validators[0]).as_str()),
    );
    assert!(
        error.contains("valset membership changes only via governance"),
        "direct Grant: {error}"
    );
}
