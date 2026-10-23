// The node's read views: its state pinned after the last block it applied, with that moment's
// status encoded once, its commitments' sync readers and anchor, and the pool's preconfirmed layer.
// The writer and the pool publish fresh ones by swap; every reader loads the newest without waiting.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;

use abi::{BlobId, ProgramId};
use bytes::Bytes;
use commonware_cryptography::Digestible as _;
use consensus::Network;
use host::{Layer, Snapshot, Source, Tip};
use node::Node;
use tokio::sync::watch;

use crate::wire::{NODE_CONTRACT, Status};
use crate::workspace::Descriptor;
use crate::{Context, Result};

pub struct View<E: Context> {
    snapshot: Snapshot<E>,
    tip: Tip,
    status: Bytes,
    programs: OnceLock<Bytes>,
    sync: Arc<statesync::Published<E>>,
    _reader: watch::Receiver<()>,
}

pub(crate) struct Facts<'a> {
    pub(crate) descriptor: &'a Descriptor,
    pub(crate) network: &'a Network,
    pub(crate) identity: &'a [u8],
    pub(crate) readers: &'a watch::Sender<()>,
}

impl<E: Context> View<E> {
    pub(crate) fn of(
        node: &Node<E>,
        facts: &Facts<'_>,
        anchor: Option<statesync::Anchor>,
    ) -> Result<View<E>> {
        let snapshot = node.snapshot()?;
        let source = snapshot.source();
        let tip = source.tip()?;
        let status = Status {
            network: facts.descriptor.network.clone(),
            time: facts.descriptor.time,
            block_time_ms: facts.descriptor.block_time_ms,
            epoch_length: facts.network.epoch_length,
            height: tip.height,
            tip: tip.id,
            root: source.root()?,
            epoch: facts.network.epoch_after(tip.height),
            identity: facts.identity.to_vec(),
            contract: NODE_CONTRACT,
            genesis: facts.descriptor.genesis_block().digest().0,
            roles: source.roles().clone(),
        };
        Ok(View {
            snapshot,
            tip,
            status: Bytes::from(abi::encode(&status)),
            programs: OnceLock::new(),
            sync: Arc::new(statesync::Published::of(node, anchor)?),
            _reader: facts.readers.subscribe(),
        })
    }

    pub fn source(&self) -> Source<'_, E> {
        self.snapshot.source()
    }

    pub(crate) fn snapshot(&self) -> Snapshot<E> {
        self.snapshot.clone()
    }

    pub fn tip(&self) -> Tip {
        self.tip
    }

    pub(crate) fn sync(&self) -> &Arc<statesync::Published<E>> {
        &self.sync
    }

    pub fn status(&self) -> Bytes {
        self.status.clone()
    }

    pub(crate) fn programs(&self) -> Result<Bytes> {
        if let Some(encoded) = self.programs.get() {
            return Ok(encoded.clone());
        }
        let programs: BTreeMap<ProgramId, BlobId> = self.source().programs()?;
        let encoded = Bytes::from(abi::encode(&programs));
        Ok(self.programs.get_or_init(|| encoded).clone())
    }
}

pub struct Speculative<E: Context> {
    _view: Arc<View<E>>,
    snapshot: Snapshot<E>,
}

impl<E: Context> Speculative<E> {
    pub(crate) fn over(view: Arc<View<E>>, snapshot: Snapshot<E>) -> Speculative<E> {
        Speculative {
            _view: view,
            snapshot,
        }
    }

    pub fn source(&self) -> Source<'_, E> {
        self.snapshot.source()
    }
}

pub enum Reading<E: Context> {
    Confirmed(Arc<View<E>>),
    Preconfirmed(Arc<Speculative<E>>),
}

impl<E: Context> Reading<E> {
    pub fn source(&self) -> Source<'_, E> {
        match self {
            Reading::Confirmed(view) => view.source(),
            Reading::Preconfirmed(speculative) => speculative.source(),
        }
    }
}

pub(crate) struct Current<E: Context> {
    pub(crate) views: ArcSwapOption<View<E>>,
    pub(crate) speculative: ArcSwapOption<Speculative<E>>,
    #[cfg(feature = "testkit")]
    pub(crate) checking_anchors: crate::testkit::Gate,
}

impl<E: Context> Current<E> {
    pub(crate) fn new() -> Current<E> {
        Current {
            views: ArcSwapOption::empty(),
            speculative: ArcSwapOption::empty(),
            #[cfg(feature = "testkit")]
            checking_anchors: Default::default(),
        }
    }

    pub(crate) fn view(&self) -> Result<Arc<View<E>>> {
        self.views.load_full().ok_or(crate::Error::Stopped)
    }

    pub(crate) fn reading(&self, layer: Layer) -> Result<Reading<E>> {
        match layer {
            Layer::Confirmed => Ok(Reading::Confirmed(self.view()?)),
            Layer::Preconfirmed => self
                .speculative
                .load_full()
                .map(Reading::Preconfirmed)
                .ok_or(crate::Error::Stopped),
        }
    }

    pub(crate) async fn for_anchor(
        &self,
        mut tips: watch::Receiver<u64>,
        finalized: &watch::Receiver<u64>,
        anchored: u64,
    ) -> Result<Arc<View<E>>> {
        loop {
            tips.borrow_and_update();
            let view = self.view()?;
            let applied = view.tip().height >= anchored;
            let unfinalized = anchored > *finalized.borrow();
            let judged = applied || unfinalized;
            if judged {
                return Ok(view);
            }
            drop(view);
            #[cfg(feature = "testkit")]
            self.checking_anchors.pass(anchored).await;
            tips.changed().await.map_err(|_| crate::Error::Stopped)?;
        }
    }

    pub(crate) fn withdraw(&self) {
        self.views.store(None);
        self.speculative.store(None);
    }
}

#[cfg(feature = "testkit")]
impl<E: Context> crate::Daemon<E> {
    pub fn checking_anchors(&self) -> &crate::testkit::Gate {
        &self.current.checking_anchors
    }

    pub fn finalized_blocks(&self) -> watch::Receiver<u64> {
        self.finalized.clone()
    }
}

#[derive(Clone)]
pub struct Views<E: Context> {
    published: Arc<Current<E>>,
    clock: E,
}

impl<E: Context> Views<E> {
    pub(crate) fn new(published: Arc<Current<E>>, clock: E) -> Views<E> {
        Views { published, clock }
    }

    pub fn now(&self) -> Option<Arc<View<E>>> {
        self.published.views.load_full()
    }

    pub(crate) fn time(&self) -> u64 {
        crate::millis(&self.clock)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::Future;
    use std::sync::Arc;

    use abi::role::{self, halo, validators};
    use commonware_cryptography::{Signer as _, ed25519};
    use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
    use consensus::{Network, Timeouts};
    use fixture_probe::Step;
    use fixture_wasm::{ACCESS, AWAITS, BLOBS, HALO, IDENTITY, PROBE, PROGRAMS, VALSET};
    use host::{Founding, Genesis, Limits};
    use node::{Mempool, Node};
    use tokio::sync::watch;

    use super::{Facts, View};
    use crate::workspace::Descriptor;

    pub(crate) const NETWORK: &[u8] = b"net";
    pub(crate) const TIME: u64 = 1_700_000_000;
    const NAME: &str = "net";
    const EPOCH_LENGTH: u64 = 2;
    const BLOCK_TIME_MS: u64 = 1_000;

    pub(crate) struct Founded {
        pub(crate) first: Arc<View<runtime::Context>>,
        pub(crate) next: Arc<View<runtime::Context>>,
        pub(crate) mempool: Mempool<runtime::Context>,
        pub(crate) key: ed25519::PrivateKey,
    }

    fn founding(program: &str, code: &[u8], params: Vec<u8>, roles: &[&str]) -> Founding {
        Founding {
            program: program.to_owned(),
            code: code.to_vec(),
            params,
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        }
    }

    fn genesis(key: &ed25519::PrivateKey) -> Genesis {
        let lanes = halo::Genesis { lanes: Vec::new() };
        let validator = validators::Member {
            key: key.public_key().as_ref().to_vec(),
            address: Some("v1:1".to_owned()),
            halo: "v1:1".to_owned(),
        };
        Genesis {
            network: NETWORK.to_vec(),
            validators: vec![validator],
            programs: vec![
                founding("programs", PROGRAMS, Vec::new(), &[role::PROGRAMS]),
                founding("valset", VALSET, Vec::new(), &[role::VALIDATORS]),
                founding(
                    "identity",
                    IDENTITY,
                    abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
                    &[role::IDENTITY],
                ),
                founding("blobs", BLOBS, Vec::new(), &[role::BLOBS]),
                founding("awaits", AWAITS, Vec::new(), &[role::AWAITS]),
                founding("halo", HALO, abi::encode(&lanes), &[role::HALO]),
                founding("access", ACCESS, Vec::new(), &[role::ACCESS]),
                founding("probe", PROBE, abi::encode(&Vec::<Step>::new()), &[]),
            ],
            views: Vec::new(),
            hooks: Vec::new(),
            limits: Limits::default(),
            epoch_length: EPOCH_LENGTH,
            time: TIME,
            member_cap: 1,
            queries: validators::QueryPolicy::Signed,
        }
    }

    pub(crate) fn founded<F, R>(test: F)
    where
        F: FnOnce(Founded) -> R,
        R: Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let config = runtime::Config::new().with_storage_directory(path.join("runtime"));
        runtime::Runner::new(config).start(|context| async move {
            let key = ed25519::PrivateKey::from_seed(1);
            let found = Node::found(context.child("node"), NAME, &path, genesis(&key)).await;
            let (mut node, origin, _) = found.unwrap();
            let descriptor = Descriptor {
                network: NAME.to_owned(),
                time: TIME,
                block_time_ms: BLOCK_TIME_MS,
            };
            let network = Network {
                epoch_length: EPOCH_LENGTH,
                cadence: descriptor.cadence(Timeouts::default()).unwrap(),
            };
            let identity = key.public_key().as_ref().to_vec();
            let (readers, _) = watch::channel(());
            let facts = Facts {
                descriptor: &descriptor,
                network: &network,
                identity: &identity,
                readers: &readers,
            };
            let first = Arc::new(View::of(&node, &facts, None).unwrap());
            let verified = node.verified();
            let mempool = Mempool::new(context.child("mempool"), verified, first.snapshot());
            let mut mempool = mempool.unwrap();
            let block = mempool.build(origin.tip(), std::iter::empty(), TIME + BLOCK_TIME_MS);
            node.apply(&block).await.unwrap();
            let next = Arc::new(View::of(&node, &facts, None).unwrap());
            test(Founded {
                first,
                next,
                mempool,
                key,
            })
            .await;
        });
    }
}
