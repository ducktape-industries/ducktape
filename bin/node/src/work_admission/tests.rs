//! Work-admission unit tests: the pure decision, the policy file, and the
//! source-parsing lint that keeps ONE verdict serving both lanes.

use std::path::{Path, PathBuf};

use super::*;

const OWNER: u64 = 1;
const FRIEND: u64 = 2;
const STRANGER: u64 = 3;

fn accounts(numbers: &[u64]) -> WorkAdmission {
    WorkAdmission::Accounts(numbers.iter().copied().collect())
}

fn caller(number: u64) -> WorkCaller {
    WorkCaller::Account(number)
}

// ---------------------------------------------------------------------------
// the pure decision
// ---------------------------------------------------------------------------

/// The default admits NO account: a node runs its own work and nothing else
/// until its operator names someone — the one assertion a forced-open
/// `verdict` must redden.
#[test]
fn the_default_admits_only_this_node() {
    let policy = WorkAdmission::default();
    assert_eq!(
        verdict(&policy, &WorkCaller::ThisNode),
        WorkVerdict::Admitted
    );
    assert_eq!(
        verdict(&policy, &caller(OWNER)),
        WorkVerdict::Refused(WorkRefusal::NotAdmitted)
    );
    assert_eq!(
        verdict(&policy, &caller(STRANGER)),
        WorkVerdict::Refused(WorkRefusal::NotAdmitted)
    );
}

/// An admitted account is admitted; an unnamed one is not. Same shape as
/// `gateway::credential_use_allowed`: explicit grantees, nobody implicit.
#[test]
fn an_admitted_account_is_admitted_and_an_unnamed_one_is_not() {
    let policy = accounts(&[OWNER, FRIEND]);
    assert_eq!(verdict(&policy, &caller(FRIEND)), WorkVerdict::Admitted);
    assert_eq!(verdict(&policy, &caller(OWNER)), WorkVerdict::Admitted);
    assert_eq!(
        verdict(&policy, &caller(STRANGER)),
        WorkVerdict::Refused(WorkRefusal::NotAdmitted)
    );
}

/// This node's own submissions never consult anything: no policy, no identity
/// read. It is what keeps a single-node workspace and an account-less node
/// working.
#[test]
fn our_own_work_needs_no_policy() {
    for policy in [WorkAdmission::default(), accounts(&[FRIEND])] {
        assert_eq!(
            verdict(&policy, &WorkCaller::ThisNode),
            WorkVerdict::Admitted
        );
    }
}

/// A module-triggered saga (the chat/pages/forge/jobs family) has no account
/// origin at this layer and is admitted. Documented as the residual, and
/// asserted so it cannot change silently.
#[test]
fn a_module_triggered_saga_is_admitted_and_that_is_the_named_residual() {
    assert_eq!(
        verdict(&WorkAdmission::default(), &WorkCaller::NotAnAccountOrigin),
        WorkVerdict::Admitted
    );
}

/// A failed identity read is NOT a refusal. On the saga lane a refusal would
/// burn an attempt against a read that simply did not answer, and on the term
/// lane it would send the caller to fix an admission that may already exist.
#[test]
fn an_unresolved_caller_is_unavailable_not_refused() {
    assert_eq!(
        verdict(&WorkAdmission::default(), &WorkCaller::Unresolved),
        WorkVerdict::AuthorityUnavailable
    );
}

/// `Anyone` decides BEFORE the caller is considered — so a node that admits
/// everyone keeps working while the identity module is unreachable. Policy
/// first, caller second, and this is why.
#[test]
fn anyone_admits_even_when_the_caller_cannot_be_resolved() {
    for caller in [
        WorkCaller::Unresolved,
        WorkCaller::KeyWithoutAccount,
        caller(STRANGER),
    ] {
        assert_eq!(
            verdict(&WorkAdmission::Anyone, &caller),
            WorkVerdict::Admitted
        );
    }
}

/// A key on no account cannot be named by any admission, so it gets its OWN
/// reason: the fix is to submit as an account, not `node work admit` here.
#[test]
fn a_key_with_no_account_is_refused_with_its_own_reason() {
    assert_eq!(
        verdict(&WorkAdmission::default(), &WorkCaller::KeyWithoutAccount),
        WorkVerdict::Refused(WorkRefusal::CallerUnbound)
    );
}

/// Every refusal token is stable, distinct, and snake_case; none of them can
/// carry an account.
#[test]
fn refusal_reasons_are_distinct_stable_tokens() {
    let reasons = [
        WorkRefusal::NotAdmitted.reason(),
        WorkRefusal::CallerUnbound.reason(),
        WorkRefusal::PolicyUnreadable.reason(),
    ];
    assert_eq!(
        reasons,
        [
            "work_not_admitted",
            "work_caller_unbound",
            "work_policy_unreadable"
        ]
    );
    for reason in reasons {
        assert!(
            reason.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{reason} is not a snake_case token"
        );
    }
}

// ---------------------------------------------------------------------------
// attribution
// ---------------------------------------------------------------------------

/// a reader that fails the test if anything reads through it.
struct NoReads;

#[async_trait::async_trait]
impl CommittedReader for NoReads {
    async fn read(&self, target: &str, _request: Vec<u8>) -> Result<Vec<u8>, String> {
        panic!("work admission read {target:?} on a path that must make no committed read");
    }
}

/// a reader that answers `OfKey` from a fixed key→account table.
struct KeyTable(Vec<(Vec<u8>, u64)>);

#[async_trait::async_trait]
impl CommittedReader for KeyTable {
    async fn read(&self, _target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        let identity::IdentityQuery::OfKey { key } =
            identity::decode_query(&request).expect("an identity query")
        else {
            panic!("work admission asks only OfKey");
        };
        let account = self
            .0
            .iter()
            .find(|(member, _)| *member == key)
            .map(|(_, number)| account_view(*number));
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            account,
        )))
    }
}

fn account_view(number: u64) -> identity::AccountView {
    identity::AccountView {
        control: identity::Control::Keys,
        number,
        name: "someone".into(),
        keys: Vec::new(),
        avatar: None,
        bio: None,
        updated_at: 0,
    }
}

/// The attribution table: a user key resolves to its account, a stranger key
/// to none, a module saga is not attributable, and this node is itself. A new
/// `SagaOrigin` variant fails the build in `resolve_caller` rather than
/// defaulting to admitted.
#[tokio::test]
async fn a_source_resolves_to_exactly_one_caller() {
    let me = b"this-node";
    let table = KeyTable(vec![(b"friend-key".to_vec(), FRIEND)]);
    let friend = SagaOrigin::External(b"friend-key".to_vec());
    assert_eq!(
        resolve_caller(&table, me, &friend).await,
        WorkCaller::Account(FRIEND)
    );
    let stranger = SagaOrigin::External(b"stranger-key".to_vec());
    assert_eq!(
        resolve_caller(&table, me, &stranger).await,
        WorkCaller::KeyWithoutAccount
    );
    let module = SagaOrigin::Module("dispatch".into());
    assert_eq!(
        resolve_caller(&NoReads, me, &module).await,
        WorkCaller::NotAnAccountOrigin
    );
    assert_eq!(
        resolve_caller(&NoReads, me, &SagaOrigin::System).await,
        WorkCaller::NotAnAccountOrigin
    );
}

// ---------------------------------------------------------------------------
// the zero-read path
// ---------------------------------------------------------------------------

/// **This node's own work costs ZERO committed reads.** Not an optimization: a
/// read here would make a single-node workspace — and every node whose
/// operator holds no account — depend on an identity module that has nothing
/// to say. The reader panics, so a reintroduced lookup fails loudly instead of
/// quietly hanging a create.
#[tokio::test]
async fn our_own_work_makes_no_committed_read() {
    let dir = scratch("noreads");
    let me = b"this-node";
    let external = SagaOrigin::External(me.to_vec());
    assert_eq!(
        admit(&NoReads, &dir, me, &external).await,
        WorkVerdict::Admitted
    );
    // and a module-triggered saga names no key at all, so it reads nothing either.
    let module = SagaOrigin::Module("dispatch".into());
    assert_eq!(
        admit(&NoReads, &dir, me, &module).await,
        WorkVerdict::Admitted
    );
}

/// An unreadable policy refuses — loudly and by its own name — rather than
/// admitting or retrying forever. It also short-circuits BEFORE any read.
#[tokio::test]
async fn an_unreadable_policy_refuses_without_reading_anything() {
    let dir = scratch("broken");
    std::fs::write(policy_path(&dir), "admit = \"not-a-list\"\n").expect("write");
    assert_eq!(
        admit(
            &NoReads,
            &dir,
            b"me",
            &SagaOrigin::External(b"stranger".to_vec())
        )
        .await,
        WorkVerdict::Refused(WorkRefusal::PolicyUnreadable)
    );
}

/// a reader whose identity read fails outright.
struct KeyReadFails;

#[async_trait::async_trait]
impl CommittedReader for KeyReadFails {
    async fn read(&self, _target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        let identity::IdentityQuery::OfKey { .. } =
            identity::decode_query(&request).expect("an identity query")
        else {
            panic!("work admission asks only OfKey");
        };
        Err("identity module did not answer".into())
    }
}

/// **A failed key read is not a refusal.** Folding it into `work_not_admitted`
/// would tell the operator to admit an account that might already be admitted
/// — the exact misdiagnosis `GrantAnswer`'s third state exists to prevent.
#[tokio::test]
async fn a_failed_key_read_is_unavailable_not_refused() {
    let dir = scratch("keyfail");
    admit_account_fixture(&dir, FRIEND).expect("policy");
    let external = SagaOrigin::External(b"friend-key".to_vec());
    assert_eq!(
        admit(&KeyReadFails, &dir, b"me", &external).await,
        WorkVerdict::AuthorityUnavailable
    );
}

/// The end-to-end shape on the saga lane: an admitted account's user-signed
/// saga runs, a stranger account's does not.
#[tokio::test]
async fn an_admitted_accounts_saga_runs_and_a_strangers_does_not() {
    let dir = scratch("saga");
    admit_account_fixture(&dir, FRIEND).expect("policy");
    let table = KeyTable(vec![
        (b"friend-key".to_vec(), FRIEND),
        (b"stranger-key".to_vec(), STRANGER),
    ]);
    let friend = SagaOrigin::External(b"friend-key".to_vec());
    assert_eq!(
        admit(&table, &dir, b"me", &friend).await,
        WorkVerdict::Admitted
    );
    let stranger = SagaOrigin::External(b"stranger-key".to_vec());
    assert_eq!(
        admit(&table, &dir, b"me", &stranger).await,
        WorkVerdict::Refused(WorkRefusal::NotAdmitted)
    );
}

// ---------------------------------------------------------------------------
// the policy file
// ---------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ducktape-work-admit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch workspace");
    dir
}

/// A missing file IS the default — the `services.toml` convention, and what
/// makes this change additive to every existing workspace.
#[test]
fn a_workspace_with_no_policy_file_runs_only_its_own_work() {
    let dir = scratch("absent");
    assert_eq!(
        load(&dir).expect("absent is the default"),
        WorkAdmission::default()
    );
}

/// Round trip, and the default REMOVES the file: one representation per
/// policy, no husk to read stale.
#[test]
fn the_policy_round_trips_and_the_default_leaves_no_file() {
    let dir = scratch("roundtrip");
    let policy = accounts(&[FRIEND, STRANGER]);
    save(&dir, &policy).expect("save");
    assert_eq!(load(&dir).expect("load"), policy);
    let text = std::fs::read_to_string(policy_path(&dir)).expect("read");
    assert!(
        text.contains("\"2\""),
        "entries are decimal numbers: {text}"
    );

    save(&dir, &WorkAdmission::Anyone).expect("save anyone");
    assert_eq!(load(&dir).expect("load anyone"), WorkAdmission::Anyone);

    save(&dir, &WorkAdmission::default()).expect("save default");
    assert!(
        !policy_path(&dir).exists(),
        "the default must leave no file behind"
    );
    assert_eq!(load(&dir).expect("load default"), WorkAdmission::default());
}

/// Revoking the last account narrows back to the default, so `admit = []` is
/// never written — one representation, no third state a reader could invent.
#[test]
fn revoking_the_last_account_narrows_back_to_the_default() {
    let policy = accounts(&[FRIEND]).without(AdmitTarget::Account(FRIEND));
    assert_eq!(policy, WorkAdmission::default());
    assert!(policy.entries().is_empty());
}

/// `anyone` absorbs and `revoke anyone` narrows all the way back.
#[test]
fn anyone_absorbs_and_revoking_it_returns_to_the_default() {
    let widened = accounts(&[FRIEND]).with(AdmitTarget::Anyone);
    assert_eq!(widened, WorkAdmission::Anyone);
    assert_eq!(
        widened.with(AdmitTarget::Account(FRIEND)),
        WorkAdmission::Anyone
    );
    assert_eq!(
        WorkAdmission::Anyone.without(AdmitTarget::Anyone),
        WorkAdmission::default()
    );
}

/// The wildcard is a STATEMENT, not an entry: mixing it with account numbers
/// is a refusal at parse, not a silently-widened policy.
#[test]
fn admit_cannot_mix_the_wildcard_with_accounts() {
    let mixed = parse(&[ANYONE.to_string(), FRIEND.to_string()]).unwrap_err();
    assert!(mixed.contains("statement, not an entry"), "got {mixed:?}");
    assert_eq!(
        parse(&[ANYONE.to_string()]).expect("bare wildcard"),
        WorkAdmission::Anyone
    );
}

/// A hand-edited entry that is not a number (or is 0) is a loud refusal,
/// never a silently dropped entry — a dropped entry would read as "admitted"
/// going one way and "refused" going the other.
#[test]
fn a_non_numeric_admit_entry_is_refused() {
    let error = parse(&["not-a-number".to_string()]).unwrap_err();
    assert!(error.contains("not an account number"), "got {error:?}");
    let zero = parse(&["0".to_string()]).unwrap_err();
    assert!(zero.contains("no account"), "got {zero:?}");
}

/// An unknown key in the file is a decode error, not tolerance.
#[test]
fn the_policy_file_is_strict() {
    let dir = scratch("strict");
    std::fs::write(policy_path(&dir), "admit = []\nspend_cap = 10\n").expect("write");
    let error = load(&dir).unwrap_err();
    assert!(error.contains("spend_cap"), "got {error:?}");
}

// ---------------------------------------------------------------------------
// the shape lint
// ---------------------------------------------------------------------------

fn source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    // a lint on CODE: the doc comments deliberately name what they forbid.
    text.lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// **The single-verdict shape, parsed rather than promised.**
///
/// Two admission checks that must agree is the dual-path defect this repo
/// forbids; one function called twice is not. So each lane reaches the policy
/// through exactly ONE `work_admission::admit(` call and names no policy type
/// of its own — a second call site, or a lane that loaded the policy and
/// decided for itself, fails here.
#[test]
fn the_work_lane_routes_through_one_verdict() {
    const LANES: [(&str, &str); 1] = [("src/compute/intake.rs", "the compute intake")];
    for (relative, lane) in LANES {
        let code = source(relative);
        let calls = code.matches("work_admission::admit(").count();
        assert_eq!(
            calls, 1,
            "{lane} reaches work admission through {calls} call sites; it must be exactly \
             one — every path into this lane routes through the same decision"
        );
        // the three tokens that mean "I touched the policy myself". A lane may
        // freely NAME a `WorkVerdict`/`WorkRefusal` — it has to, to consume one
        // — but reading the policy or calling the decision directly is the
        // second check that could disagree with the first.
        for forbidden in [
            "WorkAdmission::",
            "work_admission::load(",
            "work_admission::verdict",
        ] {
            assert!(
                !code.contains(forbidden),
                "{lane} names `{forbidden}`: the policy is decided in `work_admission` and \
                 consumed as a `WorkVerdict` — a call site that re-decides is the \
                 two-checks-that-must-agree defect"
            );
        }
    }
    let own = source("src/work_admission.rs");
    assert_eq!(
        own.matches("fn verdict(").count(),
        1,
        "there is exactly one verdict function, and the work lane reaches it through `admit`"
    );
}

/// **The premise the whole module rests on.** `/v1/submit` must keep discarding
/// the caller's claimed origin and re-signing with the node's own key: that is
/// what makes a node-authored saga's committed `External` origin a DERIVED
/// node identity rather than an asserted one. If this lane ever stamped a
/// caller-supplied origin, the compute-lane admission would silently become
/// decorative.
#[test]
fn the_submit_lane_still_resigns_with_the_node_key() {
    let code = source("src/validator/run/ingress.rs");
    assert!(
        code.contains("origin: _,"),
        "the validator submit lane must IGNORE the caller's claimed origin"
    );
    let compact: String = code
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(
        compact.contains("node::encode_frame_with_blob(&self.signer,"),
        "the validator submit lane must re-sign with this node's own signer"
    );
}
