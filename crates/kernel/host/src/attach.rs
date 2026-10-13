// The `$attach` envelope: a frame that names the blobs it brings by id and framed length, and the
// caps on them. Read by the host's submit and by the node's pool, block builder and fetch list.

use std::collections::BTreeSet;

use abi::{BlobId, ProgramId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use state::valid_program_id;

use crate::Submission;

pub const ATTACH: &str = "$attach";
pub const ATTACHMENT_BYTES: u64 = 64 << 20;
pub const BLOCK_ATTACHMENT_BYTES: u64 = 64 << 20;
pub const FRAME_ATTACHMENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Attachment {
    pub id: BlobId,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Attached {
    pub target: ProgramId,
    pub payload: Vec<u8>,
    pub blobs: Vec<Attachment>,
}

impl Attached {
    pub fn bytes(&self) -> u64 {
        self.blobs
            .iter()
            .map(|blob| blob.len)
            .fold(0, u64::saturating_add)
    }
}

pub fn attached(submission: &Submission) -> Option<Result<Attached, Refusal>> {
    let addressed = submission.target == ATTACH;
    if !addressed {
        return None;
    }
    Some(abi::decode(&submission.payload).and_then(checked))
}

fn checked(attached: Attached) -> Result<Attached, Refusal> {
    let runs_a_program = valid_program_id(&attached.target);
    if !runs_a_program {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            format!(
                "{:?} is not a program an attaching frame runs",
                attached.target
            ),
        ));
    }
    let ids: BTreeSet<&BlobId> = attached.blobs.iter().map(|blob| &blob.id).collect();
    let distinct = ids.len() == attached.blobs.len();
    if !distinct {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "an attaching frame names each blob once",
        ));
    }
    let countable = attached.blobs.len() <= FRAME_ATTACHMENTS;
    if !countable {
        return Err(Refusal::new(
            reason::CAPACITY,
            format!("a frame attaches at most {FRAME_ATTACHMENTS} blobs"),
        ));
    }
    let each_fits = attached
        .blobs
        .iter()
        .all(|blob| blob.len <= ATTACHMENT_BYTES);
    if !each_fits {
        return Err(Refusal::new(
            reason::CAPACITY,
            format!("an attachment is at most {ATTACHMENT_BYTES} bytes"),
        ));
    }
    let fits_a_block = attached.bytes() <= BLOCK_ATTACHMENT_BYTES;
    if !fits_a_block {
        return Err(Refusal::new(
            reason::CAPACITY,
            format!("a block carries at most {BLOCK_ATTACHMENT_BYTES} attached bytes"),
        ));
    }
    Ok(attached)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Budget {
    spent: u64,
}

impl Budget {
    pub fn take(&mut self, attached: Attached) -> Result<Attached, Refusal> {
        let spent = self.spent.saturating_add(attached.bytes());
        let fits = spent <= BLOCK_ATTACHMENT_BYTES;
        if !fits {
            return Err(Refusal::new(
                reason::CAPACITY,
                format!("this block's attachments pass {BLOCK_ATTACHMENT_BYTES} bytes"),
            ));
        }
        self.spent = spent;
        Ok(attached)
    }
}

pub fn carried<'a>(submissions: impl IntoIterator<Item = &'a Submission>) -> Vec<Attachment> {
    let mut budget = Budget::default();
    let mut carried = Vec::new();
    for submission in submissions {
        let Some(Ok(attached)) = attached(submission) else {
            continue;
        };
        if let Ok(attached) = budget.take(attached) {
            carried.extend(attached.blobs);
        }
    }
    carried
}
