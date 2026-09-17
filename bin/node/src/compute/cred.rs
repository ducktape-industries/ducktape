//! Executing-node credential resolution: turn a run's named gateway credential
//! into a self-host airlock broker source, gated on the run's committed saga
//! origin.
//!
//! A `ducktape agent sched --cred <name>` run carries the credential NAME in its
//! envelope, never any secret. On the node that executes the run, this resolver
//! turns that name into ROUTING METADATA and nothing else: where the owner's
//! gateway is (`airlock.<handle>.duck`), which vendor it fronts, and the public
//! seal key to pin.
//!
//! ## it does not decide who is acting — it says WHICH WORK, and nothing else
//!
//! It used to decide. It read the run's saga origin, mapped that submitting node
//! to an account, and shipped that ACCOUNT to the lender as the grant subject —
//! a claim by the COMPUTE layer about who a run acts for, which the lender had
//! no way to check. That was a reproduced credential-theft hole and PR #833
//! deleted it.
//!
//! What this resolver ships now is a POINTER: the run's `saga_id`, and nothing
//! else. The lender resolves it in ITS OWN committed state — every node sees the
//! same chain — and reads out of consensus who submitted the saga, who holds its
//! lease, and whether that submitter is granted. The executor asserts none of
//! those three facts and cannot influence any of them. The load-bearing reason
//! this is safe is stated on [`airlock::wire::WorkRef`]: `/v1/submit` discards a
//! caller's claimed submitter id and re-signs with the node's own key, so the
//! committed `SagaOrigin::External` is a signature-proven node key.
//!
//! What that means for an operator: a run submitted by A and executed on B draws
//! on A's grant when B holds A's lease — that is the delegated lane — and
//! otherwise on B's own. Either consent alone is enough for the CREDENTIAL; the
//! executor's separate `work admit` decision (whether it runs A's work at all)
//! is a different consent in the opposite direction and both must hold.
//!
//! The resolver reads state over the same committed-query lane (`/v1/query`)
//! the agent provisioner uses, so it sees exactly what consensus committed —
//! and it runs in the COMPUTE DAEMON's process, so "the executing node" is the
//! node this daemon serves.

use compute_service::{CredentialResolver, Resolved};
use gateway::{CredentialRecord, GatewayQuery, GatewayReply, HandleRegistration};
use noded::node_link::NodeLink;
use provider_host::{AirlockConfig, CredentialKind, ResolvedCredential, WorkRef};

/// The gateway route label the co-hosted airlock gateway registers itself under
/// (`bin/node/src/boot/surfaces.rs`). A resolved credential's traffic is routed
/// to `<AIRLOCK_ROUTE>.<owner-handle>.duck`.
const AIRLOCK_ROUTE: &str = "airlock";

/// Resolves a run's named credential against committed state on the executing
/// node. Cheap to clone (holds the node lane and the local browser gateway
/// URL); built once at daemon boot.
pub(crate) struct NodeCredentialResolver {
    node: NodeLink,
    /// this node's browser-gateway base URL — the `via` a self-host airlock
    /// config routes through onto the overlay gateway plane. `None` on a node
    /// with no browser gateway, which then cannot host a lent-credential run.
    via: Option<String>,
}

impl NodeCredentialResolver {
    /// `via` is the node's browser-gateway base URL, read once at daemon boot
    /// from `GET /v1/gateway/browser`. `None` — a node serving no browser
    /// gateway — cannot host a lent-credential run, and says so at resolve.
    pub(crate) fn new(node: NodeLink, via: Option<String>) -> Self {
        Self { node, via }
    }

    /// Run one committed query over the node's `/v1` lane and return its raw
    /// reply bytes. The three module reads below each decode their own reply.
    async fn query(&self, target: &str, req: Vec<u8>) -> Result<Vec<u8>, String> {
        self.node.query(target, &req).await
    }

    async fn credential_record(&self, name: &str) -> Result<Option<CredentialRecord>, String> {
        let bytes = self
            .query(
                "gateway",
                gateway::encode_query(&GatewayQuery::Credential {
                    name: name.to_string(),
                }),
            )
            .await?;
        match gateway::decode_reply(&bytes)? {
            GatewayReply::Credential(record) => Ok(record),
            other => Err(format!("gateway returned an unexpected reply: {other:?}")),
        }
    }

    /// The `.duck` handle registered for `account_id`, if any. Scans the handle
    /// registration listing. ponytail: single page (MAX_QUERY_LIMIT handles); a
    /// network past that needs pagination here.
    async fn handle_of_account(&self, account_id: u64) -> Result<Option<String>, String> {
        let bytes = self
            .query(
                "gateway",
                gateway::encode_query(&GatewayQuery::Registrations {
                    from: 0,
                    limit: gateway::MAX_QUERY_LIMIT,
                }),
            )
            .await?;
        let registrations = match gateway::decode_reply(&bytes)? {
            GatewayReply::Registrations(list) => list,
            other => return Err(format!("gateway returned an unexpected reply: {other:?}")),
        };
        Ok(registrations
            .into_iter()
            .find(|r: &HandleRegistration| r.account_id == account_id)
            .map(|r| r.handle))
    }

    /// Build the broker's self-host config from a granted record: reach the
    /// owner's gateway at `<airlock>.<owner-handle>.duck` through this node's
    /// browser gateway, pinning the on-chain seal_pk (no TEE quote in self-host).
    ///
    /// `work` is the run's committed saga id, carried through so the lender can
    /// resolve it. This side reads nothing out of it.
    async fn build_airlock(
        &self,
        record: &CredentialRecord,
        work: WorkRef,
    ) -> Result<AirlockConfig, String> {
        let Some(via) = self.via.clone() else {
            return Err("this node has no browser gateway to route credential traffic".into());
        };
        // the record names its owning account outright; the handle is that
        // account's, not the publishing node's.
        let Some(handle) = self.handle_of_account(record.owner_account).await? else {
            return Err("credential owner has no registered duck handle".into());
        };
        let resolved = ResolvedCredential {
            name: record.name.clone(),
            kind: service_kind(record.kind),
            authority: format!("{AIRLOCK_ROUTE}.{handle}.duck"),
            via,
            seal_pk: record.seal_pk,
        };
        Ok(AirlockConfig::self_host(&resolved, work))
    }
}

#[async_trait::async_trait]
impl CredentialResolver for NodeCredentialResolver {
    /// `saga_id` becomes the session's [`WorkRef::Saga`] pointer verbatim. It is
    /// NOT interpreted here — this process cannot see the account the lender's
    /// node will stamp on the hop, so any local reading of the run's origin
    /// could only ever be a second, uncheckable answer to a question the lender
    /// resolves from consensus itself.
    async fn resolve(&self, credential: &str, saga_id: &str) -> Result<Resolved, String> {
        let record = self.credential_record(credential).await?;
        let record = routable(credential, record)?;
        let work = WorkRef::Saga {
            saga_id: saga_id.to_string(),
        };
        // one line per attempt (bounded by the saga's max_attempts), because
        // the refusal otherwise only ever appears in the saga's `error` field
        // — which no CLI surfaces and no operator is watching.
        let airlock = self
            .build_airlock(&record, work)
            .await
            .inspect_err(|error| {
                tracing::warn!(
                    target: "ducktape::compute",
                    reason = "credential_unroutable",
                    %error,
                    "a run's credential could not be resolved on this node"
                );
            })?;
        Ok(Resolved { airlock })
    }
}

/// All that survives of the old `authorize`: a name this node cannot route is a
/// refusal worth making here, because the alternative is dialling nothing.
///
/// Everything else it used to do was an authorization decision, and this layer
/// does not make those. It cannot: the only identity in the flow is the one the
/// lender's node stamps on the hop, which this process neither sees nor can
/// predict. A local re-check could therefore only ever disagree with the lender —
/// which is exactly what it did, refusing nothing the lender admits and admitting
/// runs the lender refuses.
///
/// Nor was it buying earliness. The lender's refusal lands in `start_broker`,
/// before `invoke` spawns the sandbox and before any paid upstream call.
fn routable(
    credential: &str,
    record: Option<CredentialRecord>,
) -> Result<CredentialRecord, String> {
    record.ok_or_else(|| format!("unknown credential: {credential}"))
}

/// Map the committed credential tag to the compute executor vendor vocabulary.
pub(crate) fn service_kind(kind: gateway::CredentialKind) -> CredentialKind {
    match kind {
        gateway::CredentialKind::Claude => CredentialKind::Claude,
        gateway::CredentialKind::Codex => CredentialKind::Codex,
        gateway::CredentialKind::AppleCodesign => CredentialKind::AppleCodesign,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const OWNER: u64 = 1;
    const SOMEONE_ELSE: u64 = 2;

    fn record(owner: u64, grants: &[u64]) -> CredentialRecord {
        CredentialRecord {
            name: "owner-claude-1".into(),
            owner_account: owner,
            publisher_node: b"pub-node".to_vec(),
            kind: gateway::CredentialKind::Claude,
            seal_pk: [3u8; 32],
            grants: grants.iter().copied().collect::<BTreeSet<_>>(),
        }
    }

    /// A name this node can route resolves, whoever the grant names. The grant is
    /// the LENDER's decision and is made against the account its node vouches for
    /// on the hop; re-deciding it here could only ever disagree, since this
    /// process cannot see that account.
    #[test]
    fn a_known_credential_resolves_without_any_account_decision() {
        let resolved = routable("owner-claude-1", Some(record(OWNER, &[])))
            .expect("a registered credential is routable");
        assert_eq!(resolved.name, "owner-claude-1");
        assert_eq!(resolved.seal_pk, [3u8; 32]);
    }

    /// The grant set is not read here — including the case that used to be
    /// refused locally. A record granting somebody else still ROUTES; the lender
    /// is what refuses it, and it refuses on the executing node's account, not on
    /// anything this side could have supplied.
    #[test]
    fn a_record_this_node_is_not_granted_still_routes() {
        let resolved = routable("owner-claude-1", Some(record(OWNER, &[SOMEONE_ELSE])))
            .expect("routing is not authorization");
        assert_eq!(resolved.owner_account, OWNER);
    }

    #[test]
    fn an_unknown_credential_is_refused() {
        let err = routable("ghost", None).unwrap_err();
        assert!(err.contains("unknown credential: ghost"), "got {err:?}");
    }

    #[test]
    fn kinds_map_across_the_boundary() {
        assert_eq!(
            service_kind(gateway::CredentialKind::Claude),
            CredentialKind::Claude
        );
        assert_eq!(
            service_kind(gateway::CredentialKind::Codex),
            CredentialKind::Codex
        );
        assert_eq!(
            service_kind(gateway::CredentialKind::AppleCodesign),
            CredentialKind::AppleCodesign
        );
    }
}
