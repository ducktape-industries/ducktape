//! What the node records at each epoch close and opening for crosspond, and the headers, packet
//! proofs, low-water proofs and proven acks it builds from those records. Written where `run.rs`
//! applies blocks; read by the crosspond relay in `egress/crosspond.rs` and the acks route.
use std::collections::BTreeMap;
use std::sync::Arc;

use abi::role::crosspond::{
    Acked, CertificateKind, Header, Packet, Proven, ProvenAck, ProvenLow, Settled, StateProof,
    commitment_key, low_key,
};
use abi::{NetworkId, ProgramId, Root};
use arc_swap::ArcSwap;
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::Encode as _;
use commonware_consensus::types::Height;
use consensus::{MarshalMailbox, Network};
use node::Node;
use state::{OpProof, Serving};

use crate::history::History;
use crate::{Context, Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Closed {
    pub epoch: u64,
    pub height: u64,
    pub roots: Vec<(ProgramId, Root)>,
    pub sizes: BTreeMap<ProgramId, u64>,
    pub validators: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Opened {
    pub epoch: u64,
    pub block: Vec<u8>,
    pub certificate: Vec<u8>,
}

pub struct Closes {
    closed: ArcSwap<BTreeMap<u64, Arc<Closed>>>,
    opened: ArcSwap<BTreeMap<u64, Arc<Opened>>>,
    held: ArcSwap<BTreeMap<NetworkId, u64>>,
    kept: History,
}

impl Closes {
    pub(crate) fn load(kept: History) -> Result<Closes> {
        let mut closed = BTreeMap::new();
        for record in kept.closes()? {
            let record: Closed = abi::decode(&record).map_err(|refusal| {
                Error::Corrupt(format!(
                    "a kept epoch-close record does not decode: {}",
                    refusal.sentence
                ))
            })?;
            closed.insert(record.epoch, Arc::new(record));
        }
        let mut opened = BTreeMap::new();
        for record in kept.openings()? {
            let record: Opened = abi::decode(&record).map_err(|refusal| {
                Error::Corrupt(format!(
                    "a kept epoch-opening record does not decode: {}",
                    refusal.sentence
                ))
            })?;
            opened.insert(record.epoch, Arc::new(record));
        }
        Ok(Closes {
            closed: ArcSwap::from_pointee(closed),
            opened: ArcSwap::from_pointee(opened),
            held: ArcSwap::default(),
            kept,
        })
    }

    pub(crate) fn close<E: Context>(
        &self,
        node: &Node<E>,
        network: &Network,
        height: u64,
    ) -> Result<()> {
        let epoch = network.epoch_after(height);
        let store = node.host().store();
        let roots = store.roots().map_err(host::Error::from)?;
        let sizes = store
            .commitments()
            .map(|(program, commitment)| (program.clone(), commitment.serving().size()))
            .collect();
        let validators = node
            .epoch_validators(epoch)?
            .ok_or_else(|| Error::Corrupt(format!("the close of epoch {epoch} seats no one")))?;
        self.keep(Closed {
            epoch,
            height,
            roots,
            sizes,
            validators,
        })
    }

    pub(crate) fn keep(&self, closed: Closed) -> Result<()> {
        self.kept.keep_close(closed.epoch, abi::encode(&closed))?;
        let closed = Arc::new(closed);
        self.closed.rcu(|closes| {
            let mut closes = BTreeMap::clone(closes);
            closes.insert(closed.epoch, closed.clone());
            closes
        });
        Ok(())
    }

    pub(crate) fn open(&self, opened: Opened) -> Result<()> {
        let closed = self.of(opened.epoch).is_some();
        if !closed {
            return Ok(());
        }
        self.kept.keep_opening(opened.epoch, abi::encode(&opened))?;
        let opened = Arc::new(opened);
        self.opened.rcu(|openings| {
            let mut openings = BTreeMap::clone(openings);
            openings.insert(opened.epoch, opened.clone());
            openings
        });
        Ok(())
    }

    pub(crate) fn opening(&self, epoch: u64) -> Option<Arc<Opened>> {
        self.opened.load().get(&epoch).cloned()
    }

    pub(crate) fn newest_opened(&self) -> Option<(Arc<Closed>, Arc<Opened>)> {
        self.newest_first().into_iter().find_map(|closed| {
            let opened = self.opening(closed.epoch)?;
            Some((closed, opened))
        })
    }

    pub fn newest_first(&self) -> Vec<Arc<Closed>> {
        self.closed.load().values().rev().cloned().collect()
    }

    pub fn of(&self, epoch: u64) -> Option<Arc<Closed>> {
        self.closed.load().get(&epoch).cloned()
    }

    pub(crate) fn held(
        &self,
        link: NetworkId,
        epoch: u64,
        links: impl IntoIterator<Item = NetworkId>,
    ) -> Result<()> {
        self.held.rcu(|held| {
            let mut held = BTreeMap::clone(held);
            held.insert(link, epoch);
            held
        });
        let Some(floor) = floor(&self.held.load(), links) else {
            return Ok(());
        };
        self.forget_before(floor)
    }

    pub(crate) fn forget_before(&self, floor: u64) -> Result<()> {
        self.closed
            .rcu(|closes| BTreeMap::clone(closes).split_off(&floor));
        self.opened
            .rcu(|openings| BTreeMap::clone(openings).split_off(&floor));
        self.kept.forget_closes_before(floor)
    }
}

fn floor(
    held: &BTreeMap<NetworkId, u64>,
    links: impl IntoIterator<Item = NetworkId>,
) -> Option<u64> {
    let epochs: Option<Vec<u64>> = links
        .into_iter()
        .map(|link| held.get(&link).copied())
        .collect();
    epochs?.into_iter().min()
}

#[derive(Debug, thiserror::Error)]
pub enum Unbuilt {
    #[error("this node holds no block at height {0}")]
    Block(u64),
    #[error("this node holds no certificate for height {0}")]
    Certificate(u64),
    #[error("the close of epoch {0} recorded no log size for {1}")]
    Unsized(u64, ProgramId),
    #[error("this node serves no commitment of {0}")]
    Unserved(ProgramId),
    #[error("this node holds no epoch close whose opening block it recorded")]
    Unopened,
    #[error("the {program} log no longer proves {key} at size {size}")]
    Unproven {
        program: ProgramId,
        key: String,
        size: u64,
    },
    #[error(transparent)]
    State(#[from] state::Error),
}

pub async fn header<E: Context>(
    marshal: &MarshalMailbox,
    network: &Network,
    serving: impl Fn(&str) -> Option<Arc<Serving<E>>>,
    closed: &Closed,
) -> std::result::Result<Header, Unbuilt> {
    let first = network.first(closed.epoch);
    let block = marshal
        .get_block(Height::new(first))
        .await
        .ok_or(Unbuilt::Block(first))?;
    let certificate = marshal
        .get_finalization(Height::new(first))
        .await
        .ok_or(Unbuilt::Certificate(first))?;
    let opened = Opened {
        epoch: closed.epoch,
        block: block.encode().to_vec(),
        certificate: certificate.encode().to_vec(),
    };
    header_of(&serving, closed, &opened).await
}

pub async fn header_of<E: Context>(
    serving: &impl Fn(&str) -> Option<Arc<Serving<E>>>,
    closed: &Closed,
    opened: &Opened,
) -> std::result::Result<Header, Unbuilt> {
    let key = host::epoch_validators(closed.epoch);
    let value = abi::encode(&closed.validators);
    let OpProof { location, proof } = prove(serving, closed, host::NETWORK, &key, &value).await?;
    Ok(Header {
        kind: CertificateKind::Ed25519SimplexFinalize,
        block: opened.block.clone(),
        certificate: opened.certificate.clone(),
        roots: closed.roots.clone(),
        validators: closed.validators.clone(),
        validators_proof: StateProof { location, proof },
    })
}

pub async fn proven<E: Context>(
    serving: impl Fn(&str) -> Option<Arc<Serving<E>>>,
    closed: &Closed,
    program: &str,
    packet: Packet,
) -> std::result::Result<Proven, Unbuilt> {
    let key = commitment_key(&packet.destination, packet.seq);
    let value = abi::encode(&packet);
    let OpProof { location, proof } = prove(&serving, closed, program, &key, &value).await?;
    Ok(Proven {
        packet,
        proof: StateProof { location, proof },
    })
}

pub async fn low<E: Context>(
    serving: &impl Fn(&str) -> Option<Arc<Serving<E>>>,
    closed: &Closed,
    program: &str,
    to: &NetworkId,
    n: u64,
) -> std::result::Result<Option<ProvenLow>, Unbuilt> {
    match prove(serving, closed, program, &low_key(to, n), &[]).await {
        Ok(OpProof { location, proof }) => Ok(Some(ProvenLow {
            n,
            proof: StateProof { location, proof },
        })),
        Err(Unbuilt::Unproven { .. }) => Ok(None),
        Err(unbuilt) => Err(unbuilt),
    }
}

pub struct Asked<'a> {
    pub program: &'a str,
    pub held: Option<u64>,
    pub rows: Vec<(Vec<u8>, Vec<u8>)>,
}

pub async fn acked<E: Context>(
    closes: &Closes,
    serving: &impl Fn(&str) -> Option<Arc<Serving<E>>>,
    asked: Asked<'_>,
) -> std::result::Result<Acked, Unbuilt> {
    let Asked {
        program,
        held,
        rows,
    } = asked;
    let (closed, opened) = closes.newest_opened().ok_or(Unbuilt::Unopened)?;
    let header = match held == Some(closed.epoch) {
        true => None,
        false => Some(header_of(serving, &closed, &opened).await?),
    };
    let mut acks = Vec::new();
    for (key, value) in rows {
        let Ok(settled) = abi::decode::<Settled>(&value) else {
            continue;
        };
        let proof = match prove(serving, &closed, program, &key, &value).await {
            Ok(OpProof { location, proof }) => StateProof { location, proof },
            Err(Unbuilt::Unproven { .. }) => continue,
            Err(unbuilt) => return Err(unbuilt),
        };
        acks.push(ProvenAck { settled, proof });
    }
    Ok(Acked {
        epoch: closed.epoch,
        header,
        acks,
    })
}

async fn prove<E: Context>(
    serving: &impl Fn(&str) -> Option<Arc<Serving<E>>>,
    closed: &Closed,
    program: &str,
    key: &[u8],
    value: &[u8],
) -> std::result::Result<OpProof, Unbuilt> {
    let size = *closed
        .sizes
        .get(program)
        .ok_or_else(|| Unbuilt::Unsized(closed.epoch, program.to_owned()))?;
    let log = serving(program).ok_or_else(|| Unbuilt::Unserved(program.to_owned()))?;
    log.op_proof(size, key, value)
        .await?
        .ok_or_else(|| Unbuilt::Unproven {
            program: program.to_owned(),
            key: String::from_utf8_lossy(key).into_owned(),
            size,
        })
}

#[cfg(test)]
mod tests;
