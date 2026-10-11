use abi::BlobId;

use crate::Submission;

pub const BLOBS: &str = "$blobs";
pub const PROGRAMS: &str = "$programs";
pub const NETWORK: &str = "$network";
pub const RECENT: &str = "$recent";
pub const SPENT: &str = "$spent";
pub const RESERVED: [&str; 5] = [BLOBS, PROGRAMS, NETWORK, RECENT, SPENT];

pub const ID: &[u8] = b"id";
pub const LIMITS: &[u8] = b"limits";
pub const EPOCH_LENGTH: &[u8] = b"epoch_length";
pub const MEMBER_CAP: &[u8] = b"member_cap";
pub const ROLES: &[u8] = b"roles";

pub fn blob(id: &BlobId) -> Vec<u8> {
    abi::encode(id)
}

pub fn height(height: u64) -> Vec<u8> {
    height.to_be_bytes().to_vec()
}

pub fn spent(submission: &Submission) -> Vec<u8> {
    [height(submission.anchor.height), submission.id().to_vec()].concat()
}

pub fn epoch(number: u64) -> Vec<u8> {
    format!("epoch/{number}").into_bytes()
}

pub fn epoch_validators(number: u64) -> Vec<u8> {
    format!("validators/{number}").into_bytes()
}
