//! Framed transport for a trusted Tree guest running in a native child process.
//! This bounds host IPC, not the native program's access to the operating system.
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const MAX_PACKET_BYTES: usize = crate::MAX_SNAPSHOT_BYTES + 64;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Init {
        macos: bool,
    },
    Tick(
        #[serde(
            serialize_with = "crate::snapshot::serialize_bytes",
            deserialize_with = "crate::snapshot::decode_bytes"
        )]
        Vec<u8>,
    ),
    Snapshot,
    Restore {
        #[serde(
            serialize_with = "crate::snapshot::serialize_bytes",
            deserialize_with = "crate::snapshot::decode_bytes"
        )]
        state: Vec<u8>,
        macos: bool,
    },
}

pub type Response = Result<Vec<u8>, String>;

/// Preserve the Result wire layout while treating its payload as one bounded
/// byte blob. Native snapshot IPC must not visit millions of individual bytes.
pub fn encode_response(response: &Response) -> Vec<u8> {
    #[derive(Serialize)]
    struct Bytes<'a>(#[serde(serialize_with = "crate::snapshot::serialize_bytes")] &'a [u8]);
    crate::encode(&response.as_ref().map(|bytes| Bytes(bytes)))
}

pub fn decode_response(bytes: &[u8]) -> Result<Response, String> {
    #[derive(Deserialize)]
    struct Bytes(#[serde(deserialize_with = "crate::snapshot::decode_bytes")] Vec<u8>);
    crate::decode::<Result<Bytes, String>>(bytes).map(|response| response.map(|bytes| bytes.0))
}

pub fn read_packet(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .map_err(|error| error.to_string())?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_PACKET_BYTES {
        return Err("native packet exceeds the byte budget".into());
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

pub fn write_packet(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_PACKET_BYTES {
        return Err("native packet exceeds the byte budget".into());
    }
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|()| writer.write_all(bytes))
        .and_then(|()| writer.flush())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_blob_packets_keep_the_exact_native_wire_layout() {
        let payload = vec![65, 66];
        let mut expected = 0u32.to_le_bytes().to_vec();
        expected.extend(2u64.to_le_bytes());
        expected.extend(&payload);
        assert_eq!(encode_response(&Ok(payload.clone())), expected);
        assert_eq!(decode_response(&expected).unwrap(), Ok(payload.clone()));
        expected[..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(crate::encode(&Request::Tick(payload.clone())), expected);
        expected[..4].copy_from_slice(&3u32.to_le_bytes());
        expected.push(1);
        assert_eq!(
            crate::encode(&Request::Restore {
                state: payload,
                macos: true
            }),
            expected
        );
        let failure = Err("native failure".into());
        assert_eq!(encode_response(&failure), crate::encode(&failure));
        assert_eq!(
            decode_response(&encode_response(&failure)).unwrap(),
            failure
        );
    }

    #[test]
    fn native_packet_rejects_oversize_before_reading_payload() {
        struct HeaderOnly(std::io::Cursor<[u8; 4]>);
        impl Read for HeaderOnly {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    self.0.position() < 4,
                    "oversized payload must never be read"
                );
                self.0.read(output)
            }
        }
        let mut input = HeaderOnly(std::io::Cursor::new(
            ((MAX_PACKET_BYTES + 1) as u32).to_le_bytes(),
        ));
        assert!(read_packet(&mut input).unwrap_err().contains("byte budget"));
        let mut output = Vec::new();
        assert!(write_packet(&mut output, &vec![0; MAX_PACKET_BYTES + 1]).is_err());
        assert!(output.is_empty(), "rejected packet must not write a header");
    }

    #[test]
    fn native_packet_preserves_separate_requests() {
        let mut stream = Vec::new();
        for request in [Request::Init { macos: true }, Request::Snapshot] {
            write_packet(&mut stream, &crate::encode(&request)).unwrap();
        }
        let mut input = std::io::Cursor::new(stream);
        assert!(matches!(
            crate::decode::<Request>(&read_packet(&mut input).unwrap()).unwrap(),
            Request::Init { macos: true }
        ));
        assert!(matches!(
            crate::decode::<Request>(&read_packet(&mut input).unwrap()).unwrap(),
            Request::Snapshot
        ));
    }
}
