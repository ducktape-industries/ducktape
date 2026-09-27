//! The media frame carried inside a data-plane datagram: a 10-byte header,
//! then one Opus frame.
//!
//! ```text
//! offset  0..4              4..6          6..10
//!         epoch (u32 BE)    seq (u16 BE)  timestamp (u32 BE, 48 kHz units)
//! ```
//!
//! `epoch` names the SENDER'S ENGINE, not the peer: one random value per
//! `VoiceEngine`, so a peer who restarts their media without leaving the
//! roster (webview reload, reconnect) is telling every receiver that their
//! seq counter went back to 0. Without it a retained jitter buffer, anchored
//! at the old high seq, counts the whole restarted stream late forever.
//!
//! `seq` orders frames for the jitter buffer (wrapping, one per 20 ms);
//! `timestamp` rides along for consumers that need media time (DTX gaps,
//! future video sync) — the minimal jitter buffer is seq-driven and ignores
//! it. The sender identity is NOT in the header: it is the datagram's
//! transport-authenticated `PeerId`. no version byte: the enclosing plane
//! datagram already names the service, and the frame is a fixed shape
//! (flag-day rule — no in-band version).

use data_plane::MAX_DATAGRAM_PAYLOAD;

pub const MEDIA_HEADER_LEN: usize = 10;
/// Largest Opus payload per frame. Voice at 32 kbps is ~80 bytes; the bound
/// exists so a media frame always fits one plane datagram.
pub const MAX_OPUS_PAYLOAD: usize = MAX_DATAGRAM_PAYLOAD - MEDIA_HEADER_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaHeader {
    /// the sender's engine instance — see the module docs.
    pub epoch: u32,
    pub seq: u16,
    pub timestamp: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("opus payload {len} exceeds {MAX_OPUS_PAYLOAD}")]
    PayloadTooLarge { len: usize },
    #[error("media frame truncated")]
    Truncated,
}

pub fn encode_frame(header: MediaHeader, opus_payload: &[u8]) -> Result<Vec<u8>, MediaError> {
    if opus_payload.len() > MAX_OPUS_PAYLOAD {
        return Err(MediaError::PayloadTooLarge {
            len: opus_payload.len(),
        });
    }
    let mut frame = Vec::with_capacity(MEDIA_HEADER_LEN + opus_payload.len());
    frame.extend_from_slice(&header.epoch.to_be_bytes());
    frame.extend_from_slice(&header.seq.to_be_bytes());
    frame.extend_from_slice(&header.timestamp.to_be_bytes());
    frame.extend_from_slice(opus_payload);
    Ok(frame)
}

pub fn decode_frame(frame: &[u8]) -> Result<(MediaHeader, &[u8]), MediaError> {
    if frame.len() < MEDIA_HEADER_LEN {
        return Err(MediaError::Truncated);
    }
    let epoch = u32::from_be_bytes(frame[0..4].try_into().expect("4 bytes"));
    let seq = u16::from_be_bytes(frame[4..6].try_into().expect("2 bytes"));
    let timestamp = u32::from_be_bytes(frame[6..10].try_into().expect("4 bytes"));
    Ok((
        MediaHeader {
            epoch,
            seq,
            timestamp,
        },
        &frame[MEDIA_HEADER_LEN..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pins the exact wire bytes (D1: header fields big-endian) — locks this
    /// already-BE datagram codec alongside the call-socket codec
    /// ([`crate::call_wire`]) as part of the wire-standardization sweep.
    #[test]
    fn golden_header_be() {
        let header = MediaHeader {
            epoch: 0x1112_1314,
            seq: 0x0102,
            timestamp: 0x0A0B_0C0D,
        };
        assert_eq!(
            encode_frame(header, &[0x5A]).unwrap(),
            vec![
                0x11, 0x12, 0x13, 0x14, 0x01, 0x02, 0x0A, 0x0B, 0x0C, 0x0D, 0x5A
            ]
        );
    }

    #[test]
    fn frame_round_trip() {
        let header = MediaHeader {
            epoch: u32::MAX,
            seq: 65_535,
            timestamp: 4_000_000_000,
        };
        let frame = encode_frame(header, b"opus").unwrap();
        let (decoded, payload) = decode_frame(&frame).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(payload, b"opus");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            decode_frame(&[1, 0, 0]),
            Err(MediaError::Truncated)
        ));
        let big = vec![0u8; MAX_OPUS_PAYLOAD + 1];
        assert!(matches!(
            encode_frame(
                MediaHeader {
                    epoch: 0,
                    seq: 0,
                    timestamp: 0
                },
                &big
            ),
            Err(MediaError::PayloadTooLarge { .. })
        ));
    }
}
