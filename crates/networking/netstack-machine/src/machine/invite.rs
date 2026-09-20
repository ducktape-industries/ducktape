//! The join-window invite layer: node-authenticated tunnel peers installed
//! live and epoch-independent — the weakest layer, dissolved the moment an
//! epoch plan or a standby record covers the same identity — and the
//! coordinated-invite bootstrap that rendezvouses the inviter, installs it,
//! and rides the same punched socket for the authenticated intro.

use std::net::SocketAddr;

use commonware_cryptography::ed25519;
use wireguard::X25519PublicKey;
use wireguard::effect::PeerTunnelConfig;

use crate::binding;
use crate::contract::{CmdToken, Effect, ReachabilityEvent, ReqId};
use crate::epoch::EpochState;

use super::pending::{PendingOp, WgCont};
use super::{Driver, INTRO_ACK_TIMEOUT_MS, InvitePeer, KEEPALIVE_SECONDS};

impl Driver {
    /// Install a join-window tunnel peer (node-authenticated; see the
    /// command doc) and re-apply the interface — the invite layer's own
    /// `sync_prewarm` analogue, usable BEFORE any epoch exists. The apply
    /// outcome IS the reply (the inviter acks the intro only after the peer
    /// is really on the interface).
    pub(crate) fn install_invite_peer(
        &mut self,
        epoch: Option<&EpochState>,
        token: CmdToken,
        peer: ed25519::PublicKey,
        wireguard_public_key: X25519PublicKey,
        endpoint: SocketAddr,
    ) {
        let cont = WgCont::InviteInstall {
            token,
            peer: peer.clone(),
        };
        if let Err(reason) =
            self.begin_invite_tunnel(&peer, wireguard_public_key, endpoint, epoch, cont)
        {
            // on failure the interface keeps whatever configuration it had;
            // the caller decides whether to retry.
            self.effects.push(Effect::ReplyInstall {
                token,
                outcome: Err(reason),
            });
        }
    }

    /// Merge one invite peer onto the interface and start its push. The
    /// error is the caller's reply text: a tunnel to this node itself (the
    /// push's own refusal answers through the parked continuation).
    fn begin_invite_tunnel(
        &mut self,
        peer: &ed25519::PublicKey,
        wireguard_public_key: X25519PublicKey,
        endpoint: SocketAddr,
        epoch: Option<&EpochState>,
        cont: WgCont,
    ) -> Result<(), String> {
        let identity = binding::identity_of(peer);
        if identity == self.me {
            return Err("refusing an invite tunnel to self".into());
        }
        // the stronger layers decide what "covered" means before anything
        // ages out: a promoted NAT'd member's entry is the only endpoint its
        // endpoint-less record ever gets (see `merge_invite_layer`), so age
        // alone must never prune it — and it never spends a join-window slot.
        let merged = match epoch {
            Some(state) => {
                Self::epoch_layered_peers(state, self.base_peers.clone().unwrap_or_default())
            }
            None => self.base_peers.clone().unwrap_or_default(),
        };
        // the UNCOVERED entries age out on the join window; a re-intro
        // refreshes its own entry.
        let now_ms = self.now_ms;
        self.invite_peers
            .retain(|id, invite| merged.contains_key(id) || !invite.expired_at(now_ms));
        let allowed_ips = self.overlay.identity_allowed_ips(identity);
        let config = PeerTunnelConfig {
            wireguard_public_key,
            // the intro datagram's observed source — always concrete.
            endpoint: Some(endpoint),
            allowed_ips,
            keepalive_seconds: Some(KEEPALIVE_SECONDS),
        };
        // a byte-identical re-intro (same key, same observed endpoint, same
        // allowed-ips) changes nothing on the interface: refresh the
        // join-window clock and settle the reply without paying for another
        // synchronous WgApply — that push is what stalls the whole command
        // loop (record gossip, ViewTick, Nudge, every other intro) for its
        // duration, so a replay of the same intro must be free.
        let unchanged = self
            .invite_peers
            .get(&identity)
            .is_some_and(|existing| existing.config == config);
        if unchanged {
            self.invite_peers.insert(
                identity,
                InvitePeer {
                    config,
                    installed_at_ms: now_ms,
                },
            );
            match cont {
                WgCont::InviteInstall { token, peer } => {
                    self.finish_invite_install(token, peer, Ok(()))
                }
                WgCont::InviteBootstrap {
                    token,
                    peer,
                    endpoint,
                    intro,
                } => self.finish_invite_bootstrap(token, peer, endpoint, intro, Ok(())),
                _ => unreachable!("begin_invite_tunnel is only ever driven by an invite command"),
            }
            return Ok(());
        }
        self.invite_peers.insert(
            identity,
            InvitePeer {
                config,
                installed_at_ms: now_ms,
            },
        );
        let peers = self.assemble_peers(merged);
        self.start_wg_push(peers, cont);
        Ok(())
    }

    /// The invite install's push settled: the outcome is the reply, and a
    /// landed install is observed.
    pub(crate) fn finish_invite_install(
        &mut self,
        token: CmdToken,
        peer: ed25519::PublicKey,
        outcome: Result<(), String>,
    ) {
        match outcome {
            Ok(()) => {
                self.effects.push(Effect::ReplyInstall {
                    token,
                    outcome: Ok(()),
                });
                self.emit(ReachabilityEvent::InvitePeerInstalled {
                    peer,
                    interface: self.interface.clone(),
                });
            }
            Err(err) => {
                self.effects.push(Effect::ReplyInstall {
                    token,
                    outcome: Err(err),
                });
            }
        }
    }

    /// Coordinated invite bootstrap: rendezvous the inviter's WireGuard
    /// underlay endpoint, install it as the local join-window peer, and send
    /// the authenticated intro over that same punched socket so the inviter
    /// can install this node in return.
    pub(crate) fn bootstrap_coordinated_invite_peer(
        &mut self,
        token: CmdToken,
        peer: ed25519::PublicKey,
        wireguard_public_key: X25519PublicKey,
        intro: Vec<u8>,
    ) {
        let identity = binding::identity_of(&peer);
        let req = self.mint_req();
        self.effects.push(Effect::RendezvousStart {
            req,
            peer: binding::node_key(identity),
        });
        self.pending.insert(
            req,
            PendingOp::InviteRendezvous {
                token,
                peer,
                wireguard_public_key,
                intro,
            },
        );
    }

    /// The bootstrap's rendezvous came back: install the inviter, or fail
    /// the command.
    pub(crate) fn invite_rendezvous_resolved(
        &mut self,
        epoch: Option<&EpochState>,
        token: CmdToken,
        peer: ed25519::PublicKey,
        wireguard_public_key: X25519PublicKey,
        intro: Vec<u8>,
        outcome: Result<SocketAddr, String>,
    ) {
        let endpoint = match outcome {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                self.effects.push(Effect::ReplyIntro {
                    token,
                    outcome: Err(format!("coordinated invite endpoint resolution: {reason}")),
                });
                return;
            }
        };
        let cont = WgCont::InviteBootstrap {
            token,
            peer: peer.clone(),
            endpoint,
            intro,
        };
        if let Err(reason) =
            self.begin_invite_tunnel(&peer, wireguard_public_key, endpoint, epoch, cont)
        {
            self.effects.push(Effect::ReplyIntro {
                token,
                outcome: Err(reason),
            });
        }
    }

    /// The bootstrap's install settled: a landed install proceeds to the
    /// awaited intro datagram; a refusal is the command's reply.
    pub(crate) fn finish_invite_bootstrap(
        &mut self,
        token: CmdToken,
        peer: ed25519::PublicKey,
        endpoint: SocketAddr,
        intro: Vec<u8>,
        outcome: Result<(), String>,
    ) {
        if let Err(err) = outcome {
            self.effects.push(Effect::ReplyIntro {
                token,
                outcome: Err(err),
            });
            return;
        }
        self.emit(ReachabilityEvent::InvitePeerInstalled {
            peer,
            interface: self.interface.clone(),
        });
        let req = self.mint_req();
        self.effects.push(Effect::UdpSendAwait {
            req,
            endpoint,
            bytes: intro,
            timeout_ms: INTRO_ACK_TIMEOUT_MS,
        });
        self.pending.insert(req, PendingOp::IntroAck { token });
    }

    /// The awaited intro datagram settled — success or timeout, it answers
    /// the bootstrap command.
    pub(crate) fn on_datagram_replied(&mut self, req: ReqId, outcome: Result<Vec<u8>, String>) {
        let Some(op) = self.pending.remove(&req) else {
            tracing::debug!(
                target: "ducktape::reachability",
                req = req.0,
                "datagram reply dropped: its operation was superseded"
            );
            return;
        };
        let PendingOp::IntroAck { token } = op else {
            debug_assert!(false, "a datagram reply answered a non-datagram operation");
            return;
        };
        self.effects.push(Effect::ReplyIntro {
            token,
            outcome: outcome.map_err(|reason| format!("coordinated invite intro ack: {reason}")),
        });
    }
}
