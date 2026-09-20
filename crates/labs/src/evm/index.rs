//! Fluent31 read model for EVM receipts and logs.
//!
//! QMDB remains canonical EVM state. This mapper only folds the module's
//! authenticated `Receipt` follow-ups, producing ordered receipt/log views.
//! Receipt history cannot be reconstructed from current account state, so
//! after state sync its views begin at the reported backfill boundary and
//! accrue from there.
//!
//! the lab keeps the DECISION core only — pure fold + view over
//! [`StateRead`], the same shape the shipped mappers wear. wiring it as a
//! real index guest (an `index-guest` feature + `guest-builder --index`)
//! is part of graduating the experiment.

use index_guest::{Fail, OpRow, OriginKind, StateRead, Writes};
use serde::{Deserialize, Serialize};

use super::{EvmLog, EvmMsg, EvmResult, EvmTx, decode_msg};

const DEFAULT_LIMIT: usize = 50;

/// [`Fail`] code: an applied op's payload did not decode (or broke the
/// receipt-origin invariant).
const FAIL_OP_DECODE: i32 = 2;
/// [`Fail`] code: a stored row did not decode — a damaged read model.
const FAIL_ROW_DECODE: i32 = 3;
/// [`Fail`] code: a view request this mapper does not speak.
const FAIL_BAD_REQUEST: i32 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRow {
    pub height: u64,
    pub time: u64,
    pub seq: u32,
    pub caller: String,
    pub transaction: EvmTx,
    pub result: EvmResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRow {
    pub height: u64,
    pub time: u64,
    pub seq: u32,
    pub log_index: u32,
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvmViewQuery {
    Receipts {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Contract {
        address: String,
    },
    Logs {
        #[serde(default)]
        address: Option<String>,
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvmViewReply {
    Receipts {
        receipts: Vec<ReceiptRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after: Option<String>,
    },
    Contract(Option<ReceiptRow>),
    Logs {
        logs: Vec<LogRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after: Option<String>,
    },
}

fn receipt_key(height: u64, seq: u32) -> String {
    format!("receipt/{height:016x}/{seq:04x}")
}

fn contract_key(address: &[u8; 20]) -> String {
    format!("contract/{}", hex(address))
}

fn log_suffix(height: u64, seq: u32, log_index: u32) -> String {
    format!("{height:016x}/{seq:04x}/{log_index:04x}")
}

fn stage_log(out: &mut Writes, op: &OpRow, log_index: u32, log: &EvmLog) -> Result<(), Fail> {
    let address = hex(&log.address);
    let suffix = log_suffix(op.height, op.seq, log_index);
    let row = LogRow {
        height: op.height,
        time: op.time,
        seq: op.seq,
        log_index,
        address: format!("0x{address}"),
        topics: log
            .topics
            .iter()
            .map(|topic| format!("0x{}", hex(topic)))
            .collect(),
        data: format!("0x{}", hex(&log.data)),
    };
    let bytes = serde_json::to_vec(&row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    index_guest::put(out, format!("log/all/{suffix}"), bytes.clone());
    index_guest::put(
        out,
        format!("log/address/{address}/{suffix}"),
        bytes.clone(),
    );
    for topic in &log.topics {
        let topic = hex(topic);
        index_guest::put(out, format!("log/topic/{topic}/{suffix}"), bytes.clone());
        index_guest::put(
            out,
            format!("log/address-topic/{address}/{topic}/{suffix}"),
            bytes.clone(),
        );
    }
    Ok(())
}

fn decode_rows<T: for<'de> Deserialize<'de>>(
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Result<Vec<T>, Fail> {
    entries
        .iter()
        .map(|(_, value)| {
            serde_json::from_slice(value).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
        })
        .collect()
}

/// fold one applied op into derived writes. only the module's own `Receipt`
/// follow-ups materialize; every other variant is a deterministic skip.
pub fn fold_op(op: &OpRow, _read: &impl StateRead) -> Result<Writes, Fail> {
    let mut out = Writes::new();
    let EvmMsg::Receipt {
        transaction,
        caller,
        result,
    } = decode_msg(&op.payload).map_err(|e| Fail::new(FAIL_OP_DECODE, e))?
    else {
        return Ok(out);
    };
    if op.origin.kind != OriginKind::Module || op.origin.id.as_deref() != Some("evm") {
        return Err(Fail::new(
            FAIL_OP_DECODE,
            "applied EVM receipt did not originate from the EVM module",
        ));
    }

    let row = ReceiptRow {
        height: op.height,
        time: op.time,
        seq: op.seq,
        caller: format!("0x{}", hex(&caller)),
        transaction,
        result,
    };
    let bytes = serde_json::to_vec(&row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    index_guest::put(&mut out, receipt_key(op.height, op.seq), bytes.clone());
    if let Some(address) = row.result.created_address {
        index_guest::put(&mut out, contract_key(&address), bytes);
    }
    for (log_index, log) in row.result.logs.iter().enumerate() {
        stage_log(&mut out, op, log_index as u32, log)?;
    }
    Ok(out)
}

/// serve one materialized-view request.
pub fn serve_view(read: &impl StateRead, req: &[u8]) -> Result<Vec<u8>, Fail> {
    let query: EvmViewQuery =
        serde_json::from_slice(req).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))?;
    let reply = match query {
        EvmViewQuery::Receipts { after, limit } => {
            let page = read.scan_page(
                b"receipt/",
                after.as_deref().map(str::as_bytes),
                limit.unwrap_or(DEFAULT_LIMIT),
            );
            EvmViewReply::Receipts {
                receipts: decode_rows(&page.entries)?,
                has_more: page.has_more,
                next_after: page.next_after,
            }
        }
        EvmViewQuery::Contract { address } => {
            let address = fixed_hex::<20>(&address)?;
            let receipt = read
                .get(contract_key(&address).as_bytes())
                .map(|bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
                })
                .transpose()?;
            EvmViewReply::Contract(receipt)
        }
        EvmViewQuery::Logs {
            address,
            topic,
            after,
            limit,
        } => {
            let prefix = match (address, topic) {
                (Some(address), Some(topic)) => format!(
                    "log/address-topic/{}/{}/",
                    hex(&fixed_hex::<20>(&address)?),
                    hex(&fixed_hex::<32>(&topic)?),
                ),
                (Some(address), None) => {
                    format!("log/address/{}/", hex(&fixed_hex::<20>(&address)?))
                }
                (None, Some(topic)) => {
                    format!("log/topic/{}/", hex(&fixed_hex::<32>(&topic)?))
                }
                (None, None) => "log/all/".into(),
            };
            let page = read.scan_page(
                prefix.as_bytes(),
                after.as_deref().map(str::as_bytes),
                limit.unwrap_or(DEFAULT_LIMIT),
            );
            EvmViewReply::Logs {
                logs: decode_rows(&page.entries)?,
                has_more: page.has_more,
                next_after: page.next_after,
            }
        }
    };
    serde_json::to_vec(&reply).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
}

fn fixed_hex<const N: usize>(value: &str) -> Result<[u8; N], Fail> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.len() != N * 2 {
        return Err(Fail::new(
            FAIL_BAD_REQUEST,
            format!("expected {}-byte hex value", N),
        ));
    }
    let mut out = [0; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .map_err(|_| Fail::new(FAIL_BAD_REQUEST, "invalid hex value"))?;
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use index_guest::{OriginTag, apply_to_map};

    use super::*;
    use crate::evm::{EvmStatus, encode_msg};

    fn receipt() -> EvmMsg {
        EvmMsg::Receipt {
            transaction: EvmTx::Call {
                to: [7; 20],
                input: vec![1, 2, 3],
                gas_limit: 50_000,
            },
            caller: [9; 20],
            result: EvmResult {
                status: EvmStatus::Success,
                gas_used: 22_000,
                output: Vec::new(),
                created_address: Some([7; 20]),
                logs: vec![EvmLog {
                    address: [7; 20],
                    topics: vec![[8; 32]],
                    data: vec![4, 5],
                }],
            },
        }
    }

    fn view(map: &BTreeMap<Vec<u8>, Vec<u8>>, query: serde_json::Value) -> EvmViewReply {
        serde_json::from_slice(&serve_view(map, &serde_json::to_vec(&query).unwrap()).unwrap())
            .unwrap()
    }

    #[test]
    fn indexes_receipts_contracts_and_filtered_logs() {
        let mut map = BTreeMap::new();
        let writes = fold_op(
            &OpRow {
                height: 4,
                seq: 0,
                time: 44,
                origin: OriginTag::module("evm"),
                payload: encode_msg(&receipt()),
                // the evm module assigns no stamp; the fold derives everything
                // it indexes from the receipt payload.
                assigned: Vec::new(),
            },
            &map,
        )
        .unwrap();
        apply_to_map(&mut map, writes);

        let EvmViewReply::Receipts { receipts, .. } =
            view(&map, serde_json::json!({"receipts": {}}))
        else {
            panic!("receipt reply")
        };
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].caller, format!("0x{}", hex(&[9; 20])));

        let EvmViewReply::Contract(Some(contract)) = view(
            &map,
            serde_json::json!({"contract": {"address": format!("0x{}", hex(&[7; 20]))}}),
        ) else {
            panic!("contract reply")
        };
        assert_eq!(contract.height, 4);

        let EvmViewReply::Logs { logs, .. } = view(
            &map,
            serde_json::json!({"logs": {
                "address": format!("0x{}", hex(&[7; 20])),
                "topic": format!("0x{}", hex(&[8; 32]))
            }}),
        ) else {
            panic!("logs reply")
        };
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data, "0x0405");
    }
}
