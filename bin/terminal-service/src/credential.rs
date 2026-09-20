//! Terminal credential metadata resolved from committed Gateway queries.
//! Work admission runs before this resolver. The lender retains grant checks.
use agent_service::wire;
use provider_host::{CredentialKind, ResolvedCredential};
use std::future::Future;

use crate::gateway_contract as gateway;

pub struct Resolved {
    pub credential: wire::Credential,
    pub limits: std::collections::BTreeMap<String, u64>,
}

/// Resolve only public routing metadata; never accept a client-supplied key or credential.
pub async fn resolve<F: Future<Output = Result<Vec<u8>, String>>>(
    provider: &str,
    name: &str,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
    sandbox_present: bool,
    via: String,
    mut query: impl FnMut(Vec<u8>) -> F,
) -> Result<Resolved, (&'static str, String)> {
    let record = credential_record(&mut query, name)
        .await
        .map_err(|detail| ("unknown_credential", detail))?;
    let admitted = admit_create(provider, record.as_ref(), cpu, mem_gb, sandbox_present)?;
    let authority = owner_airlock_authority(&mut query, admitted.owner_account)
        .await
        .map_err(|detail| ("unknown_credential", detail))?;
    let credential = agent_service::credential_wire(&ResolvedCredential {
        name: admitted.name,
        kind: admitted.kind,
        authority,
        via,
        seal_pk: admitted.seal_pk,
    });
    Ok(Resolved {
        credential,
        limits: admitted.limits,
    })
}

fn service_kind(kind: gateway::CredentialKind) -> CredentialKind {
    match kind {
        gateway::CredentialKind::Claude => CredentialKind::Claude,
        gateway::CredentialKind::Codex => CredentialKind::Codex,
        gateway::CredentialKind::AppleCodesign => CredentialKind::AppleCodesign,
    }
}

/// the host's create decision, given committed state already fetched. Pure so it
/// is unit-testable without a pty. `Ok` carries the resolved credential pieces +
/// container limits; `Err` is a `(reason, detail)`.
#[derive(Debug)]
struct AdmitOk {
    name: String,
    kind: provider_host::CredentialKind,
    seal_pk: [u8; 32],
    owner_account: u64,
    limits: std::collections::BTreeMap<String, u64>,
}

/// What survives is what this HOST knows about itself and the record: can it
/// sandbox, does the name exist, does the requested provider contradict the
/// credential's vendor, what limits apply. The grant check that used to sit here
/// does not: it decided, against a creator account this node resolved, a question
/// the lender decides against the account it vouches for — and the two are
/// different parties the moment the creator is not the host.
fn admit_create(
    provider: &str,
    record: Option<&gateway::CredentialRecord>,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
    sandbox_present: bool,
) -> Result<AdmitOk, (&'static str, String)> {
    if !sandbox_present {
        // `sandbox_present` is `has_sandbox()` = "is an agent service attached",
        // NOT "is a sandbox image configured" — word it as the fact it tested
        // (and as `refused_from_term_error` already does). The old "no
        // configured sandbox image" text sent an operator with a perfectly
        // good `[sandbox]` table hunting the wrong config.
        return Err((
            "no_sandbox",
            "this node has no agent service attached".into(),
        ));
    }
    let Some(record) = record else {
        return Err((
            "unknown_credential",
            "no credential by that name is registered".into(),
        ));
    };
    let contradicts = provider_contradicts_kind(provider, record.kind);
    if contradicts {
        return Err((
            "provider_kind_mismatch",
            format!("provider {provider} contradicts the credential kind"),
        ));
    }
    Ok(AdmitOk {
        name: record.name.clone(),
        kind: service_kind(record.kind),
        seal_pk: record.seal_pk,
        owner_account: record.owner_account,
        limits: build_limits(cpu, mem_gb),
    })
}

/// true when an EXPLICIT vendor provider tag contradicts the credential's kind.
/// An unknown tag (a test provider) is not a contradiction — the manager's
/// provider resolution decides it (→ `unknown_provider`).
fn provider_contradicts_kind(provider: &str, kind: gateway::CredentialKind) -> bool {
    match provider {
        "claude" => kind != gateway::CredentialKind::Claude,
        "codex" => kind != gateway::CredentialKind::Codex,
        _ => false,
    }
}

/// the size a session's sandbox is built at when the caller named none.
///
/// Matches the delegated-child profile the runs module uses: enough for one
/// interactive CLI, small enough that a host running several is not surprised.
const DEFAULT_SESSION_CORES: u64 = 2;
const DEFAULT_SESSION_MEM_GB: u64 = 4;

/// `--cpu`/`--mem` → the limit keys the sandbox backend enforces, with this
/// service's default size filled in for whatever the caller left out.
///
/// A microVM is BUILT at a size and has no "unlimited" state: the provider
/// refuses a run with no `cores` outright. So the one place a create is
/// performed names the size, for the local path and the credential path alike —
/// a create carrying neither `--cpu` nor `--mem` is the ordinary case.
pub(crate) fn build_limits(
    cpu: Option<u64>,
    mem_gb: Option<u64>,
) -> std::collections::BTreeMap<String, u64> {
    std::collections::BTreeMap::from([
        ("cores".to_string(), cpu.unwrap_or(DEFAULT_SESSION_CORES)),
        (
            "mem_gb".to_string(),
            mem_gb.unwrap_or(DEFAULT_SESSION_MEM_GB),
        ),
    ])
}

/// the committed credential record for `name`, or `None` when unregistered.
async fn credential_record<F: Future<Output = Result<Vec<u8>, String>>>(
    query: &mut impl FnMut(Vec<u8>) -> F,
    name: &str,
) -> Result<Option<gateway::CredentialRecord>, String> {
    let reply = query(gateway::encode_query(&gateway::GatewayQuery::Credential {
        name: name.to_string(),
    }))
    .await?;
    match gateway::decode_reply(&reply)? {
        gateway::GatewayReply::Credential(record) => Ok(record),
        _ => Err("unexpected gateway credential reply".into()),
    }
}

/// the `airlock.<handle>.duck` authority for the credential owner's co-hosted
/// gateway, resolved from the owner account's `.duck` handle registration.
async fn owner_airlock_authority<F: Future<Output = Result<Vec<u8>, String>>>(
    query: &mut impl FnMut(Vec<u8>) -> F,
    owner_account: u64,
) -> Result<String, String> {
    // the registrations query is paginated and the module HARD-CAPS a page at
    // MAX_QUERY_LIMIT (a larger `limit` is rejected outright), so page through in
    // MAX_QUERY_LIMIT chunks until the owner's handle is found or a short page
    // marks the end of the listing.
    let mut from = 0u64;
    loop {
        let reply = query(gateway::encode_query(
            &gateway::GatewayQuery::Registrations {
                from,
                limit: duckdns::MAX_QUERY_LIMIT,
            },
        ))
        .await?;
        let page = match gateway::decode_reply(&reply)? {
            gateway::GatewayReply::Registrations(registrations) => registrations,
            _ => return Err("unexpected gateway registrations reply".into()),
        };
        let owned = page
            .iter()
            .find(|registration| registration.account_id == owner_account);
        if let Some(registration) = owned {
            return Ok(format!("airlock.{}.duck", registration.handle));
        }
        let page_len = page.len() as u64;
        let listing_exhausted = page_len < duckdns::MAX_QUERY_LIMIT;
        if listing_exhausted {
            return Err("credential owner has no registered duck handle".into());
        }
        from += page_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rec(
        name: &str,
        owner: u64,
        grants: &[u64],
        kind: gateway::CredentialKind,
    ) -> gateway::CredentialRecord {
        gateway::CredentialRecord {
            name: name.into(),
            owner_account: owner,
            publisher_node: vec![9u8; 32],
            kind,
            seal_pk: [1u8; 32],
            grants: grants.iter().copied().collect(),
        }
    }

    /// What the HOST decides, and the boundary of it. Every case here is a fact
    /// about this host or about the record; none is a fact about who is asking.
    ///
    /// The grant is deliberately absent, including for an account the record does
    /// NOT name: a record granting somebody else still admits here, because the
    /// account this host would have checked is not the account the lender checks.
    /// The lender authorizes the account its own node stamps on the gateway hop —
    /// this host's — and it refuses at `/session`, before the sandbox spawns.
    #[test]
    fn admit_gates_on_sandbox_credential_and_kind_but_never_on_who_is_asking() {
        let claude = rec("c1", 1, &[2], gateway::CredentialKind::Claude);

        // no sandbox → refused before any credential decision.
        assert_eq!(
            admit_create("claude", Some(&claude), None, None, false)
                .unwrap_err()
                .0,
            "no_sandbox"
        );
        // unknown credential.
        assert_eq!(
            admit_create("claude", None, None, None, true)
                .unwrap_err()
                .0,
            "unknown_credential"
        );
        // a record this host is on nobody's grant list for still admits: routing
        // is not authorization, and the lender has not been asked yet.
        assert!(admit_create("claude", Some(&claude), None, None, true).is_ok());
        // an explicit provider contradicting the cred's kind is refused.
        assert_eq!(
            admit_create("codex", Some(&claude), None, None, true)
                .unwrap_err()
                .0,
            "provider_kind_mismatch"
        );
        // an unknown provider tag is not a contradiction (the manager resolves it).
        assert!(admit_create("echo", Some(&claude), Some(1), Some(2), true).is_ok());
    }

    #[test]
    fn admit_maps_limits_and_kind() {
        let codex = rec("x", 1, &[], gateway::CredentialKind::Codex);
        let ok = admit_create("codex", Some(&codex), Some(64), Some(256), true).unwrap();
        assert_eq!(ok.limits.get("cores"), Some(&64));
        assert_eq!(ok.limits.get("mem_gb"), Some(&256));
        assert!(matches!(ok.kind, provider_host::CredentialKind::Codex));
    }

    #[tokio::test]
    async fn resolver_pages_committed_handles_and_preserves_the_seal_and_limits() {
        let mut requests = Vec::new();
        let resolved = resolve(
            "codex",
            "lent",
            Some(2),
            Some(4),
            true,
            "http://127.0.0.1:3000".into(),
            |bytes| {
                let request = gateway::decode_query(&bytes).unwrap();
                requests.push(request.clone());
                let reply = match request {
                    gateway::GatewayQuery::Credential { name } => {
                        assert_eq!(name, "lent");
                        gateway::GatewayReply::Credential(Some(rec(
                            "lent",
                            42,
                            &[],
                            gateway::CredentialKind::Codex,
                        )))
                    }
                    gateway::GatewayQuery::Registrations { from, limit } => {
                        assert_eq!(limit, duckdns::MAX_QUERY_LIMIT);
                        let registrations = match from {
                            0 => (0..limit)
                                .map(|number| duckdns::HandleRegistration {
                                    account_id: 1,
                                    handle: format!("other{number}"),
                                })
                                .collect(),
                            duckdns::MAX_QUERY_LIMIT => vec![duckdns::HandleRegistration {
                                account_id: 42,
                                handle: "lender".into(),
                            }],
                            _ => panic!("unexpected registration page"),
                        };
                        gateway::GatewayReply::Registrations(registrations)
                    }
                };
                std::future::ready(Ok(gateway::encode_reply(&reply)))
            },
        )
        .await
        .unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(resolved.credential.name, "lent");
        assert_eq!(resolved.credential.authority, "airlock.lender.duck");
        assert_eq!(resolved.credential.via, "http://127.0.0.1:3000");
        assert_eq!(resolved.credential.seal_pk, [1; 32]);
        assert_eq!(resolved.limits.get("cores"), Some(&2));
        assert_eq!(resolved.limits.get("mem_gb"), Some(&4));
    }

    #[tokio::test]
    async fn resolver_refuses_query_failure_wrong_reply_and_missing_owner() {
        for first in [
            Err("query unavailable".into()),
            Ok(b"{}".to_vec()),
            Ok(gateway::encode_reply(
                &gateway::GatewayReply::Registrations(vec![]),
            )),
            Ok(gateway::encode_reply(&gateway::GatewayReply::Credential(
                None,
            ))),
        ] {
            let mut replies = [first].into_iter();
            let result = resolve(
                "codex",
                "lent",
                None,
                None,
                true,
                "http://localhost".into(),
                |_| std::future::ready(replies.next().expect("no further query after failure")),
            )
            .await;
            assert!(matches!(result, Err(("unknown_credential", _))));
        }
        let mut replies = [
            Ok(gateway::encode_reply(&gateway::GatewayReply::Credential(
                Some(rec("lent", 42, &[], gateway::CredentialKind::Codex)),
            ))),
            Ok(gateway::encode_reply(
                &gateway::GatewayReply::Registrations(vec![]),
            )),
        ]
        .into_iter();
        let result = resolve(
            "codex",
            "lent",
            None,
            None,
            true,
            "http://localhost".into(),
            |_| std::future::ready(replies.next().expect("no query beyond final page")),
        )
        .await;
        assert!(matches!(result, Err(("unknown_credential", _))));
    }

    /// A create that names no size still names one: a microVM has no unlimited
    /// state, so an empty limit map is a session that cannot boot.
    #[test]
    fn a_session_is_always_built_at_a_size() {
        let named = build_limits(Some(8), Some(16));
        assert_eq!(named["cores"], 8);
        assert_eq!(named["mem_gb"], 16);

        let bare = build_limits(None, None);
        assert_eq!(bare["cores"], DEFAULT_SESSION_CORES);
        assert_eq!(bare["mem_gb"], DEFAULT_SESSION_MEM_GB);

        // one named dimension does not leave the other unset.
        let half = build_limits(Some(6), None);
        assert_eq!(half["cores"], 6);
        assert_eq!(half["mem_gb"], DEFAULT_SESSION_MEM_GB);
    }
}
