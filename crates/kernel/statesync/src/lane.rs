//! Bulk state requests and the independently trusted finalization that settles their root.
use std::io;

use abi::{ProgramId, Root};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::{Decode as _, Encode as _};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_cryptography::certificate::Verifier as _;
use commonware_parallel::Sequential;
use consensus::{Anchor, Certificate, Context, validators_of};
use node::{Block, Settled};

use crate::{Error, Head, Result};

pub const LANE: &str = "ducktape/state";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Ask {
    Head {
        past: Option<u64>,
    },
    Ops {
        hold: u64,
        program: ProgramId,
        from: u64,
    },
    Blobs,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Piece {
    Pinned(Vec<[u8; 32]>),
    Ops(Vec<u8>),
    End,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Trusted {
    pub network: Vec<u8>,
    pub epoch: u64,
    pub validators: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Closing {
    pub head: Head,
    pub hold: u64,
    pub opening: Block,
    pub certificate: Certificate,
}

pub struct Checked {
    closing: Closing,
    settled: Settled,
}

impl Closing {
    pub fn verify<E: Context>(self, context: &mut E, trusted: &Trusted) -> Result<Checked> {
        let linked = self.opening.links_to(self.head.tip);
        let anchored = self.head.anchor.names(self.head.tip);
        let certified = self.certificate.proposal.payload.0 == self.opening.tip().id;
        let matches = linked && anchored && certified;
        if !matches {
            return Err(Error::Head);
        }
        let epoch = self.certificate.round().epoch().get();
        if epoch < trusted.epoch {
            return Err(Error::Untrusted { epoch });
        }
        verify(context, trusted, &self.certificate)?;
        if let Anchor::Genesis(block) = &self.head.anchor {
            let genesis = block.height == 0;
            if !genesis {
                return Err(Error::Head);
            }
        }
        let mut records = self.opening.records();
        let Some((_, settled)) = records.next() else {
            return Err(Error::Head);
        };
        let unique = records.next().is_none();
        let names_tip = settled.height == self.head.tip.height;
        let matches = unique && names_tip;
        if !matches {
            return Err(Error::Head);
        }
        drop(records);
        Ok(Checked {
            closing: self,
            settled,
        })
    }
}

impl Checked {
    pub fn closing(&self) -> &Closing {
        &self.closing
    }

    pub fn check_root(&self, actual: Root) -> Result<()> {
        let expected = Root(self.settled.root);
        if actual != expected {
            return Err(Error::Root { expected, actual });
        }
        Ok(())
    }

    pub fn into_closing(self) -> Closing {
        self.closing
    }

    pub fn settled(&self) -> Settled {
        self.settled
    }
}

fn verify<E: Context>(context: &mut E, trusted: &Trusted, certificate: &Certificate) -> Result<()> {
    let epoch = certificate.round().epoch().get();
    let validators = validators_of(&trusted.validators).ok_or(Error::Untrusted { epoch })?;
    let scheme = Scheme::verifier(&trusted.network, validators);
    let verifies = certificate.verify(context, &scheme, &Sequential);
    if !verifies {
        return Err(Error::Untrusted { epoch });
    }
    Ok(())
}

impl BorshSerialize for Closing {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.head.serialize(writer)?;
        self.hold.serialize(writer)?;
        self.opening.encode().to_vec().serialize(writer)?;
        self.certificate.encode().to_vec().serialize(writer)
    }
}

impl BorshDeserialize for Closing {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let head = Head::deserialize_reader(reader)?;
        let hold = u64::deserialize_reader(reader)?;
        let bytes = Vec::<u8>::deserialize_reader(reader)?;
        let opening = Block::decode_cfg(bytes.as_slice(), &()).map_err(invalid)?;
        let bytes = Vec::<u8>::deserialize_reader(reader)?;
        let certificate = Certificate::decode_cfg(
            bytes.as_slice(),
            &Scheme::certificate_codec_config_unbounded(),
        )
        .map_err(invalid)?;
        Ok(Self {
            head,
            hold,
            opening,
            certificate,
        })
    }
}

fn invalid(error: commonware_codec::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
