use std::collections::BTreeMap;
use std::io;

use abi::{ProgramId, Refusal};
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
    /// The joiner holding `hold` has synced `program`, so the hold lets go of it.
    Release {
        hold: u64,
        program: ProgramId,
    },
}

#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum Response {
    Head(Head),
    Sync(Vec<u8>),
    Refused(Refusal),
    /// A release was taken; one naming no live hold changes nothing.
    Released,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    pub tip: Tip,
    pub anchor: Anchor,
    pub targets: BTreeMap<ProgramId, SyncTarget>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub tip: Tip,
    pub targets: BTreeMap<ProgramId, SyncTarget>,
}

impl BorshSerialize for Head {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        write_tip(&self.tip, writer)?;
        self.anchor.serialize(writer)?;
        write_targets(&self.targets, writer)
    }
}

impl BorshDeserialize for Head {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Head> {
        let tip = read_tip(reader)?;
        let anchor = Anchor::deserialize_reader(reader)?;
        let targets = read_targets(reader)?;
        Ok(Head {
            tip,
            anchor,
            targets,
        })
    }
}

impl BorshSerialize for Manifest {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        write_tip(&self.tip, writer)?;
        write_targets(&self.targets, writer)
    }
}

impl BorshDeserialize for Manifest {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Manifest> {
        let tip = read_tip(reader)?;
        let targets = read_targets(reader)?;
        Ok(Manifest { tip, targets })
    }
}

fn write_tip<W: io::Write>(tip: &Tip, writer: &mut W) -> io::Result<()> {
    tip.height.serialize(writer)?;
    tip.id.serialize(writer)
}

fn read_tip<R: io::Read>(reader: &mut R) -> io::Result<Tip> {
    let height = u64::deserialize_reader(reader)?;
    let id = <[u8; 32]>::deserialize_reader(reader)?;
    Ok(Tip { height, id })
}

fn write_targets<W: io::Write>(
    targets: &BTreeMap<ProgramId, SyncTarget>,
    writer: &mut W,
) -> io::Result<()> {
    let targets: Vec<(&ProgramId, Vec<u8>)> = targets
        .iter()
        .map(|(program, target)| (program, codec(target)))
        .collect();
    targets.serialize(writer)
}

fn read_targets<R: io::Read>(reader: &mut R) -> io::Result<BTreeMap<ProgramId, SyncTarget>> {
    Vec::<(ProgramId, Vec<u8>)>::deserialize_reader(reader)?
        .into_iter()
        .map(|(program, target)| Ok((program, decoded(&target, &())?)))
        .collect()
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
