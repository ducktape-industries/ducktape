//! Proxy stream framing.
//!
//! The old one-shot `[status][len][head|body]` response is replaced by a small
//! frame stream so the publisher can stream a response body, run Server-Sent
//! Events, or tunnel a WebSocket over one authenticated mesh connection. This
//! is a **sans-io** codec: `encode_frame` produces a self-delimiting frame and
//! `decode_frame` parses one from a buffer, returning [`FrameError::Incomplete`]
//! when more bytes are needed. The async read/write loop lives in the node.
//!
//! Wire: `[type:u8][len:u32 BE][payload]`. Head and failure payloads are JSON;
//! body/ws payloads are raw bytes.

use serde::{Deserialize, Serialize};

use crate::ProxyResponseHead;

pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_WS_FRAME_BYTES: usize = 1024 * 1024;
/// Head/failure JSON payloads are small and bounded independently of the body.
pub const MAX_FRAME_META_BYTES: usize = 8 * 1024;

const TYPE_RESPONSE_HEAD: u8 = 1;
const TYPE_BODY_CHUNK: u8 = 2;
const TYPE_END: u8 = 3;
const TYPE_WS_FRAME_TEXT: u8 = 4;
const TYPE_WS_FRAME_BINARY: u8 = 5;
const TYPE_WS_CLOSE: u8 = 6;
const TYPE_FAILURE: u8 = 7;

/// A wire-shaped failure. The node's own `GatewayFailure` maps to/from this so
/// the pure crate does not depend on node error types. Detail redaction is the
/// node's responsibility before it constructs one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProxyFailure {
    pub kind: FailureKind,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Invalid,
    Forbidden,
    NotFound,
    Unavailable,
    Conflict,
    /// The body went past what the route admits. Its own kind because it is
    /// the one refusal that arrives MID-BODY — a head declares no length, so
    /// there is nothing to refuse before the bytes start.
    TooLarge,
}

/// One frame in either direction of a proxy stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyFrame {
    /// First publisher→caller frame: status + response headers.
    ResponseHead(ProxyResponseHead),
    /// A body chunk, either direction.
    BodyChunk(Vec<u8>),
    /// Clean end of a body in one direction.
    End,
    /// A WebSocket message after an accepted upgrade.
    WsFrame { binary: bool, payload: Vec<u8> },
    /// A WebSocket close with its status code.
    WsClose { code: u16 },
    /// A terminal failure in place of a response.
    Failure(ProxyFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The buffer does not yet hold a full frame; read more and retry.
    Incomplete,
    /// The frame is structurally invalid — the stream must be dropped.
    Malformed(String),
}

pub fn encode_frame(frame: &ProxyFrame) -> Result<Vec<u8>, String> {
    let (kind, payload): (u8, Vec<u8>) = match frame {
        ProxyFrame::ResponseHead(head) => (
            TYPE_RESPONSE_HEAD,
            serde_json::to_vec(head).map_err(|error| error.to_string())?,
        ),
        ProxyFrame::BodyChunk(bytes) => {
            if bytes.len() > MAX_CHUNK_BYTES {
                return Err(format!("frame: chunk exceeds {MAX_CHUNK_BYTES} bytes"));
            }
            (TYPE_BODY_CHUNK, bytes.clone())
        }
        ProxyFrame::End => (TYPE_END, Vec::new()),
        ProxyFrame::WsFrame { binary, payload } => {
            if payload.len() > MAX_WS_FRAME_BYTES {
                return Err(format!(
                    "frame: ws frame exceeds {MAX_WS_FRAME_BYTES} bytes"
                ));
            }
            let kind = if *binary {
                TYPE_WS_FRAME_BINARY
            } else {
                TYPE_WS_FRAME_TEXT
            };
            (kind, payload.clone())
        }
        ProxyFrame::WsClose { code } => (TYPE_WS_CLOSE, code.to_be_bytes().to_vec()),
        ProxyFrame::Failure(failure) => (
            TYPE_FAILURE,
            serde_json::to_vec(failure).map_err(|error| error.to_string())?,
        ),
    };
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Parse one frame from the front of `buf`, returning it and the number of
/// bytes consumed. [`FrameError::Incomplete`] means `buf` is a prefix of a
/// frame; the caller reads more bytes and retries with the same buffer.
pub fn decode_frame(buf: &[u8]) -> Result<(ProxyFrame, usize), FrameError> {
    if buf.len() < 5 {
        return Err(FrameError::Incomplete);
    }
    let kind = buf[0];
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    let bound = match kind {
        TYPE_RESPONSE_HEAD | TYPE_FAILURE => MAX_FRAME_META_BYTES,
        TYPE_BODY_CHUNK => MAX_CHUNK_BYTES,
        TYPE_WS_FRAME_TEXT | TYPE_WS_FRAME_BINARY => MAX_WS_FRAME_BYTES,
        TYPE_END | TYPE_WS_CLOSE => MAX_FRAME_META_BYTES,
        other => return Err(FrameError::Malformed(format!("unknown frame type {other}"))),
    };
    if len > bound {
        return Err(FrameError::Malformed(format!(
            "frame type {kind} length {len} exceeds {bound}"
        )));
    }
    let end = 5 + len;
    if buf.len() < end {
        return Err(FrameError::Incomplete);
    }
    let payload = &buf[5..end];
    let frame = match kind {
        TYPE_RESPONSE_HEAD => ProxyFrame::ResponseHead(
            serde_json::from_slice(payload).map_err(|e| FrameError::Malformed(e.to_string()))?,
        ),
        TYPE_BODY_CHUNK => ProxyFrame::BodyChunk(payload.to_vec()),
        TYPE_END => {
            if len != 0 {
                return Err(FrameError::Malformed("end frame must be empty".into()));
            }
            ProxyFrame::End
        }
        TYPE_WS_FRAME_TEXT => ProxyFrame::WsFrame {
            binary: false,
            payload: payload.to_vec(),
        },
        TYPE_WS_FRAME_BINARY => ProxyFrame::WsFrame {
            binary: true,
            payload: payload.to_vec(),
        },
        TYPE_WS_CLOSE => {
            if len != 2 {
                return Err(FrameError::Malformed("ws close needs a 2-byte code".into()));
            }
            ProxyFrame::WsClose {
                code: u16::from_be_bytes([payload[0], payload[1]]),
            }
        }
        TYPE_FAILURE => ProxyFrame::Failure(
            serde_json::from_slice(payload).map_err(|e| FrameError::Malformed(e.to_string()))?,
        ),
        _ => unreachable!("bound match already rejected unknown types"),
    };
    Ok((frame, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProxyHeader;

    fn head() -> ProxyResponseHead {
        ProxyResponseHead {
            status: 200,
            headers: vec![ProxyHeader {
                name: "content-type".into(),
                value: "text/event-stream".into(),
            }],
        }
    }

    fn roundtrip(frame: ProxyFrame) {
        let encoded = encode_frame(&frame).unwrap();
        let (decoded, consumed) = decode_frame(&encoded).unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn every_variant_round_trips() {
        roundtrip(ProxyFrame::ResponseHead(head()));
        roundtrip(ProxyFrame::BodyChunk(b"data: tick\n\n".to_vec()));
        roundtrip(ProxyFrame::End);
        roundtrip(ProxyFrame::WsFrame {
            binary: false,
            payload: b"ping".to_vec(),
        });
        roundtrip(ProxyFrame::WsFrame {
            binary: true,
            payload: vec![0, 1, 2, 255],
        });
        roundtrip(ProxyFrame::WsClose { code: 1000 });
        roundtrip(ProxyFrame::Failure(ProxyFailure {
            kind: FailureKind::Forbidden,
            detail: "audience denied".into(),
        }));
    }

    #[test]
    fn two_frames_decode_in_sequence_from_one_buffer() {
        let mut buf = encode_frame(&ProxyFrame::BodyChunk(b"one".to_vec())).unwrap();
        buf.extend(encode_frame(&ProxyFrame::End).unwrap());
        let (first, consumed) = decode_frame(&buf).unwrap();
        assert_eq!(first, ProxyFrame::BodyChunk(b"one".to_vec()));
        let (second, _) = decode_frame(&buf[consumed..]).unwrap();
        assert_eq!(second, ProxyFrame::End);
    }

    #[test]
    fn a_partial_frame_is_incomplete_not_an_error() {
        let full = encode_frame(&ProxyFrame::BodyChunk(b"hello".to_vec())).unwrap();
        assert_eq!(decode_frame(&full[..2]), Err(FrameError::Incomplete));
        assert_eq!(
            decode_frame(&full[..full.len() - 1]),
            Err(FrameError::Incomplete)
        );
    }

    #[test]
    fn unknown_type_and_oversize_chunk_are_malformed() {
        let bogus = [99u8, 0, 0, 0, 0];
        assert!(matches!(
            decode_frame(&bogus),
            Err(FrameError::Malformed(_))
        ));

        // A body-chunk header claiming more than MAX_CHUNK_BYTES is rejected on
        // sight, before any large allocation.
        let mut oversize = vec![TYPE_BODY_CHUNK];
        oversize.extend_from_slice(&((MAX_CHUNK_BYTES as u32) + 1).to_be_bytes());
        assert!(matches!(
            decode_frame(&oversize),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn encode_rejects_oversize_chunk() {
        assert!(encode_frame(&ProxyFrame::BodyChunk(vec![0; MAX_CHUNK_BYTES + 1])).is_err());
    }
}
