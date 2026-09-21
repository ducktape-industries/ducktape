use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::{Decode, Encode, Read};
use commonware_consensus::marshal::Start;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_cryptography::certificate::Verifier as _;
use host::Tip;
use node::{Block, Digest};

use crate::marshal::Certificate;

#[derive(Clone, Debug, PartialEq)]
pub enum Anchor {
    Genesis(Block),
    Finalized(Certificate),
}

impl Anchor {
    pub fn names(&self, tip: Tip) -> bool {
        match self {
            Anchor::Genesis(block) => block.tip() == tip,
            Anchor::Finalized(certificate) => certificate.proposal.payload.0 == tip.id,
        }
    }

    pub fn start(&self) -> Start<Scheme, Digest, Block> {
        match self {
            Anchor::Genesis(block) => Start::Genesis(block.clone()),
            Anchor::Finalized(certificate) => Start::Floor(certificate.clone()),
        }
    }
}

impl BorshSerialize for Anchor {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Anchor::Genesis(block) => {
                0u8.serialize(writer)?;
                codec(block).serialize(writer)
            }
            Anchor::Finalized(certificate) => {
                1u8.serialize(writer)?;
                codec(certificate).serialize(writer)
            }
        }
    }
}

impl BorshDeserialize for Anchor {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Anchor> {
        let tag = u8::deserialize_reader(reader)?;
        let bytes = Vec::<u8>::deserialize_reader(reader)?;
        match tag {
            0 => Ok(Anchor::Genesis(decoded(&bytes, &())?)),
            1 => Ok(Anchor::Finalized(decoded(
                &bytes,
                &Scheme::certificate_codec_config_unbounded(),
            )?)),
            _ => Err(invalid(format!("anchor tag {tag}"))),
        }
    }
}

fn codec<T: Encode>(value: &T) -> Vec<u8> {
    value.encode().to_vec()
}

fn decoded<T: Read>(bytes: &[u8], cfg: &T::Cfg) -> io::Result<T> {
    T::decode_cfg(bytes, cfg).map_err(invalid)
}

fn invalid(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
