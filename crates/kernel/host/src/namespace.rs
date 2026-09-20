use abi::BlobId;

pub const BLOBS: &str = "$blobs";
pub const QUEUE: &str = "$queue";
pub const PROGRAMS: &str = "$programs";
pub const NETWORK: &str = "$network";
pub const RESERVED: [&str; 4] = [BLOBS, QUEUE, PROGRAMS, NETWORK];

pub const LIMITS: &[u8] = b"limits";

pub fn blob(id: &BlobId) -> Vec<u8> {
    abi::encode(id)
}
