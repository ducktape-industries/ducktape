//! Raw joins check the finalized aggregate root, stream every program, and resume at each log's next op.
use super::*;
use commonware_codec::Encode as _;
use statesync::{
    HeldClosing,
    lane::{Closing, Piece, Trusted},
};

#[derive(Clone)]
struct BulkSource {
    inner: Loopback,
    opening: Block,
    asked: Arc<Mutex<Vec<(String, u64)>>>,
    fault: Arc<Mutex<FaultPlan>>,
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<validators::Member>,
    forged: Option<(Vec<ed25519::PrivateKey>, Vec<validators::Member>)>,
}

enum FaultPlan {
    None,
    Cut { program: String, from: Option<u64> },
    Drop { program: String },
}

impl Exchange for BulkSource {
    type Error = Outage;

    async fn exchange(&self, _request: Request) -> Result<Response, Outage> {
        panic!("a raw join must not use the proof protocol");
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Outage>> + Send {
        self.inner
            .stream_blobs(ids)
            .map(|answer| Ok(answer.unwrap()))
    }
}

impl BulkSource {
    async fn closing_at(&self, past: Option<u64>, opening: Block) -> Result<HeldClosing, Outage> {
        let held = self.inner.hold().await.unwrap();
        let Response::Head(mut head) = held.head else {
            panic!("source head")
        };
        assert!(past.is_none_or(|past| head.tip.height > past));
        if let Some((keys, members)) = &self.forged {
            head.anchor = Anchor::Finalized(certify(keys, members, head.tip));
        }
        let certificate = certify(&self.keys, &self.members, opening.tip());
        let closing = Closing {
            head,
            hold: held.hold.unwrap(),
            opening,
            certificate,
        };
        Ok(HeldClosing {
            closing,
            ending: held.ending,
        })
    }
}

impl statesync::Bulk for BulkSource {
    async fn closing(&self, past: Option<u64>) -> Result<HeldClosing, Outage> {
        self.closing_at(past, self.opening.clone()).await
    }

    fn ops(
        &self,
        hold: u64,
        program: String,
        from: u64,
    ) -> impl Stream<Item = Result<Piece, Outage>> + Send {
        self.asked.lock().unwrap().push((program.clone(), from));
        let serving = self.inner.0.holds.serving(hold, &program).unwrap();
        let fault = self.fault.clone();
        futures::stream::unfold(
            Some((serving, from, Transfer::Pinned, program)),
            move |state| {
                let fault = fault.clone();
                async move {
                    let (serving, mut from, phase, program) = state?;
                    let (piece, next) = match phase {
                        Transfer::Pinned => {
                            let nodes = serving
                                .pinned()
                                .await
                                .unwrap()
                                .into_iter()
                                .map(|digest| digest.0)
                                .collect();
                            (
                                Ok(Piece::Pinned(nodes)),
                                Some((serving, from, Transfer::Ops, program)),
                            )
                        }
                        Transfer::Ops => {
                            let cut = {
                                let mut fault = fault.lock().unwrap();
                                match &mut *fault {
                                    FaultPlan::Cut {
                                        program: cut_program,
                                        from: cut_from,
                                    } if *cut_program == program => match cut_from {
                                        None => {
                                            *cut_from = Some(from);
                                            false
                                        }
                                        Some(first) => {
                                            assert!(from > *first);
                                            *fault = FaultPlan::None;
                                            true
                                        }
                                    },
                                    FaultPlan::Drop { program: dropped } if *dropped == program => {
                                        *fault = FaultPlan::None;
                                        true
                                    }
                                    _ => false,
                                }
                            };
                            if cut {
                                return Some((Err(Outage(Failure::Transient("cut"))), None));
                            }
                            let end = *serving.target.as_ref().unwrap().range.end();
                            if from == end {
                                return Some((Ok(Piece::End), None));
                            }
                            let ops = serving
                                .ops(from, std::num::NonZeroU64::new(1).unwrap())
                                .await
                                .unwrap();
                            from += ops.len() as u64;
                            let piece = Piece::Ops(ops.encode().to_vec());
                            (Ok(piece), Some((serving, from, Transfer::Ops, program)))
                        }
                    };
                    Some((piece, next))
                }
            },
        )
    }
}

enum Transfer {
    Pinned,
    Ops,
}

async fn opening_of(source: &Source) -> Block {
    let node = source.node.lock().await;
    let tip = node.tip().unwrap();
    let mut opening = Block::next(tip, TIME + tip.height + 1, Vec::new());
    opening.settle(node::Settled {
        height: tip.height,
        root: node.host().root().unwrap().0,
    });
    opening
}

async fn bulk_source(context: &Ctx) -> (Network, BulkSource, Trusted) {
    let mut network = found(context).await;
    network.advance(Vec::new()).await;
    network.advance(Vec::new()).await;
    network
        .advance(vec![frame(&key(2), vec![set(b"raw", b"joined")])])
        .await;
    let opening = opening_of(&network.source).await;
    let source = BulkSource {
        inner: network.exchange(),
        opening,
        asked: Arc::default(),
        fault: Arc::new(Mutex::new(FaultPlan::None)),
        keys: network.keys.clone(),
        members: network.members.clone(),
        forged: None,
    };
    let trusted = Trusted {
        network: NETWORK.to_vec(),
        epoch: 0,
        validators: network
            .members
            .iter()
            .map(|member| member.key.clone())
            .collect(),
    };
    (network, source, trusted)
}

#[test]
fn a_raw_join_checks_the_finalized_root_without_a_proof_request() {
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (network, source, trusted) = bulk_source(&context).await;
        let joined = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source,
        )
        .await
        .unwrap();
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
    });
}

#[test]
fn a_raw_join_rejects_an_aggregate_root_even_when_each_target_is_intact() {
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (_, mut source, trusted) = bulk_source(&context).await;
        source.opening.frames.clear();
        source.opening.settle(node::Settled {
            height: source.opening.height - 1,
            root: [3; 32],
        });
        let result = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source,
        )
        .await;
        assert!(matches!(result, Err(Error::Root { .. })));
    });
}

#[test]
fn a_raw_join_resumes_the_cut_program_without_restarting_the_completed_programs() {
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (network, source, trusted) = bulk_source(&context).await;
        *source.fault.lock().unwrap() = FaultPlan::Cut {
            program: "probe".into(),
            from: None,
        };
        let joined = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source.clone(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        let asked = source.asked.lock().unwrap();
        let probe: Vec<_> = asked
            .iter()
            .filter(|(program, _)| program == "probe")
            .map(|(_, from)| *from)
            .collect();
        assert_eq!(probe.len(), 2);
        assert!(probe[1] > probe[0]);
        for (program, _) in asked.iter().filter(|(program, _)| program != "probe") {
            assert_eq!(
                asked.iter().filter(|(other, _)| other == program).count(),
                1,
                "{program} restarted"
            );
        }
    });
}

use statesync::Bulk as _;

#[derive(Clone)]
struct BarrierSource {
    inner: BulkSource,
    ready: Arc<Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
    gate: futures::future::Shared<futures::future::BoxFuture<'static, ()>>,
    programs: usize,
}

impl BarrierSource {
    fn started(&self) {
        let asked = self.inner.asked.lock().unwrap().len();
        if asked != self.programs {
            return;
        }
        let Some(ready) = self.ready.lock().unwrap().take() else {
            return;
        };
        ready.send(()).unwrap();
    }
}

impl Exchange for BarrierSource {
    type Error = Outage;

    async fn exchange(&self, request: Request) -> Result<Response, Outage> {
        self.inner.exchange(request).await
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Outage>> + Send {
        self.inner.stream_blobs(ids)
    }
}

impl statesync::Bulk for BarrierSource {
    async fn closing(&self, past: Option<u64>) -> Result<HeldClosing, Outage> {
        self.inner.closing(past).await
    }

    fn ops(
        &self,
        hold: u64,
        program: String,
        from: u64,
    ) -> impl Stream<Item = Result<Piece, Outage>> + Send {
        use futures::FutureExt as _;
        let ops = self.inner.ops(hold, program, from);
        self.started();
        let gate = self.gate.clone();
        ops.then(move |piece| gate.clone().map(move |()| piece))
    }
}

#[test]
fn a_raw_join_starts_every_program_before_any_program_can_finish() {
    use futures::FutureExt as _;
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (network, source, trusted) = bulk_source(&context).await;
        let programs = source
            .closing(None)
            .await
            .unwrap()
            .closing
            .head
            .targets
            .len();
        let (started, all_started) = futures::channel::oneshot::channel();
        let (release, gate) = futures::channel::oneshot::channel();
        let source = BarrierSource {
            inner: source,
            ready: Arc::new(Mutex::new(Some(started))),
            gate: async move { gate.await.unwrap() }.boxed().shared(),
            programs,
        };
        let joined = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source.clone(),
        );
        let release = async move {
            all_started.await.unwrap();
            let asked: BTreeMap<_, _> =
                source.inner.asked.lock().unwrap().iter().cloned().collect();
            assert_eq!(asked.len(), programs);
            release.send(()).unwrap();
        };
        let (joined, ()) = futures::future::join(joined, release).await;
        assert_eq!(
            joined.unwrap().node.host().root().unwrap(),
            network.root().await
        );
    });
}

#[test]
fn a_raw_join_refuses_a_tip_anchor_its_adopted_validators_did_not_sign() {
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (_, mut source, trusted) = bulk_source(&context).await;
        let strangers: Vec<_> = (21..=23).map(key).collect();
        let members = strangers.iter().map(member).collect();
        source.forged = Some((strangers, members));
        let result = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source,
        )
        .await;
        assert!(matches!(result, Err(Error::Certificate { .. })));
    });
}

#[derive(Clone)]
struct AdvancingSource {
    inner: BulkSource,
    closings: Arc<AtomicUsize>,
    targets: Arc<Mutex<Vec<BTreeMap<String, state::SyncTarget>>>>,
}

impl Exchange for AdvancingSource {
    type Error = Outage;

    async fn exchange(&self, request: Request) -> Result<Response, Outage> {
        self.inner.exchange(request).await
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Outage>> + Send {
        self.inner.stream_blobs(ids)
    }
}

impl statesync::Bulk for AdvancingSource {
    async fn closing(&self, past: Option<u64>) -> Result<HeldClosing, Outage> {
        let second = self
            .closings
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 1;
        let opening = match second {
            false => self.inner.opening.clone(),
            true => {
                let source = &self.inner.inner.0;
                advance(
                    source,
                    &self.inner.keys,
                    &self.inner.members,
                    vec![frame(&key(13), vec![set(b"raw", b"advanced")])],
                )
                .await;
                opening_of(source).await
            }
        };
        let held = self.inner.closing_at(past, opening).await?;
        self.targets
            .lock()
            .unwrap()
            .push(held.closing.head.targets.clone());
        Ok(held)
    }

    fn ops(
        &self,
        hold: u64,
        program: String,
        from: u64,
    ) -> impl Stream<Item = Result<Piece, Outage>> + Send {
        self.inner.ops(hold, program, from)
    }
}

#[test]
fn a_raw_join_keeps_unchanged_programs_and_fetches_only_a_changed_programs_delta_at_a_new_close() {
    deterministic::Runner::default().start(|context| async move {
        let join_dir = tempfile::tempdir().unwrap();
        let (mut network, mut source, trusted) = bulk_source(&context).await;
        let held: Vec<_> = (0..8u8).map(|n| set(&[b'k', n], b"held")).collect();
        network.advance(vec![frame(&key(14), held)]).await;
        source.opening = opening_of(&network.source).await;
        *source.fault.lock().unwrap() = FaultPlan::Drop {
            program: "ping".into(),
        };
        let source = AdvancingSource {
            inner: source,
            closings: Arc::default(),
            targets: Arc::default(),
        };
        let joined = statesync::bulk_join(
            context.child("raw_join"),
            "raw-join",
            join_dir.path(),
            trusted,
            Duration::from_secs(1),
            source.clone(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        let targets = source.targets.lock().unwrap();
        assert_eq!(targets.len(), 2);
        let (before, after) = (&targets[0], &targets[1]);
        assert_ne!(before.get("probe"), after.get("probe"));
        let asked = source.inner.asked.lock().unwrap();
        let mut unchanged = 0;
        let mut changed = 0;
        let mut reused = 0;
        for (program, target) in before.iter().filter(|(program, _)| *program != "ping") {
            let froms: Vec<u64> = asked
                .iter()
                .filter(|(other, _)| other == program)
                .map(|(_, from)| *from)
                .collect();
            if after.get(program) == Some(target) {
                unchanged += 1;
                assert_eq!(
                    froms.len(),
                    1,
                    "{program} was fetched again at an unchanged target"
                );
                continue;
            }
            changed += 1;
            assert_eq!(froms.len(), 2, "{program} asked {froms:?}");
            let end = *target.range.end();
            let start = *after[program].range.start();
            let resumed = froms[1] == end.max(start);
            assert!(
                resumed,
                "{program} asked again from {froms:?}, its old end {end:?}, its new floor {start:?}"
            );
            if start < end {
                reused += 1;
            }
        }
        assert!(unchanged > 0, "no program kept its target");
        assert!(changed > 0, "no completed program changed its target");
        assert!(reused > 0, "no changed program kept any op it already held");
    });
}
