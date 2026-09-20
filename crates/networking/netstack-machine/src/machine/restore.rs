//! Retargeting and the cold-restart restore: bind a fresh epoch on every
//! cutover, and — once per host life, on the boot retarget — bring the LAST
//! applied epoch's tunnels back from the persisted mesh so plane gossip has
//! a path on a node that restarted with zero TCP links (NATed member whose
//! join ingress is gone; whole-network cold start).
//!
//! The restore is the machine's one PRE-EPOCH suspension: the remembered
//! records' endpoints re-resolve FRESH through the coordinator (a persisted
//! punch observation died with the downtime's NAT mappings), those resolves
//! join into one interface push, and only then does the retarget tail build
//! the epoch. Events delivered in that window find no epoch and drop — the
//! same fate they had while the old blocking restore held the command
//! queue — and the nudge re-offers heal whatever that window cost.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use commonware_cryptography::ed25519;
use wireguard::effect::PeerTunnelConfig;
use wireguard::{EndpointRecord, SignedEndpointRecord, UpgradeError, ValidatorIdentity};

use crate::binding;
use crate::contract::{Effect, MeshEpochEvent, ReachabilityEvent, ReqId, Resolution};
use crate::epoch::{EpochState, Role, epoch_nonce_seed};
use crate::msg::ReachabilityMsg;
use crate::store;

use super::pending::{PendingOp, PendingRestore, RestoreApply, WgCont};
use super::{Driver, KEEPALIVE_SECONDS};

impl Driver {
    /// Boot or epoch cutover: derive the role, run the once-per-life
    /// restore when the host handed persisted bytes, and land the epoch —
    /// now, or from the restore's settlement. `Ok(None)` is a node that is
    /// neither a member nor a standby (stood down), or a retarget suspended
    /// behind its restore.
    ///
    /// `carried` is the superseded epoch's accepted standby records — empty
    /// at boot, where the persisted bytes play that part.
    pub(crate) fn retarget(
        &mut self,
        event: MeshEpochEvent,
        persisted: Option<Vec<u8>>,
        carried: Vec<SignedEndpointRecord>,
    ) -> Result<Option<EpochState>, UpgradeError> {
        self.view = self.view.max(event.current_view);
        let identities: Vec<ValidatorIdentity> =
            event.members.iter().map(binding::identity_of).collect();
        // members win over a stale standby listing; anything in neither set
        // is inert (demotion normally exits the node before this).
        let standby_ids: Vec<ValidatorIdentity> = event
            .standbys
            .iter()
            .map(binding::identity_of)
            .filter(|id| !identities.contains(id))
            .collect();
        let is_member = identities.contains(&self.me);
        let is_standby = standby_ids.contains(&self.me);
        let role = match (is_member, is_standby) {
            (true, _) => Role::Member,
            (false, true) => Role::Standby,
            (false, false) => {
                self.stand_down(event.epoch);
                return Ok(None);
            }
        };
        // the plane's one lifecycle fact per epoch, and the positive half of
        // the `no_epoch_target` warn in `nudge`: an operator reading a node
        // that gossips nothing needs to know whether this line ever printed.
        match role {
            Role::Member => tracing::info!(
                target: "ducktape::reachability",
                epoch = event.epoch,
                members = identities.len(),
                standbys = standby_ids.len(),
                view = event.current_view,
                "plane targeted: member at epoch"
            ),
            Role::Standby => tracing::info!(
                target: "ducktape::reachability",
                epoch = event.epoch,
                members = identities.len(),
                standbys = standby_ids.len(),
                view = event.current_view,
                "plane targeted: standby at epoch"
            ),
        }
        match persisted {
            None => Ok(Some(self.retarget_tail(event, role, carried)?)),
            Some(bytes) => self.begin_restore(event, role, bytes),
        }
    }

    /// Decode, gate, and start resolving the persisted mesh. Strictly
    /// best-effort and strictly a bootstrap: refusals degrade to the
    /// pre-persistence behavior (live assembly only), and the boot epoch's
    /// own assembly replaces the restored interface at its apply.
    ///
    /// Everything re-derives from the persisted records — peer WireGuard
    /// keys and advertised endpoints from the records themselves, overlay
    /// addresses from `(chain_id, identity)` — except endpoints behind NAT,
    /// which are re-resolved FRESH through the coordinator (re-resolution
    /// needs no gossip). One-sided resolution suffices: WireGuard roams a
    /// peer's endpoint on any authenticated inbound packet, so whichever
    /// side resolves a working path first heals the pair.
    fn begin_restore(
        &mut self,
        event: MeshEpochEvent,
        role: Role,
        bytes: Vec<u8>,
    ) -> Result<Option<EpochState>, UpgradeError> {
        let mesh = match store::decode_verified(&bytes, &self.config.chain_id) {
            Ok(mesh) => mesh,
            Err(err) => {
                self.emit(ReachabilityEvent::RestoreFailed {
                    reason: err.to_string(),
                });
                return Ok(Some(self.retarget_tail(event, role, Vec::new())?));
            }
        };
        // the BOOT epoch's members gate the restore: a departed member's
        // tunnel is dead weight, an arrival has no persisted record (its
        // tunnel assembles live). Signatures were verified by the decode.
        let member_pk_of: BTreeMap<ValidatorIdentity, ed25519::PublicKey> = event
            .members
            .iter()
            .map(|pk| (binding::identity_of(pk), pk.clone()))
            .collect();
        // per member the higher nonce wins: a live re-advertisement accepted
        // after the persisting epoch's lock is the member's current life,
        // and its tunnel parts are the ones worth restoring.
        let mut selected: BTreeMap<ValidatorIdentity, EndpointRecord> = BTreeMap::new();
        let remembered = mesh
            .adverts
            .iter()
            .map(|advert| &advert.record)
            .chain(mesh.member_records.iter().map(|signed| &signed.record));
        for record in remembered {
            let in_boot_set = record.validator_identity != self.me
                && member_pk_of.contains_key(&record.validator_identity);
            if !in_boot_set {
                continue;
            }
            match selected.get(&record.validator_identity) {
                Some(prev) if record.nonce <= prev.nonce => {}
                _ => {
                    selected.insert(record.validator_identity, record.clone());
                }
            }
        }
        let records: Vec<EndpointRecord> = selected.into_values().collect();
        // the boot epoch's RESIDENT set gates the persisted standby records
        // exactly as its member set gates the adverts: a departed standby's
        // tunnel is dead weight. One still parked is why these persist at
        // all — it cannot re-introduce itself to a member that forgot its
        // WireGuard key (invite token consumed at admission, every remaining
        // transport rides this overlay), so only this reinstall lets its
        // ongoing handshake retries land again after a reboot.
        let standby_ids: BTreeSet<ValidatorIdentity> = event
            .standbys
            .iter()
            .map(binding::identity_of)
            .filter(|id| *id != self.me && !member_pk_of.contains_key(id))
            .collect();
        let standby_records: Vec<SignedEndpointRecord> = mesh
            .standby_records
            .iter()
            .filter(|signed| standby_ids.contains(&signed.record.validator_identity))
            .cloned()
            .collect();
        // the restored records' control endpoints feed the mesh address
        // book exactly like live acceptances — a cold restart's book starts
        // from the same signed evidence the tunnels do.
        for record in &records {
            self.observe_control_endpoint(record.validator_identity, record.control_endpoint);
        }
        for signed in &standby_records {
            self.observe_control_endpoint(
                signed.record.validator_identity,
                signed.record.control_endpoint,
            );
        }
        if records.is_empty() && standby_records.is_empty() {
            return Ok(Some(self.retarget_tail(event, role, Vec::new())?));
        }
        // an endpoint-less record installs without an endpoint (nothing to
        // resolve): that peer initiates and WireGuard roams to it. Records
        // WITH an advertised endpoint resolve through the host resolver;
        // those resolves join here.
        let mut endpoints: BTreeMap<ValidatorIdentity, Option<SocketAddr>> = BTreeMap::new();
        let mut outstanding: BTreeSet<ReqId> = BTreeSet::new();
        for record in &records {
            let owner = record.validator_identity;
            match record.wireguard_endpoint.map(|e| e.socket_addr()) {
                None => {
                    endpoints.insert(owner, None);
                }
                Some(advertised) => {
                    let req = self.mint_req();
                    self.pending
                        .insert(req, PendingOp::RestoreEndpoint { owner, advertised });
                    outstanding.insert(req);
                    self.effects.push(Effect::ResolveStart {
                        req,
                        peer: binding::node_key(owner),
                        advertised,
                    });
                }
            }
        }
        let resolves_needed = !outstanding.is_empty();
        self.pending_restore = Some(PendingRestore {
            event,
            role,
            mesh_epoch: mesh.epoch,
            records,
            standby_records,
            member_pk_of,
            endpoints,
            outstanding,
        });
        if !resolves_needed {
            self.finish_restore_resolves();
        }
        Ok(None)
    }

    /// One remembered record's endpoint resolved. Same contract as live
    /// assembly: the peer rides its advertised endpoint on a resolver
    /// failure and the failure is surfaced.
    pub(crate) fn restore_endpoint_resolved(
        &mut self,
        req: ReqId,
        owner: ValidatorIdentity,
        advertised: SocketAddr,
        outcome: Result<Resolution, String>,
    ) {
        let endpoint = match outcome {
            Ok(Resolution::Advertised) => advertised,
            Ok(Resolution::Punched(addr)) => addr,
            Err(reason) => {
                let pk = self
                    .pending_restore
                    .as_ref()
                    .and_then(|restore| restore.member_pk_of.get(&owner).cloned());
                if let Some(peer) = pk {
                    self.emit(ReachabilityEvent::PeerFailed {
                        peer,
                        reason: format!("restore endpoint resolution: {reason}"),
                    });
                }
                advertised
            }
        };
        let Some(restore) = self.pending_restore.as_mut() else {
            return;
        };
        restore.endpoints.insert(owner, Some(endpoint));
        restore.outstanding.remove(&req);
        if restore.outstanding.is_empty() {
            self.finish_restore_resolves();
        }
    }

    /// Every remembered endpoint settled: build the restored peer set and
    /// push it. The join-window invite layer rides the restore apply too (a
    /// node rebooting mid-window keeps its invite tunnel), but never enters
    /// the restored BASE — the base is the persisted mesh only. And the
    /// invite bootstrap may have brought the interface up before the first
    /// epoch event (a NATed member re-running first contact at boot) — the
    /// push reconfigures it rather than re-creating it, so the restore
    /// neither dies on `AlreadyCreated` nor drops the live join tunnel.
    fn finish_restore_resolves(&mut self) {
        let Some(restore) = self.pending_restore.take() else {
            return;
        };
        let mut peers: BTreeMap<ValidatorIdentity, PeerTunnelConfig> = BTreeMap::new();
        for record in &restore.records {
            let owner = record.validator_identity;
            let endpoint = restore.endpoints.get(&owner).copied().flatten();
            peers.insert(
                owner,
                PeerTunnelConfig {
                    wireguard_public_key: record.wireguard_public_key,
                    endpoint,
                    allowed_ips: self.overlay.identity_allowed_ips(owner),
                    keepalive_seconds: Some(KEEPALIVE_SECONDS),
                },
            );
        }
        for signed in &restore.standby_records {
            peers.insert(
                signed.record.validator_identity,
                self.standby_peer_config(&signed.record),
            );
        }
        let peer_count = peers.len();
        let parts = self.assemble_peers(peers.clone());
        self.start_wg_push(
            parts,
            WgCont::Restore(RestoreApply {
                event: restore.event,
                role: restore.role,
                base: peers,
                standby_records: restore.standby_records,
                mesh_epoch: restore.mesh_epoch,
                peer_count,
            }),
        );
    }

    /// The restore's push settled: a landed push installs the restored mesh
    /// — standby entries included — as the interface's base (the pre-warm
    /// layer merges its live record-derived peers over it; same identity:
    /// fresher wins) and hands the standby records to the epoch tail. A
    /// refusal degrades to live assembly.
    pub(crate) fn finish_restore_apply(
        &mut self,
        apply: RestoreApply,
        outcome: Result<(), String>,
    ) -> (MeshEpochEvent, Role, Vec<SignedEndpointRecord>) {
        match outcome {
            Ok(()) => {
                self.base_peers = Some(apply.base);
                self.emit(ReachabilityEvent::MeshRestored {
                    epoch: apply.mesh_epoch,
                    interface: self.interface.clone(),
                    peers: apply.peer_count,
                });
                (apply.event, apply.role, apply.standby_records)
            }
            Err(err) => {
                self.emit(ReachabilityEvent::RestoreFailed {
                    reason: format!("wireguard effect: {err}"),
                });
                (apply.event, apply.role, Vec::new())
            }
        }
    }

    /// The retarget's epoch birth: bind the epoch, seed the restored
    /// standby records, fan out our record, and take every step the fresh
    /// state already satisfies (a single-member network is a complete
    /// mesh).
    pub(crate) fn retarget_tail(
        &mut self,
        event: MeshEpochEvent,
        role: Role,
        restored_standbys: Vec<SignedEndpointRecord>,
    ) -> Result<EpochState, UpgradeError> {
        let identities: Vec<ValidatorIdentity> =
            event.members.iter().map(binding::identity_of).collect();
        let standby_ids: Vec<ValidatorIdentity> = event
            .standbys
            .iter()
            .map(binding::identity_of)
            .filter(|id| !identities.contains(id))
            .collect();
        let set = binding::active_set(&self.config.chain_id, event.epoch, identities.clone())?;
        let pk_of: BTreeMap<ValidatorIdentity, ed25519::PublicKey> = event
            .members
            .iter()
            .chain(event.standbys.iter())
            .map(|pk| (binding::identity_of(pk), pk.clone()))
            .collect();
        // a member's gossip/handshake counterparties exclude itself; a
        // standby's are exactly the members.
        let peers: Vec<ValidatorIdentity> = identities
            .iter()
            .copied()
            .filter(|id| *id != self.me)
            .collect();
        // the epoch's first signed nonce — wall-clock-seeded so a reboot's
        // re-signed record strictly supersedes its previous life's (see
        // `epoch_nonce_seed`); the epoch's counter starts past it.
        let own = wireguard::SignedEndpointRecord::sign(
            EndpointRecord {
                namespace: self.config.chain_id.clone(),
                epoch: event.epoch,
                valset_root: set.valset_root,
                admission_root: set.admission_root,
                validator_identity: self.me,
                wireguard_public_key: self.config.wireguard_public,
                control_endpoint: self.config.control_endpoint,
                wireguard_endpoint: self.config.wireguard_advertised,
                nonce: epoch_nonce_seed(self.now_ms),
            },
            &*self.signer,
        );
        let mut state = EpochState::new(
            event.epoch,
            role,
            set,
            peers,
            standby_ids,
            pk_of,
            own.clone(),
        );
        // the carried standby records seed this epoch's pre-warm layer as if
        // just delivered — the epoch's own apply REPLACES whatever the
        // interface held, and a standby cannot re-deliver its record over
        // the overlay that apply just rebuilt. That holds for BOTH sources:
        // the boot restore's persisted records (the node forgot the standby
        // across a reboot) and a live cutover's superseded epoch (the node
        // would forget it across the cutover, and an endpoint-less standby
        // — every NATed one — has no other way back in: only it can
        // initiate, and the member it initiates to must already carry its
        // key). Nonces stay unseeded so the owner's live re-offer re-runs
        // the full accept path: an idempotent reinstall plus the
        // first-contact gossip-back it heals by.
        //
        // The epoch's own standby set gates them: a resident that left is
        // dead weight, and a promoted one assembles a member tunnel instead.
        for signed in restored_standbys {
            let identity = signed.record.validator_identity;
            if !state.standbys.contains(&identity) {
                continue;
            }
            state
                .prewarm_peers
                .insert(identity, self.standby_peer_config(&signed.record));
            state.standby_records.insert(identity, signed);
        }
        let own_record = ReachabilityMsg::Record(own);
        for peer in state.peers.clone() {
            self.send_msg(&state, peer, &own_record);
        }
        match role {
            Role::Standby => Ok(state),
            Role::Member => {
                // seed the pre-warm layer's counterparties too (a lost send
                // heals by nudge; a standby with no route yet just misses
                // this round).
                for standby in state.standbys.clone() {
                    self.send_msg(&state, standby, &own_record);
                }
                self.advance(&mut state)?;
                Ok(state)
            }
        }
    }

    /// A standby record's peer tunnel config, endpoint taken VERBATIM (no
    /// rendezvous resolution): the parked side initiates (and roams), so
    /// its recorded endpoint is a first target, not a requirement — the
    /// install's real cargo is the WireGuard key.
    pub(crate) fn standby_peer_config(&self, record: &EndpointRecord) -> PeerTunnelConfig {
        PeerTunnelConfig {
            wireguard_public_key: record.wireguard_public_key,
            endpoint: record.wireguard_endpoint.map(|e| e.socket_addr()),
            allowed_ips: self.overlay.identity_allowed_ips(record.validator_identity),
            keepalive_seconds: Some(KEEPALIVE_SECONDS),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use commonware_cryptography::Signer as _;
    use commonware_cryptography::ed25519::PrivateKey;
    use wireguard::{
        AdmissionRoot, Endpoint, EndpointAdvertisement, MeshVersion, PortPolicy, Root, Transport,
        X25519PublicKey,
    };

    use super::*;
    use crate::contract::{Effect, Event, MachineConfig};
    use crate::machine::Machine;
    use crate::store::{self, PersistedMesh};

    const CHAIN: &str = "net#restore";

    fn policy() -> PortPolicy {
        PortPolicy::production()
    }

    fn config(octet: u8) -> MachineConfig {
        let policy = policy();
        let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, octet));
        let endpoint = |port, transport| Endpoint::new(ip, port, transport, &policy).unwrap();
        MachineConfig {
            chain_id: CHAIN.into(),
            wireguard_public: X25519PublicKey([octet; 32]),
            // the reported node: fully NATed, so it advertises no
            // WireGuard endpoint of its own.
            wireguard_advertised: None,
            control_endpoint: endpoint(443, Transport::Tcp),
            coordinators: Vec::new(),
            port_policy: policy,
            persist: false,
            gossip_ingress: None,
        }
    }

    /// One member's persisted advert, endpoint-less exactly like the
    /// NATed inviter whose tunnel the restart must reinstall.
    fn persisted_member(signer: &PrivateKey, epoch: u64) -> PersistedMesh {
        let record = EndpointRecord {
            namespace: CHAIN.into(),
            epoch,
            valset_root: Root([1; 32]),
            admission_root: AdmissionRoot([2; 32]),
            validator_identity: binding::identity_of(&signer.public_key()),
            wireguard_public_key: X25519PublicKey([20; 32]),
            control_endpoint: Endpoint::new(
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 20)),
                443,
                Transport::Tcp,
                &policy(),
            )
            .unwrap(),
            wireguard_endpoint: None,
            nonce: 1,
        };
        PersistedMesh::new(
            CHAIN.into(),
            epoch,
            vec![EndpointAdvertisement::sign(
                record,
                MeshVersion([7; 32]),
                signer,
            )],
            Vec::new(),
            Vec::new(),
        )
    }

    /// The restarted RESIDENT's boot: the host hands the persisted mesh with
    /// this node listed as a STANDBY, and the plane must target that epoch —
    /// reinstalling the member's tunnel from the persisted record — with no
    /// live gossip, since every transport it has left rides that tunnel.
    #[test]
    fn a_standby_restore_targets_the_plane_and_reinstalls_the_member() {
        let me = PrivateKey::from_seed(1);
        let member = PrivateKey::from_seed(2);
        let mut machine = Machine::new(Box::new(me.clone()), config(1));
        let bytes = store::encode(&persisted_member(&member, 4)).unwrap();

        let effects = machine
            .step(
                Event::Retarget {
                    event: MeshEpochEvent {
                        epoch: 4,
                        members: vec![member.public_key()],
                        standbys: vec![me.public_key()],
                        current_view: 1_783,
                    },
                    persisted: Some(bytes),
                },
                1_000,
            )
            .unwrap();

        // the member advertises no endpoint, so nothing resolves: the
        // restore goes straight to its interface push.
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ResolveStart { .. })),
            "an endpoint-less record has nothing to resolve"
        );
        let (req, peers) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::WgApply { req, peers, .. } => Some((*req, peers.clone())),
                _ => None,
            })
            .expect("the restore pushes the remembered mesh");
        assert_eq!(peers.len(), 1, "the persisted member's tunnel");
        assert_eq!(peers[0].wireguard_public_key, X25519PublicKey([20; 32]));
        assert!(
            machine.epoch.is_none(),
            "the epoch is suspended behind the restore"
        );

        machine
            .step(
                Event::WgApplied {
                    req,
                    outcome: Ok(()),
                },
                1_000,
            )
            .unwrap();

        let state = machine
            .epoch
            .as_ref()
            .expect("the restore landed the epoch");
        assert_eq!(state.role, Role::Standby);
        assert_eq!(state.epoch, 4);
        assert!(
            machine
                .driver
                .base_peers
                .as_ref()
                .is_some_and(|base| base.contains_key(&binding::identity_of(&member.public_key()))),
            "the restored mesh is the interface's base"
        );
    }

    /// The same boot, one tick earlier: a nudge that arrives while the
    /// restore is still resolving must NOT count the plane untargeted — the
    /// epoch lands when the restore settles.
    #[test]
    fn a_nudge_inside_the_restore_window_is_not_an_untargeted_plane() {
        let me = PrivateKey::from_seed(1);
        let member = PrivateKey::from_seed(2);
        let mut machine = Machine::new(Box::new(me.clone()), config(1));
        // an ADVERTISED endpoint parks the restore on a resolve, which is
        // the window a boot nudge can land in.
        let mut mesh = persisted_member(&member, 4);
        mesh.adverts.clear();
        let record = EndpointRecord {
            namespace: CHAIN.into(),
            epoch: 4,
            valset_root: Root([1; 32]),
            admission_root: AdmissionRoot([2; 32]),
            validator_identity: binding::identity_of(&member.public_key()),
            wireguard_public_key: X25519PublicKey([20; 32]),
            control_endpoint: Endpoint::new(
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 20)),
                443,
                Transport::Tcp,
                &policy(),
            )
            .unwrap(),
            wireguard_endpoint: Some(
                Endpoint::new(
                    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 20)),
                    51_820,
                    Transport::Udp,
                    &policy(),
                )
                .unwrap(),
            ),
            nonce: 1,
        };
        mesh.adverts.push(EndpointAdvertisement::sign(
            record,
            MeshVersion([7; 32]),
            &member,
        ));
        let bytes = store::encode(&mesh).unwrap();

        machine
            .step(
                Event::Retarget {
                    event: MeshEpochEvent {
                        epoch: 4,
                        members: vec![member.public_key()],
                        standbys: vec![me.public_key()],
                        current_view: 1_783,
                    },
                    persisted: Some(bytes),
                },
                1_000,
            )
            .unwrap();
        assert!(machine.driver.pending_restore.is_some());

        machine.step(Event::Nudge, 2_000).unwrap();
        assert_eq!(
            machine.driver.untargeted_nudges, 0,
            "a retarget suspended behind its restore IS targeted"
        );
    }
}
