//! A crosspond program for the node's tests: it links networks, commits what it sends, takes a
//! header whose validators hold more than a third of the set it trusts, takes deliveries unproven,
//! keeps each header and packet it took, runs each op as a foreign user's message to its port,
//! writes each packet's settled row once its result is final, writes a low row when told, and keeps
//! each low-water proof a delivery carried.
use abi::NetworkId;
use borsh::{BorshDeserialize, BorshSerialize};

pub const SETTLE: u8 = 0xff;
pub const LOW: u8 = 0xfe;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Settle {
    pub from: NetworkId,
    pub seq: u64,
    pub output: Vec<u8>,
}

pub fn settle(settle: &Settle) -> Vec<u8> {
    [vec![SETTLE], abi::encode(settle)].concat()
}

pub fn low(to: &NetworkId, n: u64) -> Vec<u8> {
    [vec![LOW], abi::encode(&(*to, n))].concat()
}

pub fn link_key(network: &NetworkId) -> Vec<u8> {
    [b"link/".as_slice(), network].concat()
}

pub fn trusted_key(network: &NetworkId) -> Vec<u8> {
    [b"trusted/".as_slice(), network].concat()
}

pub fn header_key(from: &NetworkId, epoch: u64) -> Vec<u8> {
    [b"header/".as_slice(), from, &epoch.to_be_bytes()].concat()
}

pub fn proven_key(from: &NetworkId, seq: u64) -> Vec<u8> {
    [b"proven/".as_slice(), from, &seq.to_be_bytes()].concat()
}

pub fn lowered_key(from: &NetworkId, n: u64) -> Vec<u8> {
    [b"lowered/".as_slice(), from, &n.to_be_bytes()].concat()
}

pub fn receipts_of(from: &NetworkId) -> Vec<u8> {
    [b"receipt/".as_slice(), from].concat()
}

pub fn receipt_key(from: &NetworkId, seq: u64) -> Vec<u8> {
    [receipts_of(from), seq.to_be_bytes().to_vec()].concat()
}

pub fn certified_epoch(certificate: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for (index, byte) in certificate.iter().enumerate().take(10) {
        let bits = u64::from(byte & 0x7f);
        value |= bits.checked_shl(7 * index as u32)?;
        let last = byte & 0x80 == 0;
        if last {
            return Some(value);
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::crosspond::{
        Header, Link, Op, Packet, Proven, Query as Ask, Receipt, Reply, Settled, commitment_key,
        low_key, settled_key,
    };
    use abi::{Cause, Env, Foreign, Message, NetworkId, Outcome, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{
        LOW, SETTLE, Settle, certified_epoch, header_key, link_key, lowered_key, proven_key,
        receipt_key, receipts_of, trusted_key,
    };

    const HOME: &[u8] = b"home";

    type Result<T> = core::result::Result<T, Refusal>;
    type Kept = Option<core::result::Result<Vec<u8>, Refusal>>;

    struct Crosspond;

    fn seq_of(prefix: &[u8], key: &[u8]) -> Option<u64> {
        let seq = key.strip_prefix(prefix)?;
        Some(u64::from_be_bytes(seq.try_into().ok()?))
    }

    fn item_key(source: &str, item: u64) -> Vec<u8> {
        [b"item/", source.as_bytes(), b"/", &item.to_be_bytes()].concat()
    }

    fn link(ctx: &impl Reads, network: &NetworkId) -> Result<Link> {
        ctx.record(link_key(network))?.ok_or_else(|| {
            Refusal::new(
                reason::NOT_FOUND,
                format!("no link to {}", abi::hex(network)),
            )
        })
    }

    fn home(ctx: &impl Reads) -> Result<NetworkId> {
        ctx.record(HOME)?
            .ok_or_else(|| Refusal::new(reason::WRONG_STATE, "no home network"))
    }

    fn send(ctx: &mut Execute, env: &Env, to: NetworkId, port: String, op: Vec<u8>) -> Result<()> {
        let mut row = link(ctx, &to)?;
        let packet = Packet {
            source: home(ctx)?,
            destination: to,
            seq: row.next_out,
            from: 0,
            name: String::new(),
            via: None,
            port,
            op,
            sent: env.time,
            deadline: None,
        };
        ctx.put(commitment_key(&to, packet.seq), &packet);
        row.next_out += 1;
        ctx.put(link_key(&to), &row);
        ctx.output(abi::encode(&packet.seq));
        Ok(())
    }

    fn header(ctx: &mut Execute, from: NetworkId, header: Header) -> Result<()> {
        let mut row = link(ctx, &from)?;
        let epoch = certified_epoch(&header.certificate)
            .ok_or_else(|| Refusal::new(reason::INVALID_INPUT, "the certificate names no epoch"))?;
        let trusted: Vec<Vec<u8>> = ctx.record(trusted_key(&from))?.unwrap_or_default();
        let overlap = header
            .validators
            .iter()
            .filter(|key| trusted.contains(key))
            .count();
        if overlap * 3 <= trusted.len() {
            return Err(Refusal::new(
                reason::NOT_YET,
                format!(
                    "{overlap} of the {} validators trusted at epoch {} seat epoch {epoch}",
                    trusted.len(),
                    row.trusted
                ),
            ));
        }
        ctx.put(trusted_key(&from), &header.validators);
        ctx.put(header_key(&from, epoch), &header);
        row.trusted = epoch;
        row.header = Some(epoch);
        ctx.put(link_key(&from), &row);
        Ok(())
    }

    fn deliver(
        ctx: &mut Execute,
        from: NetworkId,
        epoch: u64,
        acked: u64,
        packets: Vec<Proven>,
    ) -> Result<()> {
        let mut row = link(ctx, &from)?;
        if row.header != Some(epoch) {
            return Err(Refusal::new(
                reason::STALE,
                format!(
                    "the link holds the header of epoch {:?}, not {epoch}",
                    row.header
                ),
            ));
        }
        let prefix = receipts_of(&from);
        let kept: Vec<(Vec<u8>, Kept)> = ctx.records(Scan::prefix(prefix.clone()))?;
        for (key, _) in kept {
            let Some(seq) = seq_of(&prefix, &key).filter(|seq| *seq < acked) else {
                continue;
            };
            ctx.delete(key);
            ctx.delete(settled_key(&from, seq));
        }
        for proven in packets {
            let seq = proven.packet.seq;
            if seq < row.next_in {
                continue;
            }
            if seq > row.next_in {
                return Err(Refusal::new(
                    reason::NOT_YET,
                    format!("packet {seq} is ahead of packet {}", row.next_in),
                ));
            }
            row.next_in += 1;
            ctx.put(proven_key(&from, seq), &proven);
            ctx.put(receipt_key(&from, seq), &Kept::None);
            let Packet {
                from: account,
                port,
                op,
                ..
            } = proven.packet;
            let later = port == "later";
            if later {
                continue;
            }
            let message = Message {
                target: port,
                payload: op,
                reply: true,
                acting: None,
            };
            let foreign = Foreign {
                network: from,
                account,
            };
            let item = ctx.emit_foreign(foreign, message);
            ctx.put(item_key(&item.source, item.item), &(from, seq));
        }
        ctx.put(link_key(&from), &row);
        Ok(())
    }

    fn completed(ctx: &mut Execute, source: &str, item: u64, outcome: &Outcome) -> Result<()> {
        let Some((from, seq)) = ctx.record::<(NetworkId, u64)>(item_key(source, item))? else {
            return Ok(());
        };
        let result = match outcome {
            Outcome::Applied { output } => Ok(output.clone()),
            Outcome::Rejected(refusal) => Err(refusal.clone()),
        };
        settled(ctx, from, seq, result)
    }

    fn settled(
        ctx: &mut Execute,
        from: NetworkId,
        seq: u64,
        result: core::result::Result<Vec<u8>, Refusal>,
    ) -> Result<()> {
        let proven: Proven = ctx.record(proven_key(&from, seq))?.ok_or_else(|| {
            Refusal::new(
                reason::NOT_FOUND,
                format!("this fixture took no packet {seq}"),
            )
        })?;
        let row = Settled {
            source: from,
            destination: home(ctx)?,
            seq,
            packet: ctx.sha256(abi::encode(&proven.packet)),
            result: result.clone(),
        };
        ctx.put(receipt_key(&from, seq), &Some(result));
        ctx.put(settled_key(&from, seq), &row);
        Ok(())
    }

    impl Program for Crosspond {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<()> {
            let home: NetworkId = abi::decode(params)?;
            ctx.put(HOME, &home);
            Ok(())
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<()> {
            match &env.cause {
                Cause::Completion { item, outcome } => {
                    return completed(ctx, &item.source, item.item, outcome);
                }
                Cause::Message(_) => {
                    ctx.output(payload.to_vec());
                    return Ok(());
                }
                _ => {}
            }
            if let Some((&SETTLE, rest)) = payload.split_first() {
                let Settle { from, seq, output } = abi::decode(rest)?;
                return settled(ctx, from, seq, Ok(output));
            }
            if let Some((&LOW, rest)) = payload.split_first() {
                let (to, n): (NetworkId, u64) = abi::decode(rest)?;
                ctx.put(low_key(&to, n), &());
                return Ok(());
            }
            match abi::decode(payload)? {
                Op::Open {
                    network,
                    name,
                    epoch_length,
                    epoch,
                    validators,
                    ..
                } => {
                    ctx.put(trusted_key(&network), &validators);
                    let row = Link {
                        name,
                        epoch_length,
                        trusted: epoch,
                        header: None,
                        next_in: 1,
                        next_out: 1,
                        refused: None,
                        closed: false,
                    };
                    ctx.put(link_key(&network), &row);
                    Ok(())
                }
                Op::Send { to, port, op, .. } => send(ctx, env, to, port, op),
                Op::Header {
                    from,
                    header: given,
                } => header(ctx, from, given),
                Op::DeliverProven {
                    from,
                    epoch,
                    low,
                    packets,
                } => {
                    let acked = low.as_ref().map_or(0, |low| low.n);
                    if let Some(low) = low {
                        ctx.put(lowered_key(&from, low.n), &(epoch, low));
                    }
                    deliver(ctx, from, epoch, acked, packets)
                }
                _ => Err(Refusal::new(
                    reason::UNSUPPORTED,
                    "this fixture opens, sends, takes headers and delivers",
                )),
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<()> {
            let reply = match abi::decode(request)? {
                Ask::Link { network } => Reply::Link(ctx.record(link_key(&network))?),
                Ask::Outbound { to, after } => {
                    let mut prefix = commitment_key(&to, 0);
                    prefix.truncate(prefix.len() - size_of::<u64>());
                    let out: Vec<(Vec<u8>, Packet)> = ctx.records(Scan::prefix(prefix))?;
                    let packets = out
                        .into_iter()
                        .map(|(_, packet)| packet)
                        .filter(|packet| packet.seq > after)
                        .collect();
                    Reply::Outbound(packets)
                }
                Ask::Receipts { from, after } => {
                    let prefix = receipts_of(&from);
                    let kept: Vec<(Vec<u8>, Kept)> = ctx.records(Scan::prefix(prefix.clone()))?;
                    let receipts = kept
                        .into_iter()
                        .filter_map(|(key, result)| {
                            let seq = seq_of(&prefix, &key)?;
                            Some(Receipt { seq, result })
                        })
                        .filter(|receipt| receipt.seq > after)
                        .collect();
                    Reply::Receipts(receipts)
                }
                _ => {
                    return Err(Refusal::new(
                        reason::UNSUPPORTED,
                        "this fixture answers links, outbound packets and receipts",
                    ));
                }
            };
            ctx.reply(&reply);
            Ok(())
        }
    }

    program::program!(Crosspond);
}
