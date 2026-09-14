//! A consumed durable queue and a receiver restored from a checkpoint.
//! Replaying the saved delivery also needs its original sibling read: the
//! source's current POST state differs from the state the receiver observed.

use std::cell::RefCell;
use std::rc::Rc;

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::Host;
use node::{OrderedNode, RoundOrderer};
use recovery::{Manifest, Recovery};
use sdk::{
    Ack, Cause, Ctx, DeliveryOutcome, Error, Hop, ItemRef, Module, ModuleId, Msg, Origin,
    PendingItem, Root, StateRoot, StateSyncHandle,
};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum QueueState {
    #[default]
    Ready,
    Retired,
}

#[derive(Default)]
struct Disk {
    queue: QueueState,
    commits: u64,
}

struct Source {
    disk: Rc<RefCell<Disk>>,
    staged: Option<QueueState>,
    behavior: SourceBehavior,
}

#[derive(Clone, Copy)]
enum SourceBehavior {
    Nudge,
    EmitOnlyWhileReady,
    LargeSiblingReply,
}

impl Source {
    fn reopen(disk: Rc<RefCell<Disk>>, behavior: SourceBehavior) -> Self {
        Self {
            disk,
            staged: None,
            behavior,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Source {
    fn id(&self) -> ModuleId {
        "source".into()
    }

    fn root(&self) -> StateRoot {
        let disk = self.disk.borrow();
        let mut hash = Sha256::new();
        hash.update([match disk.queue {
            QueueState::Ready => 0,
            QueueState::Retired => 1,
        }]);
        hash.update(disk.commits.to_le_bytes());
        StateRoot(hash.finalize().into())
    }

    fn block_durable(&self) -> bool {
        true
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "review-disk".into(),
            detail: "shared cell survives the host and receiver".into(),
        })
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        match self.behavior {
            SourceBehavior::Nudge | SourceBehavior::LargeSiblingReply => Ok(()),
            SourceBehavior::EmitOnlyWhileReady => {
                let retired = self.disk.borrow().queue == QueueState::Retired;
                if retired {
                    return Err(Error::Module("source action already consumed".into()));
                }
                ctx.emit_msg(Msg {
                    target: "receiver".into(),
                    payload: Vec::new(),
                });
                Ok(())
            }
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let supported = req == b"is_ready";
        if !supported {
            return Err(Error::QueryUnsupported);
        }
        let ready = match self.disk.borrow().queue {
            QueueState::Ready => 1,
            QueueState::Retired => 0,
        };
        let size = match self.behavior {
            SourceBehavior::Nudge | SourceBehavior::EmitOnlyWhileReady => 1,
            SourceBehavior::LargeSiblingReply => 3 * 1024 * 1024,
        };
        let mut reply = vec![0; size];
        reply[0] = ready;
        Ok(reply)
    }

    async fn pending_items(&self) -> Result<Vec<PendingItem>, Error> {
        let retired = self.disk.borrow().queue == QueueState::Retired;
        if retired {
            return Ok(Vec::new());
        }
        let item = ItemRef {
            source: self.id(),
            item: 0,
        };
        Ok(vec![PendingItem {
            item: 0,
            target: "receiver".into(),
            payload: Vec::new(),
            cause: Cause::Chain {
                root: Root::Item(item.clone()),
                hop: Hop::Delivery(item),
            },
        }])
    }

    async fn acknowledge(&mut self, ctx: &mut dyn Ctx, ack: &Ack) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::System);
        assert_eq!(ack.item, 0);
        assert_eq!(ack.target, "receiver");
        assert_eq!(ack.outcome, DeliveryOutcome::Applied);
        let already_retired = self.disk.borrow().queue == QueueState::Retired;
        if already_retired {
            return Ok(());
        }
        self.staged = Some(QueueState::Retired);
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        let Some(queue) = self.staged.take() else {
            return Ok(());
        };
        let mut disk = self.disk.borrow_mut();
        disk.queue = queue;
        disk.commits += 1;
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

#[derive(Default)]
struct Receiver {
    observed_ready: Option<bool>,
    received: u64,
    staged: Option<(bool, u64)>,
}

impl Receiver {
    fn snapshot(&self) -> Vec<u8> {
        let mut bytes = vec![match self.observed_ready {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }];
        bytes.extend_from_slice(&self.received.to_le_bytes());
        bytes
    }

    fn restore(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 9);
        let observed_ready = match bytes[0] {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            _ => panic!("bad receiver snapshot"),
        };
        Self {
            observed_ready,
            received: u64::from_le_bytes(bytes[1..].try_into().unwrap()),
            staged: None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Receiver {
    fn id(&self) -> ModuleId {
        "receiver".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot(Sha256::digest(self.snapshot()).into())
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.snapshot()))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::Module("source".into()));
        let answer = ctx.query("source", b"is_ready").await?;
        let observed = match answer.first() {
            Some(1) => true,
            Some(0) => false,
            _ => return Err(Error::Module("bad source reply".into())),
        };
        let received = self
            .staged
            .map(|(_, received)| received)
            .unwrap_or(self.received)
            .checked_add(1)
            .unwrap();
        self.staged = Some((observed, received));
        Ok(())
    }

    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.snapshot())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some((observed, received)) = self.staged.take() {
            self.observed_ready = Some(observed);
            self.received = received;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

#[test]
fn consumed_source_at_post_replays_receivers_original_read_from_pre() {
    recover_consumed_source(SourceBehavior::Nudge);
}

#[test]
fn source_at_post_preserves_its_original_followup_to_receiver_at_pre() {
    recover_consumed_source(SourceBehavior::EmitOnlyWhileReady);
}

#[test]
fn large_valid_sibling_answer_survives_the_recovery_journal() {
    recover_consumed_source(SourceBehavior::LargeSiblingReply);
}

fn recover_consumed_source(behavior: SourceBehavior) {
    deterministic::Runner::default().start(|context| async move {
        let disk = Rc::new(RefCell::new(Disk::default()));
        let recovery = Recovery::open(context.child("first")).await.unwrap();
        let host = Host::genesis(vec![
            Box::new(Source::reopen(Rc::clone(&disk), behavior)),
            Box::new(Receiver::default()),
        ])
        .unwrap();
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let position = node.sink_mut().oplog_pos().await;
        let checkpoint =
            Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, position, 1).unwrap();
        assert!(checkpoint.snapshot("source").is_none());
        node.sink_mut().write_manifest(&checkpoint).await.unwrap();

        node.submit(
            &PrivateKey::from_seed(1),
            0,
            Msg {
                target: "source".into(),
                payload: Vec::new(),
            },
        )
        .await
        .unwrap();
        node.flush_batch().await.unwrap();
        assert_eq!(node.drain_delivered().await.unwrap(), 1);
        let sealed_tip = node.root_hash();
        let expected_received: u64 = match behavior {
            SourceBehavior::Nudge | SourceBehavior::LargeSiblingReply => 1,
            SourceBehavior::EmitOnlyWhileReady => 2,
        };
        let mut expected_receiver = vec![2];
        expected_receiver.extend_from_slice(&expected_received.to_le_bytes());
        assert_eq!(
            node.host().query("receiver", b"").await.unwrap(),
            expected_receiver
        );
        assert_eq!(disk.borrow().commits, 1);
        drop(node);

        let mut recovery = Recovery::open(context.child("restart")).await.unwrap();
        let checkpoint = recovery.manifest().unwrap().unwrap();
        let receiver = Receiver::restore(checkpoint.snapshot("receiver").unwrap());
        let mut host = Host::genesis(vec![
            Box::new(Source::reopen(Rc::clone(&disk), behavior)),
            Box::new(receiver),
        ])
        .unwrap();
        assert_eq!(host.module_root("receiver"), checkpoint.root("receiver"));
        assert_ne!(host.module_root("source"), checkpoint.root("source"));
        assert_eq!(
            host.query("source", b"is_ready").await.unwrap().first(),
            Some(&0)
        );
        assert!(!host.has_pending_work().await.unwrap());

        let restored = recovery
            .recover(&mut host, &checkpoint)
            .await
            .expect("saved delivery and its original sibling read must reproduce the sealed state");
        assert_eq!(restored.root_hash, sealed_tip);
        assert_eq!(
            host.query("receiver", b"").await.unwrap(),
            expected_receiver
        );
        assert_eq!(disk.borrow().commits, 1, "the source must not recommit");
    });
}

/// The registry's pending swap disappears at its own commit. A receiver at PRE
/// still needs the boundary injection that originally emitted its write.
struct RegistryBoundary {
    disk: Rc<RefCell<bool>>,
    staged: Option<bool>,
}

#[async_trait::async_trait(?Send)]
impl Module for RegistryBoundary {
    fn id(&self) -> ModuleId {
        "modules".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([u8::from(*self.disk.borrow()); 32])
    }
    fn block_durable(&self) -> bool {
        true
    }
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "review-registry".into(),
            detail: "durable shared cell".into(),
        })
    }
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match modules::decode_query(req).map_err(Error::Module)? {
            modules::ModulesQuery::ModuleStatus => {
                let pending = match *self.disk.borrow() {
                    true => None,
                    false => Some(modules::ScheduledSwap {
                        name: "receiver".into(),
                        activation_height: 1,
                        code_hash: vec![1; 32],
                        readiness: Vec::new(),
                        ready_at: Some(0),
                    }),
                };
                Ok(modules::encode_reply(
                    &modules::ModulesReply::ModuleStatus {
                        modules: vec![modules::ModuleCode {
                            module_id: "receiver".into(),
                            kind: modules::Kind::Module,
                            active_code_hash: vec![1; 32],
                            pending,
                            history: vec![modules::Activation {
                                height: 0,
                                code_hash: vec![1; 32],
                            }],
                        }],
                    },
                ))
            }
            modules::ModulesQuery::ArmedAt { .. } => {
                Ok(modules::encode_reply(&modules::ModulesReply::ArmedAt {
                    swaps: Vec::new(),
                }))
            }
        }
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let nudge = msg.payload == b"nudge";
        if nudge {
            return Ok(());
        }
        assert_eq!(
            modules::decode_msg(&msg.payload).unwrap(),
            modules::ModulesMsg::Advance
        );
        assert_eq!(ctx.env().origin, Origin::System);
        let already_advanced = *self.disk.borrow();
        if already_advanced {
            return Err(Error::Module("boundary already advanced".into()));
        }
        self.staged = Some(true);
        ctx.emit_msg(Msg {
            target: "receiver".into(),
            payload: Vec::new(),
        });
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(state) = self.staged.take() {
            *self.disk.borrow_mut() = state;
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

#[derive(Default)]
struct BoundaryReceiver {
    committed: u8,
    staged: Option<u8>,
}

#[async_trait::async_trait(?Send)]
impl Module for BoundaryReceiver {
    fn id(&self) -> ModuleId {
        "receiver".into()
    }
    fn code_hash(&self) -> Option<Vec<u8>> {
        Some(vec![1; 32])
    }
    fn root(&self) -> StateRoot {
        StateRoot([self.committed; 32])
    }
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(vec![self.committed]))
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::Module("modules".into()));
        self.staged = Some(
            self.staged
                .unwrap_or(self.committed)
                .checked_add(1)
                .unwrap(),
        );
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(value) = self.staged.take() {
            self.committed = value;
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

#[test]
fn registry_at_post_keeps_the_original_boundary_injection_for_a_receiver_at_pre() {
    deterministic::Runner::default().start(|context| async move {
        let disk = Rc::new(RefCell::new(false));
        let recovery = Recovery::open(context.child("first")).await.unwrap();
        let host = Host::genesis(vec![
            Box::new(RegistryBoundary {
                disk: Rc::clone(&disk),
                staged: None,
            }),
            Box::new(BoundaryReceiver::default()),
        ])
        .unwrap();
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let position = node.sink_mut().oplog_pos().await;
        let checkpoint =
            Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, position, 1).unwrap();
        node.sink_mut().write_manifest(&checkpoint).await.unwrap();
        for sequence in 0..2 {
            node.submit(
                &PrivateKey::from_seed(1),
                sequence,
                Msg {
                    target: "modules".into(),
                    payload: b"nudge".to_vec(),
                },
            )
            .await
            .unwrap();
            node.flush_batch().await.unwrap();
            assert_eq!(node.drain_delivered().await.unwrap(), 1);
        }
        let tip = node.root_hash();
        assert!(*disk.borrow());
        assert_eq!(
            node.host().module_root("receiver"),
            Some(StateRoot([1; 32]))
        );
        drop(node);
        let mut recovery = Recovery::open(context.child("restart")).await.unwrap();
        let checkpoint = recovery.manifest().unwrap().unwrap();
        let mut host = Host::genesis(vec![
            Box::new(RegistryBoundary { disk, staged: None }),
            Box::new(BoundaryReceiver::default()),
        ])
        .unwrap();
        assert!(
            host.prepare_work(1).await.unwrap().advance.is_none(),
            "POST has no pending boundary"
        );
        let restored = recovery.recover(&mut host, &checkpoint).await.unwrap();
        assert_eq!(restored.root_hash, tip);
        assert_eq!(host.module_root("receiver"), Some(StateRoot([1; 32])));
    });
}
