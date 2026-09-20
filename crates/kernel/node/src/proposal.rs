use abi::Refusal;
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Proposal {
    pub time: u64,
    pub frames: Vec<Vec<u8>>,
}

impl Proposal {
    pub fn encode(&self) -> Vec<u8> {
        abi::encode(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Proposal, Refusal> {
        abi::decode(bytes)
    }
}
