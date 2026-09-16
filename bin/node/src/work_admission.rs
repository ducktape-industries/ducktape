//! Work admission — whose work this node will execute.
//!
//! ## the premise this whole module rests on
//!
//! A saga's committed [`SagaOrigin::External`] key is DERIVED, never asserted:
//! it is the key whose verified frame signature carried the op. There are two
//! signers. `POST /v1/submit` on a real node DISCARDS the caller's claimed
//! submitter id and re-signs with the node's own signer
//! (`validator/run/ingress.rs`: *"this lane signs frames, and the signed origin
//! IS this node's pubkey"*), so a node-authored op carries the SUBMITTING
//! NODE's key. `POST /v1/submit/frame` relays a frame the USER signed with an
//! account key, so a user-authored op (`ducktape agent run`, a scheduled run)
//! carries that key.
//!
//! **If `/v1/submit` ever stamped a caller-supplied origin, this admission
//! would become decorative.** `the_submit_lane_still_resigns_with_the_node_key`
//! pins it.
//!
//! ## what it decides, and what it deliberately does not
//!
//! Identity never binds a node to an account. Attribution comes from the
//! signing key alone: a user key resolves to its account through
//! [`identity::IdentityQuery::OfKey`]; a node key resolves to nothing, because
//! no account is ever keyed by a node. So a grant lends to an ACCOUNT, and the
//! question this module answers is not *"who is this run acting for"* (a
//! claim about a third party, which this layer must never make) but *"will
//! this host run this party's workload at all"* — a host deciding its own
//! policy about a party it identified itself.
//!
//! ## one decision, one call site
//!
//! [`admit`] is the ONLY entry point, and `compute::intake::WorkPump` — a
//! committed saga assigned to or announced at this node — is the only lane
//! that calls it. `the_work_lane_routes_through_one_verdict` is a
//! source-parsing lint that keeps it that way: two checks that must agree is
//! the dual-path defect this repo forbids.
//!
//! An independently installed service decides its own admission with its own
//! copy of the policy file (`provider_host::work_admission`); it reaches no
//! node code, and the node reaches none of its sessions.
//!
//! ## what it does NOT close
//!
//! - A saga triggered by a MODULE (`dispatch` — i.e. the chat/pages/forge/jobs
//!   /`RequestRun` family) has no account origin at this layer and is admitted:
//!   see [`WorkCaller::NotAnAccountOrigin`].
//! - A peer node's own `/v1/submit` is a node, not an account, and the default
//!   policy names only accounts — so it lands on
//!   [`WorkCaller::KeyWithoutAccount`].
//! - The guarantee is bounded by `/v1`'s exposure. `POST /v1/submit` re-signs
//!   as THIS node, so anything that can reach the node's HTTP or RPC port takes
//!   the [`WorkCaller::ThisNode`] path by construction. Making un-tokened `/v1`
//!   callers refused is its own campaign; keeping those ports loopback-bound
//!   is what makes this module mean anything.

use std::collections::BTreeSet;
use std::path::Path;

use saga::SagaOrigin;

pub(crate) use provider_host::work_admission::{
    ANYONE, AdmitTarget, WorkAdmission, load, policy_path, save,
};
#[cfg(test)]
use provider_host::work_admission::parse;

/// Test fixture: give `workspace` a policy admitting `account`, through the same
/// writer the CLI uses. It lives HERE so a lane's own tests never need to name a
/// policy type — `both_lanes_route_through_one_verdict` forbids that, and a
/// fixture is not an exception worth carving into a lint that is otherwise
/// absolute.
#[cfg(test)]
pub(crate) fn admit_account_fixture(workspace: &Path, account: u64) -> Result<(), String> {
    save(
        workspace,
        &WorkAdmission::default().with(AdmitTarget::Account(account)),
    )
}

// ============================================================================
// the decision
// ============================================================================

/// Who is asking, as far as committed state can say. FIVE states, because
/// "could not ask", "a key on no account" and "a peer node" are different
/// operator problems — the lesson `airlock::server::GrantAnswer` already paid
/// for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkCaller {
    /// the asking key IS this node's key: our own submission. Zero queries, and
    /// it is what keeps a single-node workspace and an account-less node
    /// working.
    ThisNode,
    /// the signing key is a member of this account.
    Account(u64),
    /// identity answered, and the signing key belongs to no account. A peer
    /// node's own `/v1/submit` lands here too: no account is keyed by a node.
    KeyWithoutAccount,
    /// the saga was triggered by a MODULE, not an external submitter — the
    /// chat/pages/forge/jobs family, whose requester is one hop further back in
    /// `runs`' own state. Not attributable here, and admitted: see the module
    /// header.
    NotAnAccountOrigin,
    /// the identity read did not answer. Nothing is known.
    Unresolved,
}

/// Why work was turned away. The stable snake_case `reason` is DERIVED from the
/// variant, so a typo cannot silently downgrade a refusal
/// (`admin::AdminRefusal`'s shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkRefusal {
    NotAdmitted,
    CallerUnbound,
    PolicyUnreadable,
}

impl WorkRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            WorkRefusal::NotAdmitted => "work_not_admitted",
            WorkRefusal::CallerUnbound => "work_caller_unbound",
            WorkRefusal::PolicyUnreadable => "work_policy_unreadable",
        }
    }
}

/// THREE states, not two. Folding "I could not ask" into a refusal is the
/// expensive mistake: it tells the caller to go get an admission that may
/// already exist, and on the saga lane it would burn an attempt on a read that
/// simply failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkVerdict {
    Admitted,
    Refused(WorkRefusal),
    AuthorityUnavailable,
}

/// The committed-state read both lanes need, behind the one method they share.
/// Two transports (the daemon's `NodeLink`, the node's `NodeCommand` channel),
/// one resolution.
#[async_trait::async_trait]
pub(crate) trait CommittedReader: Send + Sync {
    async fn read(&self, target: &str, request: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// **The** admission decision. The work lane calls this and nothing else; see
/// `the_work_lane_routes_through_one_verdict`.
pub(crate) async fn admit(
    reader: &dyn CommittedReader,
    workspace: &Path,
    me: &[u8],
    origin: &SagaOrigin,
) -> WorkVerdict {
    let policy = match load(workspace) {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(
                target: "ducktape::service",
                reason = "work_policy_unreadable",
                %error,
                "work admission cannot read its policy"
            );
            return WorkVerdict::Refused(WorkRefusal::PolicyUnreadable);
        }
    };
    let caller = resolve_caller(reader, me, origin).await;
    verdict(&policy, &caller)
}

/// Pure. Policy first, caller second — so an `Anyone` node keeps running work
/// when the identity module hiccups, which is the only answer that policy can
/// mean.
fn verdict(policy: &WorkAdmission, caller: &WorkCaller) -> WorkVerdict {
    match policy {
        WorkAdmission::Anyone => WorkVerdict::Admitted,
        WorkAdmission::Accounts(accounts) => admits(accounts, caller),
    }
}

fn admits(accounts: &BTreeSet<u64>, caller: &WorkCaller) -> WorkVerdict {
    match caller {
        WorkCaller::ThisNode => WorkVerdict::Admitted,
        WorkCaller::NotAnAccountOrigin => WorkVerdict::Admitted,
        WorkCaller::Unresolved => WorkVerdict::AuthorityUnavailable,
        WorkCaller::KeyWithoutAccount => WorkVerdict::Refused(WorkRefusal::CallerUnbound),
        WorkCaller::Account(number) => match accounts.contains(number) {
            true => WorkVerdict::Admitted,
            false => WorkVerdict::Refused(WorkRefusal::NotAdmitted),
        },
    }
}

/// Attribute a committed saga origin. Exhaustive on purpose: a new
/// `SagaOrigin` variant must fail the build here rather than default to
/// admitted. `me` is this node's own key and every compared key came from a
/// verified signature — nothing a caller sends can reach these comparisons.
async fn resolve_caller(
    reader: &dyn CommittedReader,
    me: &[u8],
    origin: &SagaOrigin,
) -> WorkCaller {
    match origin {
        SagaOrigin::Module(_) | SagaOrigin::System => WorkCaller::NotAnAccountOrigin,
        SagaOrigin::External(key) => {
            if key == me {
                return WorkCaller::ThisNode;
            }
            match account_of_key(reader, key).await {
                Ok(Some(number)) => WorkCaller::Account(number),
                Ok(None) => WorkCaller::KeyWithoutAccount,
                Err(_) => WorkCaller::Unresolved,
            }
        }
    }
}

/// The committed key→account resolution. `pub(crate)` because the credential
/// lender's delegation gate (`crate::airlock`) asks the same question of the
/// same module over the same seam — it borrows this READ and nothing else, and
/// in particular never touches the admission policy above: whose work a node
/// runs and whose credential a run draws on are two separate consents.
pub(crate) async fn account_of_key(
    reader: &dyn CommittedReader,
    key: &[u8],
) -> Result<Option<u64>, String> {
    let request = identity::encode_query(&identity::IdentityQuery::OfKey { key: key.to_vec() });
    let bytes = reader.read("identity", request).await?;
    match identity::decode_reply(&bytes)? {
        identity::IdentityReply::Account(account) => Ok(account.map(|view| view.number)),
        other => Err(format!("identity returned an unexpected reply: {other:?}")),
    }
}

#[cfg(test)]
mod tests;
