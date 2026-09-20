//! `ducktape service run airlock` — the standalone credential-LENDING daemon.
//!
//! The node process no longer embeds a gateway. This serves the operator's own
//! `airlock-creds` store in its own process, with its own failure domain, and
//! reaches its node exactly the way the CLI does — over localhost `/v1`.
//!
//! ## the autonomous stance
//!
//! Nothing assigns work to this daemon. Its desired state is entirely local:
//! the credentials the operator registered with `ducktape user cred add`, and
//! the grants those credentials carry on chain. There is no roles module, so
//! there is no assignment to reconcile against and no stance machinery here to
//! build one from.
//!
//! ## inbound is transport, not protocol
//!
//! This daemon binds a LOOPBACK listener and its port is published as the
//! account's signed `airlock` gateway route. Overlay traffic for
//! `airlock.<handle>.duck` lands on the NODE's `Service::Gateway` stream plane,
//! which authenticates the WireGuard peer, names it as the caller node,
//! enforces the signed `RouteStatement` policy, and only then dials this
//! listener. That is strictly better than a directly-bound daemon: a keyless
//! service has no overlay identity to bind with, and the node's route policy is
//! a real enforcement layer a direct bind would not have.
//!
//! The invariant that matters survives either way: **no `/v1` request carries
//! that traffic, and nothing is ever pushed to this daemon over its node link.**
//! The link is used in exactly one direction, for exactly one thing — reading
//! committed state to decide a grant.
//!
//! ## TEE trust is bilateral; the node is uninvolved
//!
//! This gateway does not attest. Its trust anchor is the seal PUBLIC key on
//! consensus, which the borrower's broker pins from the credential record. An
//! enclave-attested lender is `bin/airlock-gateway`, a separate minimal binary,
//! and seal_pk pinning + quote verification stay strictly between client and
//! airlock. Neither path routes any of it through the node.
//!
//! ## it spawns nothing
//!
//! No provider set, no sandbox, no reaper, no VMs. A lending node is often a
//! laptop with no hypervisor access at all — no `/dev/kvm`, no guest images —
//! which is exactly why the hello path must not demand one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use airlock::server::{GrantAnswer, GrantQuestion};
use airlock::wire::WorkRef;
use gateway::{CredentialRecord, GatewayQuery, GatewayReply, credential_use_allowed};
use noded::node_link::NodeLink;
use saga::{SagaOrigin, SagaQuery, SagaReply, SagaStatus, SagaView};

use crate::config;
use crate::gateway_routes::RouteOwner;
use crate::services::{AIRLOCK_KIND, ServiceGrant};
use crate::work_admission::{CommittedReader, account_of_key};

/// The gateway route label this daemon publishes its loopback port under. A
/// borrower resolves `<AIRLOCK_ROUTE>.<owner-handle>.duck` to it. This is
/// the MODEL lane: its signed policy admits [`AIRLOCK_MODEL_REQUEST_BYTES`]
/// per request.
pub(crate) const AIRLOCK_ROUTE: &str = "airlock";
/// What the model lane admits per request: a `claude` turn carries multi-MB
/// conversation context and nothing bulkier. Pinned in the `airlock` route's
/// signed policy, which is the only ceiling there is — the gateway module
/// pins none, so each lane declares the number it means.
pub(crate) const AIRLOCK_MODEL_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// The SIGNING lane: the same enclave, under its own label, so a release
/// bundle's cap is the sign route's and never the model lane's. Only a TEE
/// gateway (`bin/airlock-gateway`) mounts `POST /sign/macos-bundle`, so only
/// `cred seal --vendor apple-codesign` publishes this route; the node the
/// enclave is fronted by binds the label to the same loopback port as
/// `airlock`. `ducktape release sign-bundle` resolves
/// `<AIRLOCK_SIGN_ROUTE>.<owner-handle>.duck`.
pub(crate) const AIRLOCK_SIGN_ROUTE: &str = "airlock-sign";
/// What the signing lane's route policy admits per request: the sealed
/// `.tar.zst` of an unsigned bundle. The enclave reads the body whole; this
/// number is the overlay hop's, published in the route statement.
pub(crate) const AIRLOCK_SIGN_REQUEST_BYTES: u64 = 256 * 1024 * 1024;
// the model lane is the smaller one, or the split is pointless
const _: () = assert!(AIRLOCK_MODEL_REQUEST_BYTES < AIRLOCK_SIGN_REQUEST_BYTES);

/// How long the grant gate waits on its node.
///
/// A session-open is INTERACTIVE — a borrower's run is blocked on this read —
/// and [`NodeLink`]'s own ceiling is sized for a submit that rides consensus, so
/// inheriting it would make every session-open cost two minutes.
///
/// Ten seconds does NOT mean a node answering slower is broken. `/v1/query`
/// crosses the node's command lane (unlike `/v1/status`, which deliberately does
/// not), and `http_ingress` is the 7th of 8 arms in the validator's
/// `select_biased!` — behind the 100 ms drain deadline. A catch-up stage can
/// hold the pump past ANY interactive ceiling, so this is not a health verdict
/// and no value would make it one. It is the point at which we stop blocking the
/// borrower and say [`GrantAnswer::Undetermined`] — which the borrower's
/// operator reads as "the lender's node did not answer", and retries, rather
/// than as a missing grant. Short and honestly named beats long and guessed.
const GRANT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Everything the daemon needs, resolved before any of it runs.
pub(crate) struct Airlock {
    pub(crate) grant: ServiceGrant,
    /// A [`config::ServiceConfig`], never a `Resolved`: the type has no field a
    /// secret could live in and `resolve_service` never opens `identity.key`,
    /// so this daemon HOLDING the node key is unrepresentable rather than
    /// merely unused — the same shape compute and agent take. Right here for
    /// the reason the module header gives: the lender signs nothing, submits no
    /// op, and reaches its node exactly the way the CLI does.
    pub(crate) service: config::ServiceConfig,
    pub(crate) http_base: String,
    /// where `node.toml` and `gateway-routes.json` live.
    pub(crate) workspace: std::path::PathBuf,
}

/// Serve until the process is stopped.
pub(crate) fn serve(airlock: Airlock) -> Result<(), Box<dyn std::error::Error>> {
    let Airlock {
        grant,
        service,
        http_base,
        workspace,
    } = airlock;
    serve_until(
        grant.display_id(),
        service.storage_dir,
        http_base,
        workspace,
        std::future::pending(),
    )
}

/// Serve until either a stop signal or `also_stop` resolves. [`serve`] is this
/// with the config unpacked and `also_stop` never resolving, so a test driving
/// this holds the real arming-before-publication ordering instead of a replica
/// of it — and a replica is what an earlier guard was: hoisting the arming out
/// of `block_on`, the production-only panic it exists to prevent, left that test
/// green. The arming itself now belongs to
/// [`crate::services::serve_until_stopped`], which every daemon enters through.
fn serve_until(
    instance: String,
    storage: PathBuf,
    http_base: String,
    workspace: PathBuf,
    also_stop: impl std::future::Future<Output = ()> + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::services::serve_until_stopped(also_stop, |stop| async move {
        let node = NodeLink::new(http_base).with_timeout(GRANT_QUERY_TIMEOUT);
        // WHOSE routes this node agrees to serve under the airlock label,
        // resolved before anything is published. A route entry is the
        // operator's consent to one (account, label) pair: consensus lets ANY
        // member publish a record naming this node as its publisher, so a label
        // alone would let a stranger's record — with an audience and method set
        // they wrote themselves — reach this lender's loopback API.
        let account = route_account(&node, &workspace).await?;
        run(instance, storage, node, account, workspace, stop).await
    })
}

/// Serve until `stop` resolves. Split from [`serve_until`] so the route's
/// lifetime — published before the listener is served, retired once the daemon
/// stops — is drivable without a resolved node config.
async fn run(
    instance: String,
    storage: PathBuf,
    node: NodeLink,
    account: u64,
    workspace: PathBuf,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Open the store BEFORE binding: a broken store must fail the process, not
    // leave a listener that 404s every session. Opening also mints the seal
    // keypair on first run, so `user cred add` has a stable public key to
    // publish even though no credential exists yet.
    let store = airlock_service::Store::open(&storage)?;
    let credentials = store.len();

    // A daemon start is the natural cadence to reap temps a killed writer left
    // in this workspace; no hot path pays for it.
    crate::gateway_routes::sweep_stale_temporaries(&workspace);

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();

    let router = store.router(committed_grant_check(node))?;

    // Register the loopback port only once the router exists: a route pointing
    // at a gateway that never came up is worse than no route at all.
    //
    // KNOWN RESIDUAL, not an oversight: this entry survives a death that runs no
    // code — SIGKILL, the OOM killer, `abort()`, power loss, parent death. The
    // node then keeps reverse-proxying authorized overlay ingress to a freed
    // ephemeral port, and nothing re-validates it before dialing. The borrower
    // is not misled (the node's connect-refused is a `GatewayFailure::Unavailable`
    // -> 502, which the broker names `airlock_gateway_unreachable`), so what is
    // missing is only the EVICTION, not the diagnosis. Its own PR: a stale entry
    // dropped after N consecutive connect-refusals in `gateway_plane`, or a lease
    // this beat renews. Deliberately not built here.
    let route = gateway::RouteName::named(AIRLOCK_ROUTE);
    crate::gateway_routes::register(&workspace, account, route.clone(), port)
        .map_err(|error| format!("register airlock gateway route: {error}"))?;

    tracing::info!(
        target: "ducktape::gateway",
        instance = %instance,
        credentials,
        route = AIRLOCK_ROUTE,
        "airlock daemon serving"
    );
    if credentials == 0 {
        tracing::warn!(
            target: "ducktape::gateway",
            reason = "airlock_store_empty",
            "no credentials registered yet — add one with: ducktape user cred add"
        );
    }

    // The registered port is a standing instruction to the node: reverse-proxy
    // RouteStatement-authorized overlay ingress to THIS loopback port. So the
    // entry must not outlive the process that owns it — a dead daemon's port is
    // one any local process may subsequently bind. Re-assert it on a beat (a
    // hand `gateway unbind` is corrected within one) and retire it on the way
    // out. Both are scoped to OUR port: a second daemon that took the route owns
    // it, and neither the beat nor the exit may touch a live entry that is its.
    let refresh = tokio::spawn(refresh_route(
        workspace.clone(),
        account,
        route.clone(),
        port,
    ));
    let served = tokio::select! {
        served = airlock::server::serve_router(listener, router) => served.map_err(Into::into),
        () = stop => Ok(()),
    };
    refresh.abort();
    // JOIN before retiring, not just abort: cancellation only lands at the
    // task's next await, so a re-register already in flight has to finish
    // first — otherwise it restores the entry right after we removed it.
    //
    // UNTESTED, deliberately: observing it needs the beat to be mid-write when
    // the stop lands, and HEARTBEAT is tens of seconds. The only way to see it
    // is to inject a tiny interval, which is a test waiting on time — worse than
    // no test. Deleting this line does not fail anything; keep it anyway.
    let _ = refresh.await;
    retire_route(&workspace, account, &route, port);
    served
}

/// The account this daemon's loopback route is bound FOR: the one the
/// operator's ACTIVE WALLET key is on, read from committed identity state. The
/// same account [`crate::gateway_routes`]'s `gateway bind` records, so an
/// operator-typed bind and this self-registration consent to the same pair.
async fn route_account(node: &NodeLink, workspace: &Path) -> Result<u64, String> {
    let key = crate::boot::surfaces::operator_wallet_key(workspace)
        .ok_or("no active wallet in this workspace — `ducktape wallet new <name>` first")?;
    account_of_key(node, &key).await?.ok_or_else(|| {
        "the active wallet key is on no account — `ducktape account create` first".to_string()
    })
}

/// A forever-retry loop logs attempt 1, then every 30th — the counter IS the
/// diagnosis, and an unconditional warn on a 10 s beat is a log bomb.
fn beat_is_worth_a_line(beats: u64) -> bool {
    const LOG_EVERY: u64 = 30;
    beats == 1 || beats.is_multiple_of(LOG_EVERY)
}

/// Re-assert the loopback route on the service heartbeat, so the port the node
/// proxies to can never disagree with the port this process serves on for
/// longer than one beat. Scoped to OUR port: a second daemon that took the route
/// keeps it, and this one goes quiet instead of flapping the entry every beat.
/// One counter PER CAUSE, each reset by the other: `attempts` has to mean
/// "consecutive beats of THIS failure", or a run that alternates between a
/// foreign owner and an unwritable workspace logs a total describing neither.
async fn refresh_route(
    workspace: PathBuf,
    account: u64,
    route: gateway::RouteName,
    port: u16,
) -> ! {
    let mut foreign_beats: u64 = 0;
    let mut failed_beats: u64 = 0;
    loop {
        tokio::time::sleep(crate::services::HEARTBEAT).await;
        match crate::gateway_routes::reassert(&workspace, account, &route, port) {
            Ok(RouteOwner::Vacant | RouteOwner::Ours) => {
                foreign_beats = 0;
                failed_beats = 0;
            }
            Ok(RouteOwner::Foreign) => {
                failed_beats = 0;
                foreign_beats += 1;
                if beat_is_worth_a_line(foreign_beats) {
                    tracing::warn!(
                        target: "ducktape::gateway",
                        attempts = foreign_beats,
                        reason = "route_owned_by_another_daemon",
                        "another airlock daemon owns this workspace's gateway route; \
                         this one serves nothing the node will reach"
                    );
                }
            }
            Err(error) => {
                foreign_beats = 0;
                failed_beats += 1;
                if beat_is_worth_a_line(failed_beats) {
                    tracing::warn!(
                        target: "ducktape::gateway",
                        attempts = failed_beats,
                        reason = "route_refresh_failed",
                        "airlock gateway route not re-registered: {error}"
                    );
                }
            }
        }
    }
}

/// Drop the loopback route on the way out — OURS only. Best effort: a workspace
/// that has become unwritable must not turn a clean stop into a failed one, but
/// leaving a live route pointing at a port nothing serves is worth a line.
fn retire_route(workspace: &Path, account: u64, route: &gateway::RouteName, port: u16) {
    match crate::gateway_routes::retire(workspace, account, route, port) {
        Ok(RouteOwner::Vacant | RouteOwner::Ours) => {}
        // once per shutdown, and it explains why the route outlives us.
        Ok(RouteOwner::Foreign) => tracing::info!(
            target: "ducktape::gateway",
            reason = "route_owned_by_another_daemon",
            "airlock gateway route left registered: another daemon owns it now"
        ),
        Err(error) => tracing::warn!(
            target: "ducktape::gateway",
            reason = "route_retire_failed",
            "airlock gateway route left registered: {error}"
        ),
    }
}

/// Whether this node LENDS nothing although it was asked to: the operator's
/// credential store holds registered credentials and no airlock grant exists,
/// so no daemon will ever serve them. `Some(count)` = say so; `None` = nothing
/// to lend, or the service is granted and the daemon's own absence is what
/// `service status` reports.
///
/// A store that cannot be READ is not evidence of lending, and neither is a
/// grant file that cannot be read evidence of an absent grant: the daemon fails
/// loudly on either, and a node boot must not guess. This COUNTS the store — it
/// never opens a credential, because the node process has no business
/// materializing a lending token to produce a number, and no business logging an
/// operator-chosen credential name.
pub(crate) fn lending_without_a_grant(storage: &Path, workspace: &Path) -> Option<usize> {
    let credentials =
        airlock_service::count_credentials(&airlock_service::cred_store_root(storage));
    let lends_nothing = credentials == 0;
    if lends_nothing {
        return None;
    }
    let Ok(grant) = crate::services::grant_for(workspace, AIRLOCK_KIND) else {
        return None;
    };
    if grant.is_some() {
        return None;
    }
    Some(credentials)
}

/// The longest work pointer this gate will turn into a `/v1/query`.
///
/// A REFUSAL, not an impossibility, and the difference is worth stating: the saga
/// module's `Trigger` bounds `spec` and `reply_payload` but NOT `saga_id`, so a
/// longer id is constructible. It is simply not a shape any product path emits
/// (`sched\x1f<name>`, `dispatch\x1fruns\x1f<id>`), and an unadmitted caller's
/// byte count is not this node's command lane's problem.
const MAX_WORK_POINTER_BYTES: usize = 512;

/// The committed-state grant gate the owner's own gateway enforces: given the
/// credential named, the NODE the node's proxy VOUCHED for, and the work the
/// session says it is for, resolve this node's own committed state and answer
/// whether that session may open.
///
/// This is the daemon's ONLY use of its node link, and every one of them is a
/// read.
fn committed_grant_check(node: NodeLink) -> airlock::server::GrantCheck {
    Arc::new(move |question: GrantQuestion| {
        let node = node.clone();
        Box::pin(async move { grant_answer(&node, &question).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = GrantAnswer> + Send>>
    })
}

/// Fail closed, but say WHICH closed door. A committed record that does not
/// admit the session is a refusal the borrower's operator can act on; a node
/// that did not answer is not — reporting it as a refusal sends them to add a
/// grant that already exists, which is the exact bug this taxonomy replaces.
///
/// ONE way in: **delegation** — [`delegated_answer`], for a session that
/// presented a pointer to committed work. The hop is node-to-node, and a node
/// is never an account (identity binds no node to anyone), so the caller has
/// no grant of its own to draw on; the grant belongs to the ACCOUNT whose
/// user-signed frame submitted the work, and the pointer is how the executing
/// node reaches it. An owner's own local broker never comes through here at
/// all — it dials the loopback listener, which wires no gate.
///
/// Both answers are recorded, and they are recorded differently on purpose. A
/// refusal names only a [`refuse`] token, at `debug`: it is reachable by anyone
/// who can reach the route, so everything about it is a stranger's input. An
/// admission is the owner's audit record and goes to [`admit`] at `info`,
/// because the person who needs it is the one who was not watching.
///
/// **Ordering mirrors [`delegated_answer`]'s doctrine**: a FREE refusal (one
/// decided on bytes already in the question, no committed read needed) runs
/// before the identity read. `WorkRef::Direct` is decided a priori — there is
/// never a committed record of who asked for a pty, so this arm always
/// refuses (`a_direct_session_never_delegates` pins it) — and must cost this
/// node's command lane nothing. Reading the credential record before this
/// match, as the code once did, meant every Direct question still paid one
/// `/v1/query` round trip it could never turn into an admission: any
/// admitted network member could loop a known credential name against a
/// Direct question and saturate the lender's gateway plane for free.
async fn grant_answer(reader: &dyn CommittedReader, question: &GrantQuestion) -> GrantAnswer {
    let saga_id = match &question.work {
        // Nothing to delegate against. An interactive session takes this arm by
        // construction: there is no committed record of who asked for a pty.
        WorkRef::Direct => return refuse("credential_not_granted"),
        WorkRef::Saga { saga_id } => saga_id,
    };
    let record = match committed_credential_record(reader, &question.credential).await {
        Ok(Some(record)) => record,
        Ok(None) => return refuse("credential_record_absent"),
        // The node is the AUTHORITY here, and it did not answer: a link timeout
        // ([`GRANT_QUERY_TIMEOUT`]), a refused connection while it restarts, a
        // resident whose `serving` is still None, a reply that would not decode.
        // Nothing is known about the grant, so nothing is claimed about it.
        Err(error) => {
            tracing::debug!(
                target: "ducktape::gateway",
                reason = "grant_authority_unavailable",
                "airlock session not decided: {error}"
            );
            return GrantAnswer::Undetermined;
        }
    };
    delegated_answer(reader, &record, question, saga_id).await
}

/// One resolved condition of the delegated check that needed a committed read.
/// THREE states for the same reason [`GrantAnswer`] has three: a read that did
/// not answer is not a "no".
enum Half {
    Yes,
    No,
    Unreadable,
}

/// **Delegation: a run submitted by A and executed on B draws on A's grant.**
///
/// FOUR conditions, all required, all resolved from this lender's OWN committed
/// state. A future reader must not "simplify" any of them away — each one is
/// here because dropping it hands a stranger somebody's paid subscription:
///
/// 1. **the work is still LIVE** (`status == Pending`). A saga's `assignee` is
///    never cleared on any terminal path, so without this one `Done` run is a
///    permanent, network-wide, unmetered draw: the executor re-POSTs the same
///    pointer forever and mints a fresh token each time (the session budget is
///    keyed on the credential and REFILLED on every open, so it caps nothing).
///    The owner would have nothing to revoke — the executor holds no grant, so
///    `user cred revoke` has no subject.
/// 2. **the work NAMES THIS CREDENTIAL.** Without it, one lease on A's saga
///    opens a session for any credential any lender serves that A happens to be
///    granted on — including Carol's, who never saw the saga and has no
///    relationship with the executor at all.
/// 3. **the caller is the saga's PINNED executor** — see [`pinned_to`] for why
///    the pin and not the lease. A pure byte compare: the vouched-for node IS
///    the pin, no identity in between.
/// 4. **the saga's origin key is on an account this credential is granted
///    to** — see [`submitter_is_granted`].
///
/// Drop 3 and B may point at ANY saga A ever submitted and draw on A's grant for
/// work A never assigned to it. Drop 4 and this is just the old rule wearing a
/// pointer. No condition is sufficient alone; the conjunction is the whole
/// security argument.
///
/// Nothing here trusts the caller beyond "which record to look up": the origin
/// is a signature-proven key (a user-signed frame relayed through
/// `/v1/submit/frame`, or the node's own re-signing `/v1/submit`), the pin and
/// the spec are what consensus committed, and keys are mapped to accounts
/// through the identity module. See [`airlock::wire::WorkRef`].
///
/// **Ordering is deliberate**: the three FREE conditions (1, 2 and 3, all
/// decided on bytes already read) run before the identity read, so a caller
/// pointing at finished, unrelated or somebody else's work costs this node's
/// command lane two queries rather than three.
async fn delegated_answer(
    reader: &dyn CommittedReader,
    record: &CredentialRecord,
    question: &GrantQuestion,
    saga_id: &str,
) -> GrantAnswer {
    let pointer_is_plausible = saga_id.len() <= MAX_WORK_POINTER_BYTES;
    if !pointer_is_plausible {
        return refuse("work_pointer_oversized");
    }
    let saga = match committed_saga(reader, saga_id).await {
        Ok(Some(saga)) => saga,
        // NOT a refusal. This lender simply cannot SEE that saga yet — a follower
        // behind head, or an id naming nothing at all. Reporting "no" here would
        // tell the borrower's operator to go add a grant they may already hold,
        // which is the exact bug the three-state taxonomy exists to prevent; a
        // 503 tells them to retry, and a run whose saga never commits fails on
        // its own lane rather than on this one.
        Ok(None) => return undecided("delegated_work_unseen"),
        Err(error) => {
            tracing::debug!(
                target: "ducktape::gateway",
                reason = "grant_authority_unavailable",
                "airlock session not decided: {error}"
            );
            return GrantAnswer::Undetermined;
        }
    };
    // ONE — free, and the difference between lending for a run and lending
    // forever. A terminal saga is finished work; there is nothing left to draw
    // for.
    let work_is_live = match saga.status {
        SagaStatus::Pending => true,
        SagaStatus::Done | SagaStatus::Failed | SagaStatus::TimedOut | SagaStatus::Cancelled => {
            false
        }
    };
    if !work_is_live {
        return refuse("delegated_work_finished");
    }
    // TWO — free. The committed work names exactly one credential; a session may
    // draw on that one and no other.
    let names_this_credential =
        credential_the_work_names(&saga.spec).as_deref() == Some(question.credential.as_str());
    if !names_this_credential {
        return refuse("delegated_work_names_another_credential");
    }
    // THREE — free, and it binds the pointer to THIS caller.
    if !pinned_to(&saga, &question.caller_node) {
        return refuse("delegated_caller_not_the_executor");
    }
    // FOUR — the one identity read.
    match submitter_is_granted(reader, record, &saga.origin).await {
        Half::Yes => admit(question, saga_id),
        Half::No => refuse("delegated_submitter_not_granted"),
        Half::Unreadable => GrantAnswer::Undetermined,
    }
}

/// How much of a node key the record names. The same 4-byte prefix every other
/// identity in this codebase's logs carries (`peer = %hex_bytes(&key[..4])` on
/// the join plane): enough for an owner to tell their handful of borrowers
/// apart and to correlate two lines, while the log itself stays a poor place to
/// harvest identities from. The full key is on chain for whoever needs it.
const CALLER_PREFIX_BYTES: usize = 4;

/// **The owner's record of a draw on their own subscription**: who, which
/// credential, when, and for what work.
///
/// The one `info` on this path, and it has to be one: `debug` is off at the
/// default filter, so a record nobody turned on is a record that does not exist
/// — and the owner is exactly the person who was not watching. The cadence
/// earns it. This fires once per session OPEN, not per request: a borrower's
/// broker opens one session per run and re-mints only when the 3600 s token
/// lapses, which is the `{session}` granularity the doctrine reserves `info`
/// for. The per-request line is the borrower's broker's (`ducktape::broker`, at
/// `debug`), and it belongs there — this is the lender's side, and a lender
/// counting a borrower's requests would be a different feature.
///
/// The credential NAME is here and DELIBERATELY still absent from [`refuse`]:
/// a refusal is reachable by any admitted member with a `sub` of their own
/// choosing, so naming it there writes a stranger's string into the owner's log.
/// By this line the name has been matched against a record consensus committed,
/// so it is one the owner registered themselves.
///
/// Never the token, the credential's value, or the caller's whole key — see
/// [`CALLER_PREFIX_BYTES`]. The saga id goes through `{:?}` rather than `{}`,
/// and that is not a style choice: a product saga id is `sched\x1f<name>`, and
/// `Debug` for a string is what ESCAPES that control byte instead of writing
/// it into the owner's terminal and the app's log ring. Same treatment the
/// compute intake gives its own `attempt = ?key`.
fn admit(question: &GrantQuestion, saga_id: &str) -> GrantAnswer {
    let caller = &question.caller_node[..question.caller_node.len().min(CALLER_PREFIX_BYTES)];
    tracing::info!(
        target: "ducktape::gateway",
        credential = %question.credential,
        caller = %noded::hex_bytes(caller),
        work = ?saga_id,
        "airlock session opened"
    );
    GrantAnswer::Granted
}

/// One refusal, one stable snake_case token. Never an account, a saga id, a
/// credential name or a token — a `reason` is greppable and countable, and this
/// ring is visible in the app.
fn refuse(reason: &'static str) -> GrantAnswer {
    tracing::debug!(target: "ducktape::gateway", reason, "airlock session refused");
    GrantAnswer::Refused
}

fn undecided(reason: &'static str) -> GrantAnswer {
    tracing::debug!(target: "ducktape::gateway", reason, "airlock session not decided");
    GrantAnswer::Undetermined
}

/// Which credential the COMMITTED work names, read the way the EXECUTOR's own
/// pool reads it: `WorkSpec.payload` is the run envelope verbatim, and
/// `envelope::prepare` is the single place that schema lives. `None` for a spec
/// that is not a work spec, a payload that is not the envelope, or a run that
/// names no credential — each of which entitles the pointer to nothing.
fn credential_the_work_names(spec: &[u8]) -> Option<String> {
    let work = dispatch::decode_work_spec(spec).ok()?;
    let envelope = String::from_utf8(work.payload).ok()?;
    compute_service::envelope::prepare(&envelope)
        .ok()?
        .credential
}

/// Condition three: is the vouched-for caller node the node this saga is
/// PINNED to?
///
/// **`pinned_assignee`, not `assignee`, and the choice is load-bearing.** The pin
/// is immutable — `Reassign` errors on a pinned saga, `Crank`'s re-lease returns
/// the same pin, and `Accept` no-ops once an assignee exists — whereas the LEASE
/// moves: for an UNPINNED saga a permissionless `Crank` re-leases through
/// `pick_assignee`, whose height an attacker in the capability pool can time by
/// choosing when to crank. Keying on the lease would let that rotation carry the
/// submitter's credential to whoever won the roll.
///
/// So an unpinned saga simply cannot delegate. That costs nothing real: every
/// credential-naming product path pins (`agent sched` always does), and this way
/// the rule is enforced HERE rather than inherited from an invariant three crates
/// away that nothing in this file could notice breaking.
///
/// It is also why the LEASE EXPIRY is deliberately not consulted. On a pinned
/// saga an expired lease means "that attempt lapsed and the next one is the same
/// node's", not "somebody else may hold it now" — refusing in that gap would
/// refuse live work. `status` is the freshness question, and condition one asks
/// it.
fn pinned_to(saga: &SagaView, caller_node: &[u8]) -> bool {
    saga.pinned_assignee.as_deref() == Some(caller_node)
}

/// Condition four: is the key that SUBMITTED this saga on an account the
/// credential admits?
///
/// `SagaOrigin::External` is the only attributable arm, and it is attributable
/// because the origin is the key whose verified frame signature carried the
/// op. A USER-signed frame (`ducktape agent run`, a scheduled run — relayed by
/// `/v1/submit/frame`) resolves to its account through `OfKey`; a NODE-signed
/// one (`/v1/submit` re-signs with the node's own key) resolves to nothing,
/// because no account is ever keyed by a node — so a run a node authored on
/// its own behalf draws on nobody's subscription. A saga a MODULE triggered
/// (the dispatch family: chat mention, pages/forge comment, the jobs board,
/// `RunsMsg::RequestRun`) names no account at this layer, so there is no grant
/// to check and no subject to draw as — refused. It closes nothing legitimate:
/// those payloads are composed in consensus by `runs::envelope`, which has no
/// credential field at all, so a module-origin saga cannot name a credential in
/// the first place. `System` is genesis, and the same.
///
/// Note this is the OPPOSITE call from `work_admission`, which ADMITS a
/// module-origin saga — deliberately, and the two are answering different
/// questions. "Will I spend my CPU on this?" has a defensible answer for
/// unattributable work (yes, it is bounded and it cannot name a credential).
/// "Whose subscription does this spend?" does not: there is no whose.
async fn submitter_is_granted(
    reader: &dyn CommittedReader,
    record: &CredentialRecord,
    origin: &SagaOrigin,
) -> Half {
    let submitter = match origin {
        SagaOrigin::External(key) => key,
        SagaOrigin::Module(_) => return Half::No,
        SagaOrigin::System => return Half::No,
    };
    match account_of_key(reader, submitter).await {
        Ok(Some(account)) => match credential_use_allowed(record, account) {
            true => Half::Yes,
            false => Half::No,
        },
        Ok(None) => Half::No,
        Err(_) => Half::Unreadable,
    }
}

/// Read one credential record from this node's committed gateway-module state
/// over `/v1/query`, so the gate sees exactly what consensus committed.
async fn committed_credential_record(
    reader: &dyn CommittedReader,
    name: &str,
) -> Result<Option<CredentialRecord>, String> {
    let request = gateway::encode_query(&GatewayQuery::Credential {
        name: name.to_string(),
    });
    let bytes = reader.read("gateway", request).await?;
    match gateway::decode_reply(&bytes)? {
        GatewayReply::Credential(record) => Ok(record),
        other => Err(format!("gateway returned an unexpected reply: {other:?}")),
    }
}

/// Read one saga from this node's committed saga-module state. `Ok(None)` is
/// "this node has not committed that saga", which is a genuinely different thing
/// from a read that failed — see [`delegated_answer`], which keeps them apart.
async fn committed_saga(
    reader: &dyn CommittedReader,
    saga_id: &str,
) -> Result<Option<SagaView>, String> {
    let request = saga::encode_query(&SagaQuery::Get {
        saga_id: saga_id.to_string(),
    });
    let bytes = reader.read("saga", request).await?;
    match saga::decode_reply(&bytes)? {
        SagaReply::Saga(view) => Ok(view),
        other => Err(format!("saga returned an unexpected reply: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the delegation gate ------------------------------------------------

    const CRED: &str = "owner-claude-1";
    /// the lender's node — the credential's `publisher_node`.
    const OWNER_NODE: &[u8] = b"owner-node";
    /// the owner's USER key, on account [`OWNER_ACCOUNT`].
    const OWNER_KEY: &[u8] = b"owner-key";
    const OWNER_ACCOUNT: u64 = 1;
    /// the borrower's node — the one a delegated saga is pinned to, and the
    /// one the proxy vouches for on the hop. On no account: nodes never are.
    const EXEC_NODE: &[u8] = b"executor-node";
    /// a third party's USER key, on account [`STRANGER_ACCOUNT`].
    const STRANGER_KEY: &[u8] = b"stranger-key";
    const STRANGER_ACCOUNT: u64 = 3;
    /// some other node — a node key, so on no account.
    const PEER_NODE: &[u8] = b"peer-node";
    const SAGA: &str = "sched\u{1f}delegated";

    /// A committed-state stand-in for the lender's own node. Answers the three
    /// reads the gate makes and nothing else; anything unexpected panics rather
    /// than defaulting, so a new read cannot slip in unnoticed.
    struct Committed {
        /// `None` = the lender has not committed this saga (a follower behind
        /// head, or an id naming nothing).
        saga: Option<SagaView>,
        /// which reads fail outright, by module target.
        unreadable: &'static [&'static str],
        grants: Vec<u64>,
    }

    impl Committed {
        fn new(origin: SagaOrigin, assignee: Option<&[u8]>) -> Self {
            Self {
                saga: Some(view(origin, assignee)),
                unreadable: &[],
                grants: Vec::new(),
            }
        }
    }

    /// The committed spec of a real `agent sched --cred <name>` run, composed
    /// through the SAME two producers the CLI uses — a hand-rolled JSON blob here
    /// would let the gate and the composer drift apart silently.
    fn spec_naming(credential: &str) -> Vec<u8> {
        dispatch::encode_work_spec(&dispatch::WorkSpec {
            kind: dispatch::WORK_SPEC_KIND.into(),
            dispatch_id: "delegated".into(),
            capability: "sched-claude".into(),
            payload: compute_service::envelope::compose_headless(SAGA, "PING", Some(credential))
                .into_bytes(),
            demands: Default::default(),
            admission: dispatch::AdmissionPolicy::Queue,
        })
    }

    fn view(origin: SagaOrigin, assignee: Option<&[u8]>) -> SagaView {
        SagaView {
            origin,
            reply_to: None,
            reply_payload: Vec::new(),
            spec: spec_naming(CRED),
            capability: None,
            status: SagaStatus::Pending,
            attempt: 0,
            max_attempts: 1,
            assignee: assignee.map(<[u8]>::to_vec),
            pinned_assignee: assignee.map(<[u8]>::to_vec),
            lease_views: None,
            lease_expires_at: None,
            deadline: None,
            result: None,
            error: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[async_trait::async_trait]
    impl CommittedReader for Committed {
        async fn read(&self, target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
            if self.unreadable.contains(&target) {
                return Err(format!("{target} did not answer"));
            }
            match target {
                "gateway" => Ok(gateway::encode_reply(&GatewayReply::Credential(Some(
                    CredentialRecord {
                        name: CRED.into(),
                        owner_account: OWNER_ACCOUNT,
                        publisher_node: OWNER_NODE.to_vec(),
                        kind: gateway::CredentialKind::Claude,
                        seal_pk: [3u8; 32],
                        grants: self.grants.iter().copied().collect(),
                    },
                )))),
                "saga" => Ok(saga::encode_reply(&SagaReply::Saga(self.saga.clone()))),
                "identity" => {
                    let identity::IdentityQuery::OfKey { key } =
                        identity::decode_query(&request).expect("an identity query")
                    else {
                        panic!("the gate asks identity only OfKey");
                    };
                    // user keys are on accounts; node keys never are.
                    let account = match key.as_slice() {
                        OWNER_KEY => Some(OWNER_ACCOUNT),
                        STRANGER_KEY => Some(STRANGER_ACCOUNT),
                        _node_or_unknown => None,
                    };
                    Ok(identity::encode_reply(&identity::IdentityReply::Account(
                        account.map(|number| identity::AccountView {
                            control: identity::Control::Keys,
                            number,
                            name: "someone".into(),
                            keys: Vec::new(),
                            avatar: None,
                            bio: None,
                            updated_at: 0,
                        }),
                    )))
                }
                other => panic!("the gate read {other:?}, which it has no business reading"),
            }
        }
    }

    fn question(caller_node: &[u8], work: WorkRef) -> GrantQuestion {
        GrantQuestion {
            credential: CRED.into(),
            caller_node: caller_node.to_vec(),
            work,
        }
    }

    fn pointer() -> WorkRef {
        WorkRef::Saga {
            saga_id: SAGA.into(),
        }
    }

    /// What ONE gate call wants captured: its buffer, and the level below which
    /// it wants nothing — the filter the subscriber used to own, moved to the
    /// writer because the subscriber is now shared by the whole binary.
    struct Capture {
        max: tracing::Level,
        lines: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    tokio::task_local! {
        /// The gate call currently being captured, if any. Task-local rather
        /// than thread-local because the seam being captured is an `await`.
        static CAPTURING: Capture;
    }

    /// The one writer the shared subscriber owns: it hands an event to the gate
    /// call currently being captured, and throws the line away when that is
    /// nobody — every other test in this binary — or when the event sits above
    /// the level that call asked for.
    struct RouteToTheCapturingCall;

    /// Where one formatted line goes: a capture's buffer, or nowhere.
    struct Route(Option<Arc<std::sync::Mutex<Vec<u8>>>>);

    impl std::io::Write for Route {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(lines) = &self.0 {
                lines.lock().unwrap().extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for RouteToTheCapturingCall {
        type Writer = Route;

        fn make_writer(&self) -> Route {
            Route(None)
        }

        fn make_writer_for(&self, meta: &tracing::Metadata<'_>) -> Route {
            Route(
                CAPTURING
                    .try_with(|capture| {
                        let wanted = *meta.level() <= capture.max;
                        wanted.then(|| capture.lines.clone())
                    })
                    .ok()
                    .flatten(),
            )
        }
    }

    /// Everything ONE gate call logged, captured off a subscriber installed ONCE
    /// for the whole test binary at TRACE.
    ///
    /// Process-wide, NOT the `with_subscriber` scope this used to use, and that
    /// is the whole fix. `tracing` caches a callsite's interest globally and
    /// computes it the first time the callsite is HIT; while exactly one
    /// dispatcher is registered, `tracing_core` takes a shortcut and asks *the
    /// hitting thread's current* dispatcher instead of the registry
    /// (`callsite::Dispatchers::rebuilder` → `Rebuilder::JustOne` →
    /// `dispatcher::get_default`). A scoped subscriber is current only on the
    /// thread inside its own future, so whichever sibling reached [`refuse`] or
    /// [`admit`] first — on a thread carrying no subscriber at all, i.e.
    /// `NoSubscriber`, which is interested in nothing — cached that callsite as
    /// `never` for the rest of the process, and every later capture of it came
    /// back EMPTY. Under the parallel test runner that was a 3% flake. A
    /// process-wide subscriber is the current one on EVERY thread, so the
    /// shortcut answers with it and the interest is always "yes".
    ///
    /// Which is why EVERY gate call in this module goes through here, including
    /// the ones that assert nothing about the log: the install has to happen
    /// before anything can register those callsites, and
    /// [`every_gate_call_goes_through_the_capture`] keeps it that way.
    ///
    /// `max` is load-bearing rather than decoration. The draw record is asserted
    /// at INFO — what the DEFAULT filter admits — because a record only
    /// reachable once somebody sets `RUST_LOG` is a record the owner never sees,
    /// and the owner not watching is the whole premise. Refusals are asserted at
    /// TRACE, so "it does not name the credential" cannot pass by hiding under a
    /// level.
    async fn logged(
        max: tracing::Level,
        state: &dyn CommittedReader,
        question: &GrantQuestion,
    ) -> (GrantAnswer, String) {
        static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INSTALLED.get_or_init(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_max_level(tracing::Level::TRACE)
                    .with_writer(RouteToTheCapturingCall)
                    .finish(),
            )
            .expect("no other unit test in this binary installs a global subscriber");
        });

        let lines: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let capture = Capture {
            max,
            lines: lines.clone(),
        };
        let answer = CAPTURING
            .scope(capture, grant_answer(state, question))
            .await;
        let text = String::from_utf8(lines.lock().unwrap().clone()).expect("utf8 log output");
        (answer, text)
    }

    /// The gate, for the tests that assert on the ANSWER. Still goes through
    /// [`logged`] — see there for why calling [`grant_answer`] directly from a
    /// test breaks a different test.
    async fn answered(state: &dyn CommittedReader, question: &GrantQuestion) -> GrantAnswer {
        logged(tracing::Level::TRACE, state, question).await.0
    }

    /// The capture works only because no callsite of this gate can be registered
    /// before [`logged`] has installed the subscriber. One direct
    /// [`grant_answer`] call from a test would register [`refuse`]/[`admit`] with
    /// no subscriber current, cache them as "never" for the whole binary, and
    /// silently empty the capture in whichever sibling ran later — so the shape
    /// is checked rather than asked for.
    #[test]
    fn every_gate_call_goes_through_the_capture() {
        let source = include_str!("airlock.rs");
        let (_, tests) = source.split_once("mod tests {").expect("the test module");
        assert_eq!(
            tests.matches(concat!("grant_answer", "(")).count(),
            1,
            "a test calls grant_answer directly instead of logged/answered"
        );
    }

    /// THE DELEGATED ADMISSION: the executor node is the pin AND the submitting
    /// key is on a granted account — here implicitly, because the submitter
    /// owns the credential. The executor node is on no account at all.
    #[tokio::test]
    async fn a_pointer_admits_the_pinned_node_on_the_submitters_grant() {
        let state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Granted
        );
    }

    /// **The owner's half of the record**: a delegated draw is precisely the
    /// case where a node holding NO grant spends the owner's subscription. One
    /// line, three questions.
    #[tokio::test]
    async fn an_admitted_draw_is_recorded_for_the_owner() {
        let state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        let (answer, log) = logged(
            tracing::Level::INFO,
            &state,
            &question(EXEC_NODE, pointer()),
        )
        .await;
        assert_eq!(answer, GrantAnswer::Granted);
        // WHO — the node the transport vouched for, by its prefix.
        let caller = noded::hex_bytes(&EXEC_NODE[..CALLER_PREFIX_BYTES]);
        assert!(log.contains(&format!("caller={caller}")), "{log}");
        // WHICH credential — the owner's own name for it, matched against the
        // record consensus committed.
        assert!(log.contains(&format!("credential={CRED}")), "{log}");
        // FOR WHAT WORK — the pointer this lender resolved, with the saga id's
        // `\x1f` escaped rather than written into the owner's terminal.
        assert!(log.contains(&format!("work={SAGA:?}")), "{log}");
        // WHEN is the subscriber's timestamp; nothing here supplies it.
        //
        // …and never the whole key: a log is a poor place to harvest
        // identities from, and the full key is on chain for whoever needs it.
        assert!(!log.contains(&noded::hex_bytes(EXEC_NODE)), "{log}");
    }

    /// A saga the executing NODE authored itself (`/v1/submit` re-signs with
    /// the node key) is on nobody's account, so it draws on nobody's
    /// subscription — even when that node is the pin. Only a user-signed frame
    /// carries a grant.
    #[tokio::test]
    async fn a_node_signed_saga_draws_on_nobody() {
        let state = Committed::new(SagaOrigin::External(EXEC_NODE.to_vec()), Some(EXEC_NODE));
        let (answer, log) = logged(
            tracing::Level::TRACE,
            &state,
            &question(EXEC_NODE, pointer()),
        )
        .await;
        assert_eq!(answer, GrantAnswer::Refused);
        assert!(
            log.contains(r#"reason="delegated_submitter_not_granted""#),
            "{log}"
        );
    }

    /// The refusal side is unchanged, and it has to STAY unchanged now that the
    /// admitted side names things. A refusal is reachable by any admitted member
    /// with a `sub` of their own choosing, so a later "make the two lines
    /// symmetric" refactor would hand a stranger a pen and the owner's log.
    #[tokio::test]
    async fn a_refusal_still_names_neither_the_credential_nor_the_caller() {
        let state = Committed::new(SagaOrigin::External(STRANGER_KEY.to_vec()), Some(EXEC_NODE));
        let (answer, log) = logged(
            tracing::Level::TRACE,
            &state,
            &question(EXEC_NODE, pointer()),
        )
        .await;
        assert_eq!(answer, GrantAnswer::Refused);
        assert!(
            log.contains(r#"reason="delegated_submitter_not_granted""#),
            "{log}"
        );
        assert!(!log.contains(CRED), "{log}");
        assert!(
            !log.contains(&noded::hex_bytes(&EXEC_NODE[..CALLER_PREFIX_BYTES])),
            "{log}"
        );
        assert!(!log.contains(SAGA), "{log}");
    }

    /// The `max` [`logged`] takes still FILTERS, now that it lives in the writer
    /// rather than in a per-call subscriber. Without this, a capture that took
    /// every level would keep passing if [`admit`] were ever demoted to `debug`
    /// — and the whole point of asserting the draw record at INFO is that the
    /// owner sees it under the DEFAULT filter. Same refusal as above, which is a
    /// `debug!`, asked for at INFO: nothing.
    #[tokio::test]
    async fn a_capture_below_the_events_level_records_nothing() {
        let state = Committed::new(SagaOrigin::External(STRANGER_KEY.to_vec()), Some(EXEC_NODE));
        let (answer, log) = logged(
            tracing::Level::INFO,
            &state,
            &question(EXEC_NODE, pointer()),
        )
        .await;
        assert_eq!(answer, GrantAnswer::Refused);
        assert_eq!(log, "");
    }

    /// The EXECUTOR condition. The origin is the owner, so a gate that checked
    /// only the origin admits this — and that is precisely the hole: every saga
    /// the owner ever submitted would become a key to the owner's subscription,
    /// for work the owner never assigned to this node.
    #[tokio::test]
    async fn a_caller_that_is_not_the_pinned_executor_is_refused() {
        let state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(PEER_NODE));
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Refused
        );
    }

    /// HALF TWO. The caller genuinely is the pin — dropping the origin check
    /// would make the pointer a universal key for any assignee.
    #[tokio::test]
    async fn a_submitter_the_credential_does_not_admit_is_refused() {
        let state = Committed::new(SagaOrigin::External(STRANGER_KEY.to_vec()), Some(EXEC_NODE));
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Refused
        );
    }

    /// …and an explicit grant to that submitter's account is the only thing
    /// that changes it. Same saga, same caller, same pin.
    #[tokio::test]
    async fn a_granted_submitter_delegates_without_owning_the_credential() {
        let mut state =
            Committed::new(SagaOrigin::External(STRANGER_KEY.to_vec()), Some(EXEC_NODE));
        state.grants = vec![STRANGER_ACCOUNT];
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Granted
        );
    }

    /// A saga this lender has NOT committed decides nothing. It is what a
    /// follower behind head sees, and a 403 there would send the borrower's
    /// operator to add a grant they may already hold.
    #[tokio::test]
    async fn a_saga_the_lender_cannot_see_is_undetermined_not_refused() {
        let mut state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        state.saga = None;
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Undetermined
        );
    }

    /// The same three-state rule for each read the delegated path makes.
    #[tokio::test]
    async fn a_read_that_did_not_answer_is_undetermined() {
        for unreadable in [&["saga"][..], &["identity"][..]] {
            let mut state =
                Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
            state.unreadable = unreadable;
            assert_eq!(
                answered(&state, &question(EXEC_NODE, pointer())).await,
                GrantAnswer::Undetermined,
                "an unanswered {unreadable:?} read must not read as a refusal"
            );
        }
    }

    /// A saga a MODULE triggered names no account, so there is no subject to
    /// draw as. Deliberately the OPPOSITE call from `work_admission`, which
    /// admits the same origin: "will I spend my CPU" has a defensible answer for
    /// unattributable work; "whose subscription does this spend" does not.
    #[tokio::test]
    async fn a_module_triggered_saga_names_no_account_to_draw_as() {
        for origin in [SagaOrigin::Module("dispatch".into()), SagaOrigin::System] {
            let state = Committed::new(origin, Some(EXEC_NODE));
            assert_eq!(
                answered(&state, &question(EXEC_NODE, pointer())).await,
                GrantAnswer::Refused
            );
        }
    }

    /// A session presenting no pointer has nothing to draw on: a node is
    /// never an account, so it holds no grant of its own — every interactive
    /// pty takes this arm and is refused, however delegable a saga exists.
    #[tokio::test]
    async fn a_direct_session_never_delegates() {
        let state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        assert_eq!(
            answered(&state, &question(EXEC_NODE, WorkRef::Direct)).await,
            GrantAnswer::Refused,
            "a delegable saga existing does not delegate a session that never named it"
        );
    }

    /// **A Direct question is decided a priori and must cost zero reads.**
    /// The refusal above (`a_direct_session_never_delegates`) proves the
    /// ANSWER; this proves the COST — the fake reader panics on any call at
    /// all, so this only stays green while the match on `question.work` runs
    /// before `committed_credential_record`.
    #[tokio::test]
    async fn a_direct_question_performs_zero_reads() {
        struct PanicsOnAnyRead;
        #[async_trait::async_trait]
        impl CommittedReader for PanicsOnAnyRead {
            async fn read(&self, target: &str, _request: Vec<u8>) -> Result<Vec<u8>, String> {
                panic!("a Direct question must never reach a committed read (target={target})");
            }
        }
        assert_eq!(
            answered(&PanicsOnAnyRead, &question(EXEC_NODE, WorkRef::Direct)).await,
            GrantAnswer::Refused
        );
    }

    /// **A finished run is not a standing licence.** The saga module never clears
    /// `assignee` on ANY terminal path, so without this the executor of one
    /// `Done` run re-POSTs the same pointer forever and mints a fresh token each
    /// time — a permanent, unmetered draw the owner cannot revoke, because the
    /// executor holds no grant to revoke.
    #[tokio::test]
    async fn a_finished_run_is_not_a_standing_licence() {
        for status in [
            SagaStatus::Done,
            SagaStatus::Failed,
            SagaStatus::TimedOut,
            SagaStatus::Cancelled,
        ] {
            let mut state =
                Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
            let saga = state.saga.as_mut().expect("the fixture commits a saga");
            saga.status = status;
            saga.attempt = 99;
            assert_eq!(
                answered(&state, &question(EXEC_NODE, pointer())).await,
                GrantAnswer::Refused,
                "a {status:?} saga must not still open sessions"
            );
        }
    }

    /// **The pointer buys ONE credential — the one the committed work names.**
    /// Without this, a single lease on A's saga opens a session for any
    /// credential any lender serves that A is granted on, including a third
    /// party's who never saw the saga and has no relationship with the executor.
    #[tokio::test]
    async fn a_session_may_not_name_a_credential_the_work_does_not() {
        let mut state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        state.saga.as_mut().expect("a saga").spec = spec_naming("a-totally-different-credential");
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Refused
        );
    }

    /// …and work whose credential cannot be read at all names none: a spec that
    /// is not a work spec, and a run that carries no credential.
    #[tokio::test]
    async fn work_that_names_no_credential_delegates_nothing() {
        for spec in [b"not a work spec at all".to_vec(), {
            dispatch::encode_work_spec(&dispatch::WorkSpec {
                kind: dispatch::WORK_SPEC_KIND.into(),
                dispatch_id: "d".into(),
                capability: "c".into(),
                payload: compute_service::envelope::compose_headless(SAGA, "PING", None)
                    .into_bytes(),
                demands: Default::default(),
                admission: dispatch::AdmissionPolicy::Queue,
            })
        }] {
            let mut state =
                Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
            state.saga.as_mut().expect("a saga").spec = spec;
            assert_eq!(
                answered(&state, &question(EXEC_NODE, pointer())).await,
                GrantAnswer::Refused
            );
        }
    }

    /// **The PIN is the binding, not the lease.** An unpinned saga's assignee
    /// moves — a permissionless `Crank` re-leases through `pick_assignee` at a
    /// height an attacker in the capability pool can choose, `Reassign` moves it
    /// outright, and `Accept` claims one that landed unassigned. Keying on the
    /// lease would carry the submitter's credential to whoever won that roll, so
    /// an unpinned saga delegates to nobody — including to the node currently
    /// holding its lease.
    #[tokio::test]
    async fn an_unpinned_saga_delegates_to_nobody() {
        let mut state = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        state.saga.as_mut().expect("a saga").pinned_assignee = None;
        assert_eq!(
            answered(&state, &question(EXEC_NODE, pointer())).await,
            GrantAnswer::Refused,
            "the lease alone is not the binding, even when it is this caller's"
        );
    }

    /// The three FREE conditions are decided before the identity read, so a
    /// caller pointing at finished, unrelated or somebody else's work costs
    /// this node's command lane two queries rather than three. The reader
    /// PANICS on `identity`.
    #[tokio::test]
    async fn a_pointer_at_the_wrong_work_costs_no_identity_read() {
        struct NoIdentity(Committed);
        #[async_trait::async_trait]
        impl CommittedReader for NoIdentity {
            async fn read(&self, target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
                assert_ne!(target, "identity", "a free condition already settled this");
                self.0.read(target, request).await
            }
        }
        let mut finished =
            Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        finished.saga.as_mut().expect("a saga").status = SagaStatus::Done;
        let mut other = Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(EXEC_NODE));
        other.saga.as_mut().expect("a saga").spec = spec_naming("some-other-credential");
        let somebody_elses =
            Committed::new(SagaOrigin::External(OWNER_KEY.to_vec()), Some(PEER_NODE));
        for state in [finished, other, somebody_elses] {
            assert_eq!(
                answered(&NoIdentity(state), &question(EXEC_NODE, pointer())).await,
                GrantAnswer::Refused
            );
        }
    }

    /// A pointer nobody could have committed is refused before it becomes a
    /// query: an unadmitted caller's byte count is not this node's problem.
    #[tokio::test]
    async fn an_oversized_pointer_is_refused_before_it_is_looked_up() {
        struct RecordThenPanic;
        #[async_trait::async_trait]
        impl CommittedReader for RecordThenPanic {
            async fn read(&self, target: &str, _request: Vec<u8>) -> Result<Vec<u8>, String> {
                assert_eq!(target, "gateway", "an oversized pointer is never looked up");
                Ok(gateway::encode_reply(&GatewayReply::Credential(Some(
                    CredentialRecord {
                        name: CRED.into(),
                        owner_account: OWNER_ACCOUNT,
                        publisher_node: OWNER_NODE.to_vec(),
                        kind: gateway::CredentialKind::Claude,
                        seal_pk: [3u8; 32],
                        grants: Default::default(),
                    },
                ))))
            }
        }
        let oversized = WorkRef::Saga {
            saga_id: "x".repeat(MAX_WORK_POINTER_BYTES + 1),
        };
        assert_eq!(
            answered(&RecordThenPanic, &question(EXEC_NODE, oversized)).await,
            GrantAnswer::Refused
        );
    }

    /// one complete credential dir under `<storage>/airlock-creds/<name>/`,
    /// exactly the shape `ducktape user cred add` writes.
    fn seed_credential(storage: &Path, name: &str) {
        let dir = airlock_service::cred_store_root(storage).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kind"), "codex\n").unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"tok"}}"#,
        )
        .unwrap();
    }

    fn grant_airlock(workspace: &Path) {
        std::fs::write(
            workspace.join(crate::services::FILE_NAME),
            "version = 1\n\n[[service]]\nkind = \"airlock\"\ninstance = \"".to_string()
                + &"ab".repeat(32)
                + "\"\nnonce = \""
                + &"cd".repeat(16)
                + "\"\ngranted_unix = 1700000000\ncapabilities = []\nscopes = []\n",
        )
        .unwrap();
    }

    /// The upgrade an operator lands in without asking: credentials registered,
    /// no grant, so nothing lends them and every other diagnostic still looks
    /// healthy. This predicate is the only thing that notices.
    ///
    /// Storage and workspace are DIFFERENT dirs, as they are in the real shape
    /// (`config::resolve`): passing one dir for both would let the two arguments
    /// be transposed and the test still pass.
    #[test]
    fn a_populated_store_with_no_grant_is_the_one_state_worth_warning_about() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        // nothing registered: there is nothing to lend, so nothing to say.
        assert_eq!(lending_without_a_grant(&storage, &workspace), None);

        // a credential and no grant — the silent-loss shape.
        seed_credential(&storage, "owner-codex-1");
        assert_eq!(lending_without_a_grant(&storage, &workspace), Some(1));

        // granted: the daemon's absence is `service status`'s job to report,
        // not this line's.
        grant_airlock(&workspace);
        assert_eq!(lending_without_a_grant(&storage, &workspace), None);
    }

    /// Both documented silences. The warn claims an operator's credentials are
    /// going unlent, so it must fire only on EVIDENCE: a store it could not read
    /// proves no lending, and a grant file it could not parse proves no missing
    /// grant. Guessing either way is a false alarm on a healthy node.
    #[test]
    fn a_state_it_cannot_read_is_never_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        // no store at all (never `cred add`ed): nothing is registered.
        assert_eq!(lending_without_a_grant(&storage, &workspace), None);

        // a store, and a services.toml that does not parse. The grant may well
        // exist; we cannot tell, so we say nothing.
        seed_credential(&storage, "owner-codex-1");
        std::fs::write(
            workspace.join(crate::services::FILE_NAME),
            "this is not toml {{{",
        )
        .unwrap();
        assert_eq!(lending_without_a_grant(&storage, &workspace), None);
    }

    /// The registered port is a standing instruction to the node's reverse
    /// proxy, so its lifetime must be exactly the daemon's: published before the
    /// listener serves, gone once the daemon stops.
    ///
    /// It enters through [`crate::services::serve_until_stopped`], the ONE
    /// arming site every daemon uses — [`serve_until`] is that call plus the
    /// account resolution a node has to answer. Arming installs a real signal
    /// handler, which PANICS outside a reactor rather than erroring; a refactor
    /// hoisting it out of `block_on` is a production-only crash with no
    /// compile-time complaint.
    ///
    /// What it does NOT cover: the beat task's abort-then-join. See the comment
    /// on that line for why it has no honest test seam.
    #[test]
    fn the_route_lives_exactly_as_long_as_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let storage = dir.path().join("storage");
        let route = gateway::RouteName::named(AIRLOCK_ROUTE);

        // The stop future's FIRST poll happens inside `run`'s select!, i.e.
        // after the route is registered and the router is being served. That is
        // the daemon's own "serving" event, so this observation waits on the
        // system rather than on a clock.
        let observed = Arc::new(std::sync::Mutex::new(None));
        let seen = observed.clone();
        let peek = (workspace.clone(), route.clone());
        // the account the operator bound this label for; in production
        // `serve_until` reads it off committed identity state first.
        const ACCOUNT: u64 = 7;
        let stop = async move {
            *seen.lock().unwrap() = crate::gateway_routes::load(&peek.0)
                .unwrap()
                .port(ACCOUNT, &peek.1);
        };

        crate::services::serve_until_stopped(stop, |stop| {
            run(
                "airlock#test".into(),
                storage,
                // never dialed: no session is opened in this test.
                NodeLink::new("http://127.0.0.1:1".to_string()),
                ACCOUNT,
                workspace.clone(),
                stop,
            )
        })
        .expect("a stopped daemon exits cleanly");

        let served_on = observed
            .lock()
            .unwrap()
            .expect("the route must be published before the daemon serves");
        assert_ne!(
            served_on, 0,
            "a registered route names a real loopback port"
        );
        assert_eq!(
            crate::gateway_routes::load(&workspace)
                .unwrap()
                .port(ACCOUNT, &route),
            None,
            "a stopped daemon leaves no route pointing at a port any process may now bind"
        );
    }
}
