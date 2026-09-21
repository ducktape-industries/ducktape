use abi::{Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::{Signer as _, ed25519};
use host::Submission;
use keyscheme::KeyScheme;

pub const NAMESPACE: &[u8] = b"ducktape:frame";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Body {
    pub scheme: KeyScheme,
    pub signer: Vec<u8>,
    pub network: Vec<u8>,
    pub seq: u64,
    pub target: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Frame {
    pub body: Body,
    pub proof: Vec<u8>,
}

impl Body {
    pub fn preimage(&self) -> Vec<u8> {
        abi::encode(self)
    }
}

impl Frame {
    pub fn sign(
        key: &ed25519::PrivateKey,
        network: &[u8],
        seq: u64,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        let body = Body {
            scheme: KeyScheme::Ed25519,
            signer: key.public_key().as_ref().to_vec(),
            network: network.to_vec(),
            seq,
            target: target.to_owned(),
            payload,
        };
        let proof = key.sign(NAMESPACE, &body.preimage()).as_ref().to_vec();
        Frame { body, proof }
    }

    pub fn encode(&self) -> Vec<u8> {
        abi::encode(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Frame, Refusal> {
        abi::decode(bytes)
    }

    pub fn verify(&self, network: &[u8]) -> Result<Submission, Refusal> {
        let on_this_network = self.body.network == network;
        if !on_this_network {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                "the frame names another network",
            ));
        }
        let bound = self.body.scheme.verify(
            &self.body.signer,
            NAMESPACE,
            &self.body.preimage(),
            &self.proof,
        );
        if !bound {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                "the proof does not bind the frame to its signer",
            ));
        }
        Ok(Submission {
            signer: self.body.signer.clone(),
            seq: self.body.seq,
            target: self.body.target.clone(),
            payload: self.body.payload.clone(),
        })
    }
}

pub fn verify(bytes: &[u8], network: &[u8]) -> Result<Submission, Refusal> {
    Frame::decode(bytes)?.verify(network)
}
