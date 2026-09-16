//! Local RPC and network/app ingress handlers.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Clock as _, Metrics as _};

use sdk::Msg;

use super::{ValidatorRuntime, graceful_checkpoint};
use crate::config::{hex_bytes, unhex};
use crate::constants::{GATE_SETTLE_TIMEOUT, OPS_REFRESH_INTERVAL, SUBMIT_HOLD};
use crate::host_reads::{read_redemption_from_host, read_valset_members, read_valset_residents};
use crate::rpc::{JoinRequestView, JoinStateView, RpcJob, RpcReply, RpcRequest, RpcStatus};
use crate::util::{hex, unix_ms};
use crate::{config, join_gate, relay, relay_runtime};

impl ValidatorRuntime<'_> {
    async fn refresh_operations(&self, exposition: &str) {
        let members = self.orchestrator.current_members();
        let me = self.signer.public_key();
        let reachable = reachable_validators(exposition, members, &me);
        self.metrics.update_consensus(
            self.orchestrator.epoch(),
            self.node.finalized_view().unwrap_or(0),
            members.len() as u64,
            reachable,
            // CUSTODY, not the flush queue: `pending_batch` empties on every
            // flush while the frames it held are still unresolved, so a gauge
            // reading it reported ~0 for a mempool at its cap. custody is what
            // `node::MAX_CUSTODY_FRAMES` bounds and what a flood fills.
            (self.node.custody_len() + self.node.orderer().pending_len()) as u64,
        );
        self.metrics.update_storage(
            self.prev_ckpt.0.unwrap_or_default(),
            self.index.is_poisoned(),
            self.index.module_ids().into_iter().map(|module| {
                let height = self.index.applied_height(&module).unwrap_or_default();
                (module, height)
            }),
        );
    }

    /// the ONE observability publish seam: refresh the pricier sections
    /// (throttled to `OPS_REFRESH_INTERVAL` — the exposition parse AND the
    /// valset standing reads ride the same pace), then publish this node's
    /// boundary snapshot into the shared cell. called at startup and at the
    /// end of every drain turn.
    pub(super) async fn publish_status(&mut self) {
        if self.context.current() >= self.next_ops_refresh {
            let exposition = self.context.encode();
            self.refresh_operations(&exposition).await;
            // the peers standing: committed valset roles + chain position for
            // the off-lane /v1/peers composition. roles move only on valset
            // change, so the 1/s pace bounds staleness far below block rate.
            let hex_set = |keys: Vec<Vec<u8>>| keys.iter().map(|k| hex_bytes(k)).collect();
            self.status.publish_peers(noded::PeersStanding {
                validators: hex_set(
                    read_valset_members(self.node.host())
                        .await
                        .unwrap_or_default(),
                ),
                residents: hex_set(read_valset_residents(self.node.host()).await),
                height: self.node.finalized().map(|f| f.height).unwrap_or(0),
                epoch: Some(self.orchestrator.epoch()),
                // a validator SERVES the detection lane and polls nobody, so
                // it hears no peer's build stamp and reports every peer's as
                // unknown. the poller — a parked or folding resident — is the
                // side that learns one, and the side that warns.
                builds: Default::default(),
            });
            self.next_ops_refresh = self.context.current() + OPS_REFRESH_INTERVAL;
        }
        super::publish_boundary_status(
            &self.status,
            &self.node,
            &self.metrics,
            &self.status_public_key,
        );
    }

    /// one direct-peer sample: the exposition parse plus valset standing,
    /// stamped with this validator's own chain position.
    async fn peers_sample(&self) -> noded::peers::PeersView {
        let hex_set = |keys: Vec<Vec<u8>>| keys.iter().map(|k| hex_bytes(k)).collect();
        let validators = hex_set(
            read_valset_members(self.node.host())
                .await
                .unwrap_or_default(),
        );
        let residents = hex_set(read_valset_residents(self.node.host()).await);
        let height = self.node.finalized().map(|f| f.height).unwrap_or(0);
        let epoch = Some(self.orchestrator.epoch());
        noded::peers::peers_from_exposition(&self.context.encode(), unix_ms(), height, epoch)
            .with_roles(&validators, &residents)
    }

    pub(super) async fn on_rpc(
        &mut self,
        RpcJob {
            req,
            reply,
            written,
        }: RpcJob,
    ) {
        let Self {
            node,
            orchestrator,
            next_seq,
            signer,
            label,
            join_requests,
            metrics,
            ..
        } = self;

        let resp = match req {
            RpcRequest::Submit {
                target,
                payload_hex,
            } => match unhex(&payload_hex) {
                Ok(payload) => {
                    let seq = *next_seq;
                    *next_seq += 1;
                    match node.submit(signer, seq, Msg { target, payload }).await {
                        Ok(_) => RpcReply::ok(),
                        Err(e) => RpcReply::err(format!("submit failed: {e}")),
                    }
                }
                Err(e) => RpcReply::err(format!("bad payload_hex: {e}")),
            },
            RpcRequest::Query { target, req_hex } => match unhex(&req_hex) {
                Ok(req_bytes) => match node.host().query(&target, &req_bytes).await {
                    Ok(bytes) => RpcReply {
                        reply_hex: Some(hex_bytes(&bytes)),
                        ..RpcReply::ok()
                    },
                    Err(e) => RpcReply::err(format!("query failed: {e}")),
                },
                Err(e) => RpcReply::err(format!("bad req_hex: {e}")),
            },
            RpcRequest::Status => {
                let modules = crate::util::module_roots_hex(node.host());
                RpcReply {
                    status: Some(RpcStatus {
                        height: node.finalized().map(|f| f.height),
                        root_hash: hex(&node.root_hash()),
                        modules,
                        netstack: metrics.operational_status().netstack,
                    }),
                    ..RpcReply::ok()
                }
            }
            RpcRequest::JoinRequests => {
                // read-time hygiene: an approved joiner holds
                // STANDING now (resident or already validator) —
                // its request is settled, drop it.
                let members = read_valset_members(node.host()).await.unwrap_or_default();
                let residents_now = read_valset_residents(node.host()).await;
                join_requests.retain(|joiner, _| {
                    !members.contains(joiner) && !residents_now.contains(joiner)
                });
                let views = join_requests
                    .iter()
                    .map(|(joiner, r)| JoinRequestView {
                        joiner: hex_bytes(joiner),
                        issuer: hex_bytes(&r.issuer),
                        first_seen_ms: r.first_seen_ms,
                        last_seen_ms: r.last_seen_ms,
                    })
                    .collect();
                RpcReply {
                    join_requests: Some(views),
                    ..RpcReply::ok()
                }
            }
            RpcRequest::JoinState => {
                // a validator is a full member — the terminal join state. the
                // node-owned source: no log-marker parsing.
                RpcReply {
                    join_state: Some(JoinStateView {
                        phase: "promoted".into(),
                        detail: "validator".into(),
                        height: node.finalized().map(|f| f.height),
                    }),
                    ..RpcReply::ok()
                }
            }
            RpcRequest::Peers => RpcReply {
                peers: Some(self.peers_sample().await),
                ..RpcReply::ok()
            },
            RpcRequest::Shutdown => {
                // best-effort final checkpoint + journal barrier so
                // the restart replays a minimal suffix; a failure
                // here is just the crash path, which also recovers.
                // SAME sequence as the signal arm (shared macro).
                graceful_checkpoint(node, orchestrator, *next_seq).await;
                let _ = reply.send(RpcReply::ok());
                // the send only queues the reply on the rpc thread; exiting
                // here would race its write and close the socket on a caller
                // that never saw a reply line.
                let _ = written.await;
                tracing::info!(
                    target: "ducktape::node",
                    node = %label,
                    "shutdown requested via rpc; exiting"
                );
                std::process::exit(0);
            }
        };
        let _ = reply.send(resp);
    }

    /// the join gate, arriving over the WireGuard-tunnel
    /// doorbell: the reachability plane already OPENED and VERIFIED the sealed
    /// intro (V1–V5 crypto, V4 expiry) and installed the tunnel —
    /// what reaches this loop is a verified request. this runs the
    /// COMMITTED-STATE checks (V6/V7/V9), then — on pass — submits `Redeem`
    /// and HOLDS the joiner's outcome against that frame until the drain
    /// reports its consensus fate (`pending_gates`, resolved in `on_drain`
    /// into `gate_outcomes`, where the doorbell answers the joiner's next
    /// retransmit). the outcome is authoritative: `Admitted` means standing
    /// is COMMITTED, `Rejected{terminal}` means stop.
    pub(super) async fn on_gate_forward(&mut self, fwd: join_gate::GateForward) {
        let Self {
            context,
            node,
            gate_outcomes,
            next_seq,
            signer,
            label,
            validators,
            coordination,
            join_requests,
            gating,
            pending_gates,
            ..
        } = self;

        let joiner_bytes = fwd.joiner.clone();
        let issuer_bytes = fwd.issuer.clone();
        // V6: nonce unspent in committed redemptions — a point read by nonce
        // against the exactly-once set. a nonce redeemed by ANOTHER key can
        // never redeem again — terminal Spent. (redeemed by the SAME key =
        // this joiner already has standing; V9 handles it.)
        let redemption = read_redemption_from_host(node.host(), &fwd.nonce).await;
        if let Some(spent) = redemption.filter(|r| r.joiner != joiner_bytes) {
            tracing::warn!(
                target: "ducktape::join",
                node = %label,
                peer = %hex_bytes(&joiner_bytes[..4.min(joiner_bytes.len())]),
                spent_by = %hex_bytes(&spent.joiner[..4.min(spent.joiner.len())]),
                height = spent.height,
                reason = "invite_already_redeemed",
                "gate: peer presented an ALREADY-REDEEMED invite; refusing permanently"
            );
            super::settle_gate(
                gate_outcomes,
                joiner_bytes,
                join_gate::IntroReply::Rejected {
                    code: join_gate::RejectCode::Spent,
                    detail: "invite already redeemed — an invite admits exactly one person; \
                             ask the inviter for a fresh invite"
                        .into(),
                    terminal: true,
                },
                context.current(),
            );
            return;
        }
        let members = read_valset_members(node.host()).await.unwrap_or_default();
        let residents_now = read_valset_residents(node.host()).await;
        // V7: issuer in committed valset. NON-TERMINAL — this member's local
        // view cannot distinguish a REMOVED issuer (invite dead) from a
        // just-admitted one it has not applied yet; a terminal answer here
        // would let one lagging validator kill a healthy join. the joiner
        // fails over to another member (V7, PR #538 ruling).
        if !members.contains(&issuer_bytes) {
            super::settle_gate(
                gate_outcomes,
                joiner_bytes,
                join_gate::IntroReply::Rejected {
                    code: join_gate::RejectCode::IssuerUnknown,
                    detail: "the inviting member is not in this member's current view — if it \
                             was removed, this invite is dead (ask a current member for a fresh \
                             one); if it was just admitted, another member will redeem shortly"
                        .into(),
                    terminal: false,
                },
                context.current(),
            );
            return;
        }
        // V9: already holding standing (validator OR resident) → idempotent
        // SUCCESS. a re-gated joiner is not an error — answer Admitted at the
        // current committed height.
        // the ack carries the cap on BOTH admit paths: a multi-candidate race
        // routinely lands the joiner's winning ack on THIS arm (a sibling
        // member's Redeem already applied), and the sealed `Admitted` is the
        // cap's only delivery channel — the joiner deletes its invite token on
        // the first Admitted it accepts, so a cap-less one is unrecoverable.
        if members.contains(&joiner_bytes) || residents_now.contains(&joiner_bytes) {
            let height = node.finalized().map(|f| f.height).unwrap_or(0);
            let cap = mint_joiner_cap(coordination, validators, signer, &joiner_bytes);
            super::settle_gate(
                gate_outcomes,
                joiner_bytes,
                join_gate::IntroReply::Admitted { height, cap },
                context.current(),
            );
            return;
        }

        // A verified retransmit refreshes the request even while its first
        // redemption is still waiting for consensus.
        crate::rpc::insert_join_request(
            join_requests,
            joiner_bytes.clone(),
            issuer_bytes.clone(),
            unix_ms(),
        );

        // V6/V7/V9 pass. ONE in-flight gate per joiner key: a duplicate
        // forward while settling (the joiner's retransmit cadence outpacing
        // consensus) re-arms nothing — no double-submit (the nonce set would
        // collapse racing submits to one grant anyway, but a second
        // pending_gate could mis-map the committed grant's sibling reject onto
        // this joiner, so dedup is load-bearing).
        if gating.contains_key(&joiner_bytes) {
            return;
        }

        let minted_cap = mint_joiner_cap(coordination, validators, signer, &joiner_bytes);

        // SETTLE-THEN-ANSWER: submit the Redeem and hold the joiner's
        // outcome against the frame id. `submit` returns the FrameId; the drain
        // reports its consensus fate on `pending_gates` (Applied → Admitted,
        // Rejected → mapped code, timeout → Busy) — this handler never blocks.
        let join_gate::GateForward {
            issuer,
            nonce,
            token_sig,
            joiner,
            proof,
            expires_unix_secs,
        } = fwd;
        let redeem = governance::GovMsg::Redeem {
            issuer,
            nonce,
            token_sig,
            joiner,
            proof,
            expires_unix_secs,
        };
        let seq = *next_seq;
        *next_seq += 1;
        match node
            .submit(
                signer,
                seq,
                Msg {
                    target: "governance".into(),
                    payload: governance::encode_msg(&redeem),
                },
            )
            .await
        {
            Ok(frame_id) => {
                tracing::info!(
                    target: "ducktape::join",
                    node = %label,
                    peer = %hex_bytes(&joiner_bytes[..4.min(joiner_bytes.len())]),
                    issuer = %hex_bytes(&issuer_bytes[..4.min(issuer_bytes.len())]),
                    frame = %hex_bytes(&frame_id),
                    "gate: redemption submitted for {}; awaiting consensus before answering \
                     Admitted",
                    hex_bytes(&joiner_bytes[..4.min(joiner_bytes.len())])
                );
                gating.insert(joiner_bytes.clone(), frame_id);
                pending_gates.insert(
                    frame_id,
                    super::GatePending {
                        joiner: joiner_bytes,
                        cap: minted_cap,
                        deadline: context.current() + GATE_SETTLE_TIMEOUT,
                    },
                );
            }
            Err(e) => {
                // submit failure is transient: the joiner tries another
                // member rather than exiting.
                super::settle_gate(
                    gate_outcomes,
                    joiner_bytes,
                    join_gate::IntroReply::Rejected {
                        code: join_gate::RejectCode::Busy,
                        detail: format!("could not submit redemption: {e}"),
                        terminal: false,
                    },
                    context.current(),
                );
            }
        }
    }

    pub(super) async fn on_relay(&mut self, peer: ed25519::PublicKey, bytes: Vec<u8>) {
        let Ok(msg) = relay::decode_msg(&bytes) else {
            return;
        };
        // the leader nudge is not part of the relay submit protocol — it
        // carries no frame, needs no standing read, and touches no relay
        // state — so it dispatches BEFORE the protocol machine.
        if matches!(msg, relay::RelayMsg::Nudge) {
            self.on_leader_nudge(&peer).await;
            return;
        }
        let Self {
            context,
            node,
            relay_tx,
            pending_submits,
            pending_relays,
            validator_relay,
            ..
        } = self;
        let now = context.current();

        // both intake doors read standing (members ∪ residents): a blob offer
        // to bound the pack fanout, and a submit to bound consensus custody —
        // the RELAYING peer must be a node this network committed to, even
        // though the frame it carries may be signed by any key at all.
        let needs_node_standing = matches!(
            msg,
            relay::RelayMsg::BlobOffer { .. } | relay::RelayMsg::Submit { .. }
        );
        let (members_now, residents_now) = if needs_node_standing {
            (
                read_valset_members(node.host()).await.unwrap_or_default(),
                read_valset_residents(node.host()).await,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let Some(action) =
            validator_relay.on_message(now, peer, msg, &members_now, &residents_now, relay_tx)
        else {
            return;
        };
        match action {
            relay_runtime::ValidatorAction::SubmitResident {
                frame_id,
                frame,
                peer,
            } => match node.submit_frame(frame).await {
                Ok(id) => {
                    debug_assert_eq!(id, frame_id);
                    // APPEND: the same frame relayed twice is one consensus
                    // unit, and both couriers are owed the same answer.
                    pending_relays
                        .entry(id)
                        .or_insert_with(|| (Vec::new(), now + SUBMIT_HOLD))
                        .0
                        .push(peer);
                }
                Err(e) => relay_runtime::send_reply(
                    relay_tx,
                    &peer,
                    frame_id,
                    relay::RelayOutcome::Refused {
                        detail: format!("submit failed: {e}"),
                    },
                ),
            },
            relay_runtime::ValidatorAction::SubmitLocal {
                frame_id,
                frame,
                reply,
                deadline,
            } => match node.submit_frame(frame).await {
                Ok(id) => {
                    debug_assert_eq!(id, frame_id);
                    // APPEND: the same frame submitted twice is one consensus
                    // unit, and every caller holding its id is owed the answer.
                    pending_submits
                        .entry(id)
                        .or_insert_with(|| (Vec::new(), deadline))
                        .0
                        .push(reply);
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("submit failed: {e}")));
                }
            },
        }
    }

    /// take custody of an already-framed op on THIS validator: fan a required blob
    /// out to the peers that need it, then pin + propose the frame and hold the
    /// caller's reply against the frame id until the drain answers it.
    ///
    /// the frame arrives SIGNED and is submitted verbatim — nothing here looks
    /// at, replaces, or re-derives its origin. that is what makes it usable by
    /// both callers: the frameless lane, whose frame this node just signed with
    /// its own key, and the signed-frame lane, whose frame some OTHER key signed
    /// (an agent's per-run session key). a re-sign here would silently convert
    /// the second into the first — the exact defect the frame lane closes.
    async fn submit_local_frame(
        &mut self,
        frame: Vec<u8>,
        reply: futures::channel::oneshot::Sender<Result<noded::BlockSummary, String>>,
    ) {
        let Self {
            context,
            node,
            relay_tx,
            signer,
            pending_submits,
            validator_relay,
            ..
        } = self;
        let now = context.current();

        let peers: Vec<ed25519::PublicKey> = if relay::required_blob_digest(&frame).is_some() {
            read_valset_members(node.host())
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|raw| ed25519::PublicKey::decode(raw.as_slice()).ok())
                .filter(|key| key != &signer.public_key())
                .collect()
        } else {
            Vec::new()
        };
        match validator_relay.prepare_local(now, frame, reply, peers, relay_tx) {
            Ok(Some(relay_runtime::ValidatorAction::SubmitLocal {
                frame_id,
                frame,
                reply,
                deadline,
            })) => match node.submit_frame(frame).await {
                Ok(id) => {
                    debug_assert_eq!(id, frame_id);
                    // APPEND: the same frame submitted twice is one consensus
                    // unit, and every caller holding its id is owed the answer.
                    pending_submits
                        .entry(id)
                        .or_insert_with(|| (Vec::new(), deadline))
                        .0
                        .push(reply);
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("submit failed: {e}")));
                }
            },
            Ok(Some(relay_runtime::ValidatorAction::SubmitResident { .. })) => {
                unreachable!("local preparation returns a local action")
            }
            Ok(None) => {}
            Err((reply, detail)) => {
                let _ = reply.send(Err(detail));
            }
        }
    }

    pub(super) async fn on_http(&mut self, cmd: noded::NodeCommand) {
        match cmd {
            // `origin` is the caller's CLAIMED submitter identity —
            // meaningful on the embedded daemon, but this lane signs
            // frames, and the signed origin IS this node's pubkey
            // (authenticated authorship that governance relies on).
            // a claimed origin cannot ride a signed frame without
            // making authorship forgeable, so it is ignored here;
            // display names resolve via the name registry instead. a
            // caller that needs to be its OWN author signs a frame and
            // takes the SubmitFrame arm below.
            noded::NodeCommand::Submit {
                target,
                payload,
                required_blob,
                origin: _,
                reply,
            } => {
                let seq = self.next_seq;
                self.next_seq += 1;
                let frame = node::encode_frame_with_blob(
                    &self.signer,
                    seq,
                    &Msg { target, payload },
                    required_blob,
                );
                self.submit_local_frame(frame, reply).await;
            }
            // an ALREADY-SIGNED frame: submitted VERBATIM, never re-signed and
            // never re-originated. `OrderedNode::submit_frame` verifies the
            // signature before anything is pinned, so a forged or tampered frame
            // is a rejection here, not junk in the store — and the origin the
            // block carries is the frame's own proven signer, which is the only
            // authorship a module can trust.
            noded::NodeCommand::SubmitFrame { frame, reply } => {
                self.submit_local_frame(frame, reply).await;
            }
            noded::NodeCommand::Query { target, req, reply } => {
                let result = self
                    .node
                    .host()
                    .query(&target, &req)
                    .await
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            noded::NodeCommand::QueryAs {
                target,
                req,
                reader,
                reply,
            } => {
                let result = self
                    .node
                    .host()
                    .query_as(&target, &req, sdk::Origin::External(reader))
                    .await
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
    }
}

/// MINT the coordinator capability for a joiner (private coordination only,
/// and only a GENESIS validator's cap is trusted by the coordinator).
/// Additive and side-effect-free — a pure ed25519 sign. The cap cannot ride
/// the invite (the joiner's key did not exist at invite-mint time), so the
/// sealed `Admitted` ack is its only delivery channel; EVERY arm that answers
/// Admitted mints through here.
fn mint_joiner_cap(
    coordination: &config::Coordination,
    validators: &[ed25519::PublicKey],
    signer: &ed25519::PrivateKey,
    joiner: &[u8],
) -> Option<Vec<u8>> {
    let private_coordination = *coordination == config::Coordination::Private;
    let signer_is_genesis_validator = validators.contains(&signer.public_key());
    if !private_coordination || !signer_is_genesis_validator {
        return None;
    }
    // `verify_intro` decoded this key upstream — a non-32-byte joiner cannot
    // reach the loop; mint nothing rather than panic.
    let subj = <[u8; 32]>::try_from(joiner).ok()?;
    let cap = nat_traversal::mint_coord_cap(
        signer,
        nat_traversal::NodeKey(subj),
        nat_traversal::now_secs() + nat_traversal::COORD_CAP_TTL_SECS,
    );
    Some(config::pack_coord_cap(&cap))
}

/// Commonware owns the detailed peer series. This bounded adapter counts only
/// current validators and includes self, insulating the stable Ducktape facade
/// from dashboard knowledge of dependency-specific metric names.
fn reachable_validators(
    exposition: &str,
    validators: &std::collections::BTreeSet<ed25519::PublicKey>,
    me: &ed25519::PublicKey,
) -> u64 {
    // ponytail: O(validators × exposition lines); replace with a connection
    // snapshot API if validator sets become large enough for scrape cost to matter.
    validators
        .iter()
        .filter(|validator| {
            if *validator == me {
                return true;
            }
            let prefix = format!(
                "network_tracker_directory_connected{{peer=\"{}\"}} ",
                validator
            );
            exposition.lines().any(|line| {
                line.strip_prefix(&prefix)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some_and(|value| value > 0.0)
            })
        })
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_reachable_count_includes_self_and_connected_members_only() {
        let me = ed25519::PrivateKey::from_seed(1).public_key();
        let connected = ed25519::PrivateKey::from_seed(2).public_key();
        let disconnected = ed25519::PrivateKey::from_seed(3).public_key();
        let outsider = ed25519::PrivateKey::from_seed(4).public_key();
        let validators = [me.clone(), connected.clone(), disconnected]
            .into_iter()
            .collect();
        let exposition = format!(
            "network_tracker_directory_connected{{peer=\"{connected}\"}} 1720000000\n\
             network_tracker_directory_connected{{peer=\"{outsider}\"}} 1720000000\n"
        );

        assert_eq!(reachable_validators(&exposition, &validators, &me), 2);
    }

    #[test]
    fn private_coordination_genesis_validator_mints_a_cap_for_the_joiner() {
        let signer = ed25519::PrivateKey::from_seed(1);
        let validators = vec![signer.public_key()];
        let joiner = ed25519::PrivateKey::from_seed(9).public_key();

        let packed = mint_joiner_cap(
            &config::Coordination::Private,
            &validators,
            &signer,
            joiner.as_ref(),
        )
        .expect("a genesis validator on a private network mints a cap");
        let cap = config::unpack_coord_cap(&packed).expect("the packed cap round-trips");
        assert_eq!(cap.issuer, signer.public_key());

        // public coordination needs none, and a non-genesis signer's cap is
        // not trusted by the coordinator — both mint nothing.
        assert!(
            mint_joiner_cap(
                &config::Coordination::Public,
                &validators,
                &signer,
                joiner.as_ref()
            )
            .is_none()
        );
        assert!(
            mint_joiner_cap(
                &config::Coordination::Private,
                &[],
                &signer,
                joiner.as_ref()
            )
            .is_none()
        );
    }

    /// The V9 "joiner already holds standing" arm answers Admitted without a
    /// consensus round, so on a multi-candidate race its ack routinely reaches
    /// the joiner first — and the joiner deletes its invite token on the first
    /// Admitted it accepts. A hardcoded empty cap in this file means some
    /// Admitted path lost the coordinator capability for good.
    #[test]
    fn no_admitted_path_settles_without_a_minted_cap() {
        // built at runtime so this needle does not match its own source line.
        let hardcoded_empty_cap = format!("cap: {}", "None");
        assert!(
            !include_str!("ingress.rs").contains(&hardcoded_empty_cap),
            "every Admitted arm must mint through mint_joiner_cap"
        );
    }

    /// An eligible forward refreshes the request before deduplicating its
    /// in-flight redemption. A retransmit of a joiner already tracked
    /// must move `last_seen_ms` forward without touching `first_seen_ms` or
    /// growing the map, or `ducktape node join requests` shows a last_seen
    /// frozen at first contact for a joiner that has been retrying for 20 min.
    #[test]
    fn join_request_retransmit_moves_last_seen_forward() {
        let mut requests = std::collections::BTreeMap::new();
        let joiner = vec![7u8; 32];
        let issuer = vec![9u8; 32];

        crate::rpc::insert_join_request(&mut requests, joiner.clone(), issuer.clone(), 1_000);
        crate::rpc::insert_join_request(&mut requests, joiner.clone(), issuer, 5_000);

        assert_eq!(requests.len(), 1, "a retransmit never grows the map");
        let record = requests.get(&joiner).expect("the joiner is tracked");
        assert_eq!(
            record.first_seen_ms, 1_000,
            "first_seen_ms is set once, on the FIRST forward"
        );
        assert_eq!(
            record.last_seen_ms, 5_000,
            "a retransmit MUST move last_seen_ms forward"
        );
    }

    #[test]
    fn join_request_expiry_uses_the_last_verified_retransmit() {
        let mut requests = std::collections::BTreeMap::new();
        let window = reachability::INVITE_JOIN_WINDOW_MS;
        let expired = vec![1];
        let retrying = vec![2];
        crate::rpc::insert_join_request(&mut requests, expired.clone(), vec![9], 0);
        crate::rpc::insert_join_request(&mut requests, retrying.clone(), vec![9], 0);
        crate::rpc::insert_join_request(&mut requests, retrying.clone(), vec![9], window);

        crate::rpc::sweep_join_requests(&mut requests, window + 1, window);
        assert!(!requests.contains_key(&expired));
        assert_eq!(requests[&retrying].first_seen_ms, 0);
        crate::rpc::sweep_join_requests(&mut requests, window * 2, window);
        assert!(requests.contains_key(&retrying));
        crate::rpc::sweep_join_requests(&mut requests, window * 2 + 1, window);
        assert!(requests.is_empty());
    }

    /// The join-request map is keyed on the attacker-chosen joiner key with
    /// no other size limit — it must cap at
    /// [`crate::reachability_plane::MAX_TRACKED_JOINERS`], evicting the
    /// OLDEST (smallest `last_seen_ms`) entry to make room for a new joiner
    /// past the cap, mirroring [`crate::reachability_plane::insert_gate_outcome`].
    #[test]
    fn the_4097th_joiner_evicts_the_oldest() {
        let cap = crate::reachability_plane::MAX_TRACKED_JOINERS;
        let mut requests = std::collections::BTreeMap::new();
        for i in 0..cap {
            crate::rpc::insert_join_request(
                &mut requests,
                (i as u32).to_be_bytes().to_vec(),
                vec![0],
                i as u64,
            );
        }
        assert_eq!(requests.len(), cap);
        let oldest = 0u32.to_be_bytes().to_vec();
        assert!(requests.contains_key(&oldest));

        let newcomer = (cap as u32).to_be_bytes().to_vec();
        crate::rpc::insert_join_request(&mut requests, newcomer.clone(), vec![0], cap as u64);

        assert_eq!(
            requests.len(),
            cap,
            "the map stays capped at MAX_TRACKED_JOINERS"
        );
        assert!(
            !requests.contains_key(&oldest),
            "the oldest entry must be evicted to make room"
        );
        assert!(
            requests.contains_key(&newcomer),
            "the new joiner past the cap must be tracked"
        );
    }
}
