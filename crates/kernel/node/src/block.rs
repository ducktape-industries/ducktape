use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, RangeCfg, Read, ReadExt as _, Write, varint::UInt};
use commonware_consensus::Heightable;
use commonware_consensus::types::Height;
use commonware_cryptography::{Digestible, Hasher as _, Sha256, sha256};
use host::Tip;

pub type Digest = sha256::Digest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub height: u64,
    pub parent: Digest,
    pub time: u64,
    pub frames: Vec<Vec<u8>>,
}

impl Block {
    pub fn genesis(network: &[u8], time: u64) -> Block {
        Block {
            height: 0,
            parent: Sha256::hash(&[network]),
            time,
            frames: Vec::new(),
        }
    }

    pub fn next(parent: Tip, time: u64, frames: Vec<Vec<u8>>) -> Block {
        Block {
            height: parent.height + 1,
            parent: sha256::Digest(parent.id),
            time,
            frames,
        }
    }

    pub fn tip(&self) -> Tip {
        Tip {
            height: self.height,
            id: self.digest().0,
        }
    }

    pub fn links_to(&self, tip: Tip) -> bool {
        self.height == tip.height + 1 && self.parent.0 == tip.id
    }
}

impl Write for Block {
    fn write(&self, writer: &mut impl BufMut) {
        UInt(self.height).write(writer);
        self.parent.write(writer);
        UInt(self.time).write(writer);
        self.frames.write(writer);
    }
}

impl Read for Block {
    type Cfg = ();

    fn read_cfg(reader: &mut impl Buf, _: &Self::Cfg) -> Result<Self, Error> {
        let height = UInt::read(reader)?.into();
        let parent = Digest::read(reader)?;
        let time = UInt::read(reader)?.into();
        let unbounded = RangeCfg::from(0..=usize::MAX);
        let frames = Vec::read_cfg(reader, &(unbounded, (unbounded, ())))?;
        Ok(Block {
            height,
            parent,
            time,
            frames,
        })
    }
}

impl EncodeSize for Block {
    fn encode_size(&self) -> usize {
        UInt(self.height).encode_size()
            + self.parent.encode_size()
            + UInt(self.time).encode_size()
            + self.frames.encode_size()
    }
}

impl Digestible for Block {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        Sha256::hash(&[&commonware_codec::Encode::encode(self)])
    }
}

impl Heightable for Block {
    fn height(&self) -> Height {
        Height::new(self.height)
    }
}

impl commonware_consensus::Block for Block {
    fn parent(&self) -> Digest {
        self.parent
    }
}
