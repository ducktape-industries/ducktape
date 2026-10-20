// The node's read views: its state pinned after the last block it applied, with that moment's
// status encoded once, its commitments' sync readers, and the pool's preconfirmed layer over it.
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
    pub(crate) fn of(node: &Node<E>, facts: &Facts<'_>) -> Result<View<E>> {
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
            sync: Arc::new(statesync::Published::of(node)?),
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
}

impl<E: Context> Current<E> {
    pub(crate) fn new() -> Current<E> {
        Current {
            views: ArcSwapOption::empty(),
            speculative: ArcSwapOption::empty(),
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

    pub(crate) fn withdraw(&self) {
        self.views.store(None);
        self.speculative.store(None);
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
