//! One `Crosspond` egress request answered by this node: the packets its network owes a linked
//! network are carried there as `Header` and `DeliverProven` frames with this network's proven
//! low-water mark, and the answer holds that network's settled rows with their op proofs. Called
//! through `call.rs` by the egress worker in `mod.rs`.
use std::collections::BTreeSet;
use std::sync::Arc;

use abi::role;
use abi::role::crosspond::{
    Acked, Link, Op, Packet, Proven, ProvenLow, Query, Receipt, Reply, low_key,
};
use abi::role::egress::{self, Answer};
use abi::{NetworkId, Origin, ProgramId, Refusal, Scan, reason};
use commonware_cryptography::Digestible as _;
use commonware_cryptography::ed25519::PrivateKey;
use consensus::MarshalMailbox;
use futures::StreamExt as _;
use host::{Layer, Tip};
use node::{Block, Frame};
use state::Serving;

use crate::client::Client;
use crate::crosspond::{self as records, Closed, Unbuilt};
use crate::egress::call::{self, Blocks};
use crate::egress::config::Peer;
use crate::live::Network as Chain;
use crate::wire::{AcksAsk, Status};
use crate::{Context, Daemon, Error};

pub struct Relay<E: Context> {
    daemon: Arc<Daemon<E>>,
    anchors: Option<MarshalMailbox>,
    links: Vec<NetworkId>,
}

enum Stop {
    Failed(String),
    Unreached(String),
    Unclosed(u64),
}

enum Landed {
    Applied,
    Refused(Refusal),
}

impl<E: Context> Relay<E> {
    pub fn new(
        daemon: Arc<Daemon<E>>,
        anchors: Option<MarshalMailbox>,
        links: Vec<NetworkId>,
    ) -> Self {
        Relay {
            daemon,
            anchors,
            links,
        }
    }

    async fn once(&self, name: &str, peer: &Peer, link: &NetworkId) -> Result<Acked, Stop> {
        let client = Client::new(peer.base.as_str().trim_end_matches('/'));
        let status = client
            .status()
            .await
            .map_err(|error| unreached(name, error))?;
        if status.genesis != *link {
            return Err(Stop::Failed(format!(
                "the node at {} is {}, not {}",
                peer.base,
                Chain::chain_id(&status.network, &status.genesis),
                abi::hex(link)
            )));
        }
        let Some(target) = status.roles.get(role::CROSSPOND).cloned() else {
            return Err(Stop::Failed(format!(
                "{name} binds no program to its {} role",
                role::CROSSPOND
            )));
        };
        let there = There {
            name,
            client: &client,
            status: &status,
            target: &target,
            key: &self.daemon.key,
        };
        let here = self.daemon.descriptor.genesis_block().digest().0;
        let (program, owed) = self.owed(link).await?;
        let lowest = owed.first().map_or(0, |packet| packet.seq);
        let row = there.link(&here).await?;
        let next_in = row.as_ref().map_or(lowest, |row| row.next_in);
        let unsent: Vec<Packet> = owed
            .iter()
            .filter(|packet| packet.seq >= next_in)
            .cloned()
            .collect();
        let mut held = row.as_ref().and_then(|row| row.header);
        if !unsent.is_empty() {
            let view = self.daemon.view().map_err(stopped)?;
            let serving = |program: &str| view.sync().serving(program).cloned();
            let (closed, first) = self.covering(&serving, &program, &unsent, held).await?;
            self.header(&there, &here, row.as_ref(), &closed).await?;
            held = Some(closed.epoch);
            let low = self.low(&serving, &closed, &program, link).await?;
            let prover = Prover {
                serving: &serving,
                closed: &closed,
                program: &program,
            };
            let rest = unsent.into_iter().skip(1);
            let frame = packed(&there, &here, low, first, rest, &prover).await?;
            match there.land(&frame).await? {
                Landed::Applied => {}
                Landed::Refused(refusal) => return Err(Stop::Failed(sentence(&refusal))),
            }
        }
        let held_there = self.header_held(&program, link).await?;
        let acked = there.acks(&here, &owed, held_there).await?;
        let acked = self.fitted(acked)?;
        if let Some(epoch) = held {
            let links = self.links.iter().copied();
            if let Err(error) = self.daemon.crosspond.held(*link, epoch, links) {
                tracing::warn!(
                    target: "ducktape::crosspond",
                    event = "closes_kept",
                    epoch,
                    %error,
                    "this node keeps the epoch-close records below epoch {epoch}: dropping them failed"
                );
            }
        }
        Ok(acked)
    }

    async fn asked_here(&self, program: &ProgramId, query: &Query) -> Result<Reply, Stop> {
        let view = self.daemon.view().map_err(stopped)?;
        let source = view.source();
        let now = self.daemon.now();
        let answer = source
            .query(
                Layer::Confirmed,
                now,
                Origin::System,
                program,
                abi::encode(query),
            )
            .await
            .map_err(|error| Stop::Failed(error.to_string()))?
            .map_err(|refusal| Stop::Failed(sentence(&refusal)))?;
        abi::decode(&answer).map_err(|refusal| Stop::Failed(sentence(&refusal)))
    }

    async fn header_held(
        &self,
        program: &ProgramId,
        link: &NetworkId,
    ) -> Result<Option<u64>, Stop> {
        match self
            .asked_here(program, &Query::Link { network: *link })
            .await?
        {
            Reply::Link(row) => Ok(row.and_then(|row| row.header)),
            _ => Err(Stop::Failed(format!(
                "{program} answered Link with something else"
            ))),
        }
    }

    async fn low<F>(
        &self,
        serving: &F,
        closed: &Closed,
        program: &str,
        link: &NetworkId,
    ) -> Result<Option<ProvenLow>, Stop>
    where
        F: Fn(&str) -> Option<Arc<Serving<E>>>,
    {
        let view = self.daemon.view().map_err(stopped)?;
        let mut prefix = low_key(link, 0);
        prefix.truncate(prefix.len() - size_of::<u64>());
        let rows = view
            .source()
            .view(Layer::Confirmed)
            .scan(program, &Scan::prefix(&prefix))
            .map_err(|error| Stop::Failed(error.to_string()))?;
        let newest = rows.iter().rev().find_map(|row| {
            let n = row.key.strip_prefix(prefix.as_slice())?;
            Some(u64::from_be_bytes(n.try_into().ok()?))
        });
        let Some(n) = newest else {
            return Ok(None);
        };
        records::low(serving, closed, program, link, n)
            .await
            .map_err(|unbuilt| Stop::Failed(unbuilt.to_string()))
    }

    fn fitted(&self, mut acked: Acked) -> Result<Acked, Stop> {
        let view = self.daemon.view().map_err(stopped)?;
        let Some(egress) = view.source().roles().get(role::EGRESS).cloned() else {
            return Err(Stop::Failed(format!(
                "this network binds no program to its {} role",
                role::EGRESS
            )));
        };
        let widest = Tip {
            height: u64::MAX - 1,
            id: [0; 32],
        };
        let network = self.daemon.descriptor.id();
        let carried = |acked: &Acked| {
            let answer = egress::Op::Answer {
                id: u64::MAX,
                answer: Answer::Output(abi::encode(acked)),
            };
            let frame = Frame::sign(
                &self.daemon.key,
                &network,
                widest,
                None,
                &egress,
                abi::encode(&answer),
            );
            Block::carries_length(frame.encode().len())
        };
        while !carried(&acked) {
            if acked.acks.pop().is_none() {
                return Err(Stop::Failed(
                    "the header of the acks is past what one answer carries".to_owned(),
                ));
            }
        }
        Ok(acked)
    }

    async fn owed(&self, link: &NetworkId) -> Result<(ProgramId, Vec<Packet>), Stop> {
        let view = self.daemon.view().map_err(stopped)?;
        let Some(program) = view.source().roles().get(role::CROSSPOND).cloned() else {
            return Err(Stop::Failed(format!(
                "this network binds no program to its {} role",
                role::CROSSPOND
            )));
        };
        let asked = Query::Outbound {
            to: *link,
            after: 0,
        };
        match self.asked_here(&program, &asked).await? {
            Reply::Outbound(packets) => Ok((program, packets)),
            _ => Err(Stop::Failed(format!(
                "{program} answered Outbound with something else"
            ))),
        }
    }

    async fn covering<F>(
        &self,
        serving: &F,
        program: &str,
        unsent: &[Packet],
        held: Option<u64>,
    ) -> Result<(Arc<Closed>, Proven), Stop>
    where
        F: Fn(&str) -> Option<Arc<Serving<E>>>,
    {
        let tip = *self.daemon.tips.borrow();
        let network = &self.daemon.network;
        let Some(first) = unsent.first() else {
            return Err(Stop::Unclosed(tip));
        };
        for closed in self.daemon.crosspond.newest_first() {
            let older = held.is_some_and(|held| closed.epoch < held);
            if older {
                break;
            }
            let unfinal = tip < network.first(closed.epoch);
            if unfinal {
                continue;
            }
            let prover = Prover {
                serving,
                closed: &closed,
                program,
            };
            if let Some(proven) = prover.prove(first.clone()).await? {
                return Ok((closed, proven));
            }
        }
        let next_open = (tip / network.epoch_length + 1) * network.epoch_length;
        Err(Stop::Unclosed(next_open))
    }

    async fn header(
        &self,
        there: &There<'_>,
        here: &NetworkId,
        row: Option<&Link>,
        closed: &Closed,
    ) -> Result<(), Stop> {
        let held = row.and_then(|row| row.header);
        if held >= Some(closed.epoch) {
            return Ok(());
        }
        let refusal = match self.land_header(there, here, closed, None).await? {
            Landed::Applied => return Ok(()),
            Landed::Refused(refusal) => refusal,
        };
        let one_third = refusal.reason == reason::NOT_YET;
        let Some(row) = row.filter(|_| one_third) else {
            return Err(Stop::Failed(sentence(&refusal)));
        };
        for epoch in row.trusted.saturating_add(1)..closed.epoch {
            let Some(between) = self.daemon.crosspond.of(epoch) else {
                continue;
            };
            let landed = self.land_header(there, here, &between, None).await?;
            if let Landed::Refused(refusal) = landed {
                return Err(Stop::Failed(sentence(&refusal)));
            }
        }
        let again = Some(closed.epoch);
        match self.land_header(there, here, closed, again).await? {
            Landed::Applied => Ok(()),
            Landed::Refused(refusal) => Err(Stop::Failed(sentence(&refusal))),
        }
    }

    async fn land_header(
        &self,
        there: &There<'_>,
        here: &NetworkId,
        closed: &Closed,
        salt: Option<u64>,
    ) -> Result<Landed, Stop> {
        let view = self.daemon.view().map_err(stopped)?;
        let serving = |program: &str| view.sync().serving(program).cloned();
        let Some(anchors) = &self.anchors else {
            return Err(Stop::Failed(
                "this node runs no consensus, so it builds no header to relay".to_owned(),
            ));
        };
        let header = records::header(anchors, &self.daemon.network, serving, closed)
            .await
            .map_err(|unbuilt| Stop::Failed(unbuilt.to_string()))?;
        let op = Op::Header {
            from: *here,
            header,
        };
        there.land(&there.sign(&op, salt)).await
    }
}

async fn reach(blocks: &mut impl Blocks, height: u64) -> bool {
    while blocks.tip() < height {
        if !blocks.changed().await {
            return false;
        }
    }
    true
}

impl<E: Context> call::Crosspond for Relay<E> {
    async fn relay(
        &mut self,
        name: &str,
        peer: &Peer,
        link: &NetworkId,
        blocks: &mut (impl Blocks + Send),
    ) -> Answer {
        loop {
            let stop = match self.once(name, peer, link).await {
                Ok(acked) => return Answer::Output(abi::encode(&acked)),
                Err(stop) => stop,
            };
            let waited = match stop {
                Stop::Failed(reason) => return Answer::Failed(reason),
                Stop::Unreached(reason) => match call::again(blocks).await {
                    true => true,
                    false => return Answer::Failed(reason),
                },
                Stop::Unclosed(height) => reach(blocks, height).await,
            };
            if !waited {
                return Answer::Failed(format!("this node stopped before it relayed to {name}"));
            }
        }
    }
}

struct There<'a> {
    name: &'a str,
    client: &'a Client,
    status: &'a Status,
    target: &'a ProgramId,
    key: &'a PrivateKey,
}

impl There<'_> {
    fn sign(&self, op: &Op, salt: Option<u64>) -> Frame {
        frame(
            self.key,
            &self.status.network,
            self.status.anchor(),
            salt,
            self.target,
            abi::encode(op),
        )
    }

    async fn ask(&self, query: &Query) -> Result<Reply, Stop> {
        let frame = Frame::sign_query(
            self.key,
            self.status.network.as_bytes(),
            self.status.anchor(),
            self.target,
            abi::encode(query),
        );
        let answer = self.client.query(Layer::Confirmed, frame.encode()).await;
        let bytes = match answer {
            Ok(bytes) => bytes,
            Err(Error::Refused(refusal)) => return Err(Stop::Failed(sentence(&refusal))),
            Err(error) => return Err(unreached(self.name, error)),
        };
        abi::decode(&bytes).map_err(|refusal| Stop::Failed(sentence(&refusal)))
    }

    async fn link(&self, here: &NetworkId) -> Result<Option<Link>, Stop> {
        match self.ask(&Query::Link { network: *here }).await? {
            Reply::Link(link) => Ok(link),
            _ => Err(Stop::Failed(format!(
                "{}'s {} answered Link with something else",
                self.name, self.target
            ))),
        }
    }

    async fn receipts(&self, here: &NetworkId, after: u64) -> Result<Vec<Receipt>, Stop> {
        let query = Query::Receipts { from: *here, after };
        match self.ask(&query).await? {
            Reply::Receipts(receipts) => Ok(receipts),
            _ => Err(Stop::Failed(format!(
                "{}'s {} answered Receipts with something else",
                self.name, self.target
            ))),
        }
    }

    async fn land(&self, frame: &Frame) -> Result<Landed, Stop> {
        match self.client.land(frame).await {
            Ok((_, receipt)) => match receipt.outcome {
                abi::Outcome::Applied { .. } => Ok(Landed::Applied),
                abi::Outcome::Rejected(refusal) => Ok(Landed::Refused(refusal)),
            },
            Err(Error::Refused(refusal)) if refusal.reason == reason::ANCHOR => {
                Err(Stop::Unreached(sentence(&refusal)))
            }
            Err(Error::Refused(refusal)) => Ok(Landed::Refused(refusal)),
            Err(error) => Err(unreached(self.name, error)),
        }
    }

    async fn acks(
        &self,
        here: &NetworkId,
        owed: &[Packet],
        held: Option<u64>,
    ) -> Result<Acked, Stop> {
        let lowest = owed.first().map_or(0, |packet| packet.seq);
        let mut heads = self
            .client
            .heads()
            .await
            .map_err(|error| unreached(self.name, error))?;
        let mut from = 0;
        loop {
            let next_in = self.link(here).await?.map_or(lowest, |row| row.next_in);
            let taken: Vec<u64> = owed
                .iter()
                .map(|packet| packet.seq)
                .filter(|seq| *seq < next_in)
                .collect();
            let receipts = self.receipts(here, lowest.saturating_sub(1)).await?;
            let receipts: Vec<Receipt> = receipts
                .into_iter()
                .filter(|receipt| taken.contains(&receipt.seq))
                .collect();
            let pending = receipts.iter().any(|receipt| receipt.result.is_none());
            let ask = AcksAsk {
                from: *here,
                seqs: taken,
                held,
            };
            let answered = match self.client.crosspond_acks(&ask).await {
                Ok(acked) => Some(acked),
                Err(Error::Refused(refusal)) if refusal.reason == reason::NOT_YET => None,
                Err(Error::Refused(refusal)) => return Err(Stop::Failed(sentence(&refusal))),
                Err(error) => return Err(unreached(self.name, error)),
            };
            if let Some(acked) = answered {
                let proven: BTreeSet<u64> = acked.acks.iter().map(|ack| ack.settled.seq).collect();
                let unproven = receipts
                    .iter()
                    .any(|receipt| !proven.contains(&receipt.seq));
                if !pending && !unproven {
                    return Ok(acked);
                }
                from = match pending {
                    true => 0,
                    false => acked
                        .epoch
                        .saturating_add(1)
                        .saturating_mul(self.status.epoch_length),
                };
            }
            loop {
                match heads.next().await {
                    Some(Ok(head)) if head.height >= from => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(unreached(self.name, error)),
                    None => return Err(unreached(self.name, Error::Closed)),
                }
            }
        }
    }
}

pub(crate) fn frame(
    key: &PrivateKey,
    network: &str,
    anchor: Tip,
    salt: Option<u64>,
    target: &str,
    payload: Vec<u8>,
) -> Frame {
    Frame::sign(key, network.as_bytes(), anchor, salt, target, payload)
}

struct Prover<'a, F> {
    serving: &'a F,
    closed: &'a Closed,
    program: &'a str,
}

impl<F> Prover<'_, F> {
    async fn prove<E: Context>(&self, packet: Packet) -> Result<Option<Proven>, Stop>
    where
        F: Fn(&str) -> Option<Arc<Serving<E>>>,
    {
        match records::proven(self.serving, self.closed, self.program, packet).await {
            Ok(proven) => Ok(Some(proven)),
            Err(Unbuilt::Unproven { .. }) => Ok(None),
            Err(unbuilt) => Err(Stop::Failed(unbuilt.to_string())),
        }
    }
}

async fn packed<E: Context, F>(
    there: &There<'_>,
    here: &NetworkId,
    low: Option<ProvenLow>,
    first: Proven,
    mut rest: impl Iterator<Item = Packet>,
    prover: &Prover<'_, F>,
) -> Result<Frame, Stop>
where
    F: Fn(&str) -> Option<Arc<Serving<E>>>,
{
    let deliver = |low: Option<ProvenLow>, packets: Vec<Proven>| Op::DeliverProven {
        from: *here,
        epoch: prover.closed.epoch,
        low,
        packets,
    };
    let head = |low: Option<ProvenLow>| there.sign(&deliver(low, Vec::new()), None).encode().len();
    let first_length = abi::encode(&first).len();
    let low = low.filter(|low| Block::carries_length(head(Some(low.clone())) + first_length));
    let mut length = head(low.clone());
    let mut packets = Vec::new();
    let mut next = Some(first);
    while let Some(proven) = next.take() {
        let grown = length + abi::encode(&proven).len();
        if !Block::carries_length(grown) {
            break;
        }
        length = grown;
        packets.push(proven);
        let Some(packet) = rest.next() else {
            break;
        };
        next = prover.prove(packet).await?;
    }
    if packets.is_empty() {
        return Err(Stop::Failed(format!(
            "one packet with its proof is past what one frame on {} carries",
            there.name
        )));
    }
    Ok(there.sign(&deliver(low, packets), None))
}

fn sentence(refusal: &Refusal) -> String {
    format!("{}: {}", refusal.reason, refusal.sentence)
}

fn unreached(name: &str, error: Error) -> Stop {
    Stop::Unreached(format!("{name} was not reached: {error}"))
}

fn stopped(error: Error) -> Stop {
    Stop::Failed(format!("this node cannot read its state: {error}"))
}

#[cfg(test)]
mod tests;
