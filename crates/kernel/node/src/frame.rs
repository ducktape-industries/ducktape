use std::sync::Arc;

use abi::{Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::{Signer as _, ed25519};
use host::{Submission, Tip};
use keyscheme::KeyScheme;

pub const NAMESPACE: &[u8] = b"ducktape:frame";
pub const QUERY_NAMESPACE: &[u8] = b"ducktape:query";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Body {
    pub scheme: KeyScheme,
    pub signer: Vec<u8>,
    pub network: Vec<u8>,
    pub anchor: Tip,
    pub salt: Option<u64>,
    pub target: String,
    pub payload: Vec<u8>,
    pub client: Option<String>,
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

    pub fn submission(&self) -> Submission {
        Submission {
            signer: self.signer.clone(),
            anchor: self.anchor,
            salt: self.salt,
            target: self.target.clone(),
            payload: self.payload.clone(),
            client: self.client.clone(),
        }
    }
}

impl Frame {
    pub fn sign(
        key: &ed25519::PrivateKey,
        network: &[u8],
        anchor: Tip,
        salt: Option<u64>,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        Frame::signed(NAMESPACE, None, key, network, anchor, salt, target, payload)
    }

    pub fn sign_as(
        client: &str,
        key: &ed25519::PrivateKey,
        network: &[u8],
        anchor: Tip,
        salt: Option<u64>,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        let client = Some(client);
        Frame::signed(
            NAMESPACE, client, key, network, anchor, salt, target, payload,
        )
    }

    pub fn sign_query(
        key: &ed25519::PrivateKey,
        network: &[u8],
        anchor: Tip,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        Frame::signed(
            QUERY_NAMESPACE,
            None,
            key,
            network,
            anchor,
            None,
            target,
            payload,
        )
    }

    pub fn sign_query_as(
        client: &str,
        key: &ed25519::PrivateKey,
        network: &[u8],
        anchor: Tip,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        let client = Some(client);
        Frame::signed(
            QUERY_NAMESPACE,
            client,
            key,
            network,
            anchor,
            None,
            target,
            payload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn signed(
        namespace: &[u8],
        client: Option<&str>,
        key: &ed25519::PrivateKey,
        network: &[u8],
        anchor: Tip,
        salt: Option<u64>,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        let body = Body {
            scheme: KeyScheme::Ed25519,
            signer: key.public_key().as_ref().to_vec(),
            network: network.to_vec(),
            anchor,
            salt,
            target: target.to_owned(),
            payload,
            client: client.map(str::to_owned),
        };
        let proof = key.sign(namespace, &body.preimage()).as_ref().to_vec();
        Frame { body, proof }
    }

    pub fn encode(&self) -> Vec<u8> {
        abi::encode(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Frame, Refusal> {
        abi::decode(bytes)
    }

    pub fn verify(&self, network: &[u8]) -> Result<Submission, Refusal> {
        self.verified(NAMESPACE, network)
    }

    pub fn verify_query(&self, network: &[u8]) -> Result<Submission, Refusal> {
        self.verified(QUERY_NAMESPACE, network)
    }

    fn verified(&self, namespace: &[u8], network: &[u8]) -> Result<Submission, Refusal> {
        let on_this_network = self.body.network == network;
        if !on_this_network {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                "the frame names another network",
            ));
        }
        let bound = self.body.scheme.verify(
            &self.body.signer,
            namespace,
            &self.body.preimage(),
            &self.proof,
        );
        if !bound {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                "the proof does not bind the frame to its signer",
            ));
        }
        Ok(self.body.submission())
    }
}

pub fn verify(bytes: &[u8], network: &[u8]) -> Result<Submission, Refusal> {
    Frame::decode(bytes)?.verify(network)
}

pub fn verify_query(bytes: &[u8], network: &[u8]) -> Result<Submission, Refusal> {
    Frame::decode(bytes)?.verify_query(network)
}

#[derive(Clone, Debug)]
pub struct Checked(Arc<Verdict>);

#[derive(Debug)]
struct Verdict {
    frame: Vec<u8>,
    network: Vec<u8>,
    submission: Submission,
}

impl Checked {
    pub fn frame(&self) -> &[u8] {
        &self.0.frame
    }

    pub fn network(&self) -> &[u8] {
        &self.0.network
    }

    pub fn submission(&self) -> &Submission {
        &self.0.submission
    }
}

pub(crate) fn check(frame: Vec<u8>, network: &[u8]) -> Result<Checked, Refusal> {
    let submission = verify(&frame, network)?;
    let verdict = Verdict {
        frame,
        network: network.to_vec(),
        submission,
    };
    Ok(Checked(Arc::new(verdict)))
}
