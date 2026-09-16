//! Wire encoding — the cross-node surface of the plane.
//!
//! Datagram frame (9-byte header, then payload):
//!
//! ```text
//! offset  0          1..9
//!         service    flow (u64 BE)
//! ```
//!
//! Stream hello (one length-prefixed frame, opener → acceptor, answered by a
//! single [`HELLO_ACK`] byte):
//!
//! ```text
//! u16 BE len | service | flow (u64 BE) | intent | meta (len-10 bytes)
//! ```
//!
//! `intent` and `meta` are service-defined and opaque to the plane. there is
//! no version byte: the frame is a fixed shape (flag-day rule — no in-band
//! version), and the service id is the discriminant.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Service;
use crate::flow::FlowId;

/// Largest whole datagram frame. Derivation: overlay MTU 1420 (WireGuard
/// over a 1500 underlay) − 40 (IPv6) − 8 (UDP) = 1372 UDP payload bytes.
/// The plane never fragments — consumers must fit the payload bound.
pub const MAX_DATAGRAM: usize = 1372;
pub const DATAGRAM_HEADER_LEN: usize = 9;
pub const MAX_DATAGRAM_PAYLOAD: usize = MAX_DATAGRAM - DATAGRAM_HEADER_LEN;

/// Hello body length ahead of meta: service + flow + intent.
const HELLO_FIXED_LEN: usize = 10;
/// The gateway plane packs a proxied request's HEAD (method, path, and the
/// forwarded HTTP headers) into the stream Hello's `meta`. Real HTTP heads —
/// an airlock `/v1/messages` carries `anthropic-version`, `anthropic-beta`, the
/// scoped bearer, and the seal marker — run past 1.5 KiB, so a 1 KiB cap
/// silently 502'd every cross-node airlock request. Bounded by the `u16` length
/// prefix (< 64 KiB); 16 KiB is generous for any HTTP head while staying small.
pub const MAX_HELLO_META: usize = 16 * 1024;

/// The single byte an acceptor answers a hello with. Anything else (or a
/// closed stream) means the open was refused.
pub const HELLO_ACK: u8 = 0x06;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("datagram payload {len} exceeds {MAX_DATAGRAM_PAYLOAD}")]
    PayloadTooLarge { len: usize },
    #[error("hello meta {len} exceeds {MAX_HELLO_META}")]
    MetaTooLarge { len: usize },
    #[error("frame truncated")]
    Truncated,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn encode_datagram(
    service: Service,
    flow: FlowId,
    payload: &[u8],
) -> Result<Vec<u8>, WireError> {
    if payload.len() > MAX_DATAGRAM_PAYLOAD {
        return Err(WireError::PayloadTooLarge { len: payload.len() });
    }
    let mut frame = Vec::with_capacity(DATAGRAM_HEADER_LEN + payload.len());
    frame.push(service.lane_id());
    frame.extend_from_slice(&flow.as_u64().to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn decode_datagram(frame: &[u8]) -> Result<(Service, FlowId, &[u8]), WireError> {
    if frame.len() < DATAGRAM_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    let service = Service::from_lane_id(frame[0]);
    let flow = FlowId::from_raw(u64::from_be_bytes(frame[1..9].try_into().expect("8 bytes")));
    Ok((service, flow, &frame[DATAGRAM_HEADER_LEN..]))
}

/// The stream-open frame. `intent`/`meta` are the service's to define.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub service: Service,
    pub flow: FlowId,
    pub intent: u8,
    pub meta: Vec<u8>,
}

pub async fn write_hello<W: AsyncWrite + Unpin>(
    writer: &mut W,
    hello: &Hello,
) -> Result<(), WireError> {
    if hello.meta.len() > MAX_HELLO_META {
        return Err(WireError::MetaTooLarge {
            len: hello.meta.len(),
        });
    }
    let len = HELLO_FIXED_LEN + hello.meta.len();
    let mut frame = Vec::with_capacity(2 + len);
    frame.extend_from_slice(&(len as u16).to_be_bytes());
    frame.push(hello.service.lane_id());
    frame.extend_from_slice(&hello.flow.as_u64().to_be_bytes());
    frame.push(hello.intent);
    frame.extend_from_slice(&hello.meta);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Hello, WireError> {
    let mut len_bytes = [0u8; 2];
    reader.read_exact(&mut len_bytes).await?;
    let len = u16::from_be_bytes(len_bytes) as usize;
    if len < HELLO_FIXED_LEN {
        return Err(WireError::Truncated);
    }
    if len > HELLO_FIXED_LEN + MAX_HELLO_META {
        return Err(WireError::MetaTooLarge {
            len: len - HELLO_FIXED_LEN,
        });
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let service = Service::from_lane_id(body[0]);
    let flow = FlowId::from_raw(u64::from_be_bytes(body[1..9].try_into().expect("8 bytes")));
    Ok(Hello {
        service,
        flow,
        intent: body[9],
        meta: body[10..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a declared lane, as the registry would hand it over.
    const VOICE: Service = Service::from_lane_id(2);

    #[test]
    fn datagram_round_trip() {
        let flow = FlowId::derive(b"voice-channel:general");
        let frame = encode_datagram(VOICE, flow, b"opus bytes").unwrap();
        let (service, decoded_flow, payload) = decode_datagram(&frame).unwrap();
        assert_eq!(service, VOICE);
        assert_eq!(decoded_flow, flow);
        assert_eq!(payload, b"opus bytes");
    }

    #[test]
    fn datagram_rejects_oversize_and_truncation_but_not_an_unbound_lane() {
        let flow = FlowId::from_raw(7);
        let big = vec![0u8; MAX_DATAGRAM_PAYLOAD + 1];
        assert!(matches!(
            encode_datagram(VOICE, flow, &big),
            Err(WireError::PayloadTooLarge { .. })
        ));
        assert!(matches!(
            decode_datagram(&[1, 2, 3]),
            Err(WireError::Truncated)
        ));
        // the lane id is DATA, not a closed set: a byte no plane registered
        // decodes fine and is dropped by the flow lookup that finds nothing.
        // Refusing it here would make the decoder a second registry, and the
        // decoder has no lane table to be right about.
        let mut frame = encode_datagram(VOICE, flow, b"x").unwrap();
        frame[0] = 250;
        let (service, _, _) = decode_datagram(&frame).unwrap();
        assert_eq!(service, Service::from_lane_id(250));
    }

    #[tokio::test]
    async fn hello_round_trip() {
        let hello = Hello {
            service: Service::STATE_SYNC,
            flow: FlowId::derive(b"snapshot:abc"),
            intent: 3,
            meta: b"range=0..100".to_vec(),
        };
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_hello(&mut a, &hello).await.unwrap();
        let read = read_hello(&mut b).await.unwrap();
        assert_eq!(read, hello);
    }

    #[test]
    fn the_well_known_ports_derive_from_the_lane_id() {
        // both ends compute the same dial port from the id alone, with no
        // signaling — which is why the id, not a name, is the wire fact.
        for id in [1u8, 2, 6, 99] {
            let service = Service::from_lane_id(id);
            assert_eq!(service.lane_id(), id);
            assert_eq!(service.overlay_stream_port(), 45800 + id as u16);
            assert_eq!(service.overlay_datagram_port(), 45900 + id as u16);
        }
        // the two kernel lanes are the ids `modules::RESERVED_LANE_IDS`
        // refuses to declare, so a declared lane can never take their ports.
        assert_eq!(Service::STATE_SYNC.lane_id(), 1);
        assert_eq!(Service::MODULE_CODE.lane_id(), 6);
    }
}
