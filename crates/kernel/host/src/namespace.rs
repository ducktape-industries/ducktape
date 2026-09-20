use abi::BlobId;

pub const BLOBS: &str = "$blobs";
pub const QUEUE: &str = "$queue";
pub const PROGRAMS: &str = "$programs";
pub const NETWORK: &str = "$network";
pub const SIGNERS: &str = "$signers";
pub const RESERVED: [&str; 5] = [BLOBS, QUEUE, PROGRAMS, NETWORK, SIGNERS];

pub const LIMITS: &[u8] = b"limits";
pub const EPOCH_LENGTH: &[u8] = b"epoch_length";
pub const TIP: &[u8] = b"tip";

pub fn blob(id: &BlobId) -> Vec<u8> {
    abi::encode(id)
}

pub fn epoch(number: u64) -> Vec<u8> {
    format!("epoch/{number}").into_bytes()
}
