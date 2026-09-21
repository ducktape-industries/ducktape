use std::collections::BTreeMap;
use std::io;

use abi::{BlobId, ProgramId, Refusal};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::{Decode, Encode, Read};
use consensus::Anchor;
use host::Tip;
use state::SyncTarget;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Request {
    Head,
    Sync {
        program: ProgramId,
        request: Vec<u8>,
    },
    Blob(BlobId),
}

#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum Response {
    Head(Head),
    Sync(Vec<u8>),
    Blob(Option<Vec<u8>>),
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    pub tip: Tip,
    pub anchor: Anchor,
    pub targets: BTreeMap<ProgramId, SyncTarget>,
}

impl BorshSerialize for Head {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.tip.height.serialize(writer)?;
        self.tip.id.serialize(writer)?;
        self.anchor.serialize(writer)?;
        let targets: Vec<(&ProgramId, Vec<u8>)> = self
            .targets
            .iter()
            .map(|(program, target)| (program, codec(target)))
            .collect();
        targets.serialize(writer)
    }
}

impl BorshDeserialize for Head {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Head> {
        let height = u64::deserialize_reader(reader)?;
        let id = <[u8; 32]>::deserialize_reader(reader)?;
        let anchor = Anchor::deserialize_reader(reader)?;
        let targets = Vec::<(ProgramId, Vec<u8>)>::deserialize_reader(reader)?
            .into_iter()
            .map(|(program, target)| Ok((program, decoded(&target, &())?)))
            .collect::<io::Result<_>>()?;
        Ok(Head {
            tip: Tip { height, id },
            anchor,
            targets,
        })
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
