//! The video frame layer inside a data-plane datagram on `Service::Video`:
//! a 17-byte header, then one fragment of one encoded (VP8) frame. A frame
//! larger than a datagram fragments across several; ANY missing fragment
//! drops the whole frame — no retransmit, recovery is the next keyframe.
//! no version byte: the enclosing plane datagram already names the service,
//! and the frame is a fixed shape (flag-day rule — no in-band version).
//!
//! ```text
//! offset  0      1..5              5..7               7..9
//!         flags  frame_no (u32BE)  frag_index (u16BE) frag_count (u16BE)
//! offset  9..13  13..17
//!         ts_ms  epoch (u32BE)
//! ```
//!
//! `epoch` names the SENDER'S ENGINE, not the peer: one random value per
//! `VoiceEngine`, so a peer who restarts their media without leaving the
//! roster is telling every receiver that their `frame_no` went back to 0.
//! Without it a retained reassembler, holding a high `last_emitted`, calls
//! the whole restarted stream stale forever and never asks for a keyframe.

use data_plane::MAX_DATAGRAM_PAYLOAD;

pub const VIDEO_HEADER_LEN: usize = 17;
/// flags bit 0: this frame is a keyframe (decoder sync point).
pub const FLAG_KEYFRAME: u8 = 0b0000_0001;
/// Encoded bytes per fragment: a plane datagram payload minus this header.
pub const MAX_FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM_PAYLOAD - VIDEO_HEADER_LEN;
/// Fragments per frame — bounds reassembly memory. 96 × 1344 ≈ 126 KiB,
/// comfortable for a 720p VP8 keyframe at the top of the rate ladder.
pub const MAX_FRAGS: usize = 96;
pub const MAX_FRAME_BYTES: usize = MAX_FRAGS * MAX_FRAGMENT_PAYLOAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHeader {
    pub keyframe: bool,
    pub frame_no: u32,
    pub frag_index: u16,
    pub frag_count: u16,
    pub ts_ms: u32,
    /// the sender's engine instance — see the module docs.
    pub epoch: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("encoded frame {len} exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge { len: usize },
    #[error("empty frame")]
    Empty,
    #[error("video fragment truncated")]
    Truncated,
    #[error("inconsistent fragment header")]
    BadHeader,
}

pub fn encode_fragment(header: VideoHeader, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(VIDEO_HEADER_LEN + payload.len());
    frame.push(if header.keyframe { FLAG_KEYFRAME } else { 0 });
    frame.extend_from_slice(&header.frame_no.to_be_bytes());
    frame.extend_from_slice(&header.frag_index.to_be_bytes());
    frame.extend_from_slice(&header.frag_count.to_be_bytes());
    frame.extend_from_slice(&header.ts_ms.to_be_bytes());
    frame.extend_from_slice(&header.epoch.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub fn decode_fragment(frame: &[u8]) -> Result<(VideoHeader, &[u8]), VideoError> {
    if frame.len() < VIDEO_HEADER_LEN {
        return Err(VideoError::Truncated);
    }
    let header = VideoHeader {
        keyframe: frame[0] & FLAG_KEYFRAME != 0,
        frame_no: u32::from_be_bytes(frame[1..5].try_into().expect("4 bytes")),
        frag_index: u16::from_be_bytes(frame[5..7].try_into().expect("2 bytes")),
        frag_count: u16::from_be_bytes(frame[7..9].try_into().expect("2 bytes")),
        ts_ms: u32::from_be_bytes(frame[9..13].try_into().expect("4 bytes")),
        epoch: u32::from_be_bytes(frame[13..17].try_into().expect("4 bytes")),
    };
    if header.frag_count == 0
        || header.frag_count as usize > MAX_FRAGS
        || header.frag_index >= header.frag_count
    {
        return Err(VideoError::BadHeader);
    }
    Ok((header, &frame[VIDEO_HEADER_LEN..]))
}

/// Split one encoded frame into ready-to-send datagram payloads.
pub fn fragment_frame(
    epoch: u32,
    frame_no: u32,
    keyframe: bool,
    ts_ms: u32,
    data: &[u8],
) -> Result<Vec<Vec<u8>>, VideoError> {
    if data.is_empty() {
        return Err(VideoError::Empty);
    }
    if data.len() > MAX_FRAME_BYTES {
        return Err(VideoError::FrameTooLarge { len: data.len() });
    }
    let count = data.len().div_ceil(MAX_FRAGMENT_PAYLOAD);
    Ok(data
        .chunks(MAX_FRAGMENT_PAYLOAD)
        .enumerate()
        .map(|(index, chunk)| {
            encode_fragment(
                VideoHeader {
                    keyframe,
                    frame_no,
                    frag_index: index as u16,
                    frag_count: count as u16,
                    ts_ms,
                    epoch,
                },
                chunk,
            )
        })
        .collect())
}

/// Wrapping frame_no comparison: is `a` newer than `b`?
pub(crate) fn frame_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pins the exact wire bytes (D1: header fields big-endian) — locks this
    /// already-BE datagram codec alongside the call-socket codec
    /// ([`crate::call_wire`]) as part of the wire-standardization sweep.
    #[test]
    fn golden_header_be() {
        let header = VideoHeader {
            keyframe: true,
            frame_no: 0x0102_0304,
            frag_index: 0x0506,
            frag_count: 0x0708,
            ts_ms: 0x090A_0B0C,
            epoch: 0x0D0E_0F10,
        };
        assert_eq!(
            encode_fragment(header, &[0xAA]),
            vec![
                0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
                0x0E, 0x0F, 0x10, 0xAA,
            ]
        );
    }

    #[test]
    fn fragments_round_trip() {
        // 2 full fragments + a partial third.
        let data_len = MAX_FRAGMENT_PAYLOAD * 2 + 10;
        let data: Vec<u8> = (0..data_len).map(|i| (i % 251) as u8).collect();
        let fragments = fragment_frame(9, 7, true, 12_345, &data).unwrap();
        assert_eq!(fragments.len(), 3);

        let mut reassembled = Vec::with_capacity(data_len);
        for (index, fragment) in fragments.iter().enumerate() {
            let (header, payload) = decode_fragment(fragment).unwrap();
            assert_eq!(header.frame_no, 7);
            assert!(header.keyframe);
            assert_eq!(header.ts_ms, 12_345);
            assert_eq!(header.frag_index, index as u16);
            assert_eq!(header.frag_count, 3);
            assert_eq!(header.epoch, 9);
            reassembled.extend_from_slice(payload);
        }
        assert_eq!(reassembled, data);
    }

    #[test]
    fn exact_multiple_has_no_empty_tail() {
        let data = vec![0xAB; MAX_FRAGMENT_PAYLOAD * 3];
        let fragments = fragment_frame(0, 1, false, 0, &data).unwrap();
        assert_eq!(fragments.len(), 3);
        for fragment in &fragments {
            let (header, payload) = decode_fragment(fragment).unwrap();
            assert_eq!(header.frag_count, 3);
            assert_eq!(payload.len(), MAX_FRAGMENT_PAYLOAD);
        }
    }

    #[test]
    fn empty_and_oversize_inputs_error() {
        assert!(matches!(
            fragment_frame(0, 0, false, 0, &[]),
            Err(VideoError::Empty)
        ));
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            fragment_frame(0, 0, false, 0, &big),
            Err(VideoError::FrameTooLarge { len }) if len == MAX_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(matches!(
            decode_fragment(&[1, 0, 0]),
            Err(VideoError::Truncated)
        ));
    }

    #[test]
    fn decode_rejects_zero_frag_count() {
        let fragment = encode_fragment(
            VideoHeader {
                keyframe: false,
                frame_no: 0,
                frag_index: 0,
                frag_count: 0,
                ts_ms: 0,
                epoch: 0,
            },
            b"x",
        );
        assert!(matches!(
            decode_fragment(&fragment),
            Err(VideoError::BadHeader)
        ));
    }

    #[test]
    fn decode_rejects_index_out_of_range() {
        let fragment = encode_fragment(
            VideoHeader {
                keyframe: false,
                frame_no: 0,
                frag_index: 2,
                frag_count: 2,
                ts_ms: 0,
                epoch: 0,
            },
            b"x",
        );
        assert!(matches!(
            decode_fragment(&fragment),
            Err(VideoError::BadHeader)
        ));
    }

    #[test]
    fn frame_no_comparison_wraps() {
        assert!(frame_newer(1, 0));
        assert!(frame_newer(0, u32::MAX));
        assert!(frame_newer(5, u32::MAX - 5));
        assert!(!frame_newer(u32::MAX - 5, 5));
    }
}
