use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use std::ops::Range;

use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::RangeCfg;
use commonware_runtime::Supervisor;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_storage::Context;
use commonware_storage::journal::contiguous::{Contiguous as _, variable};
use commonware_storage::metadata::{self, Metadata};
use commonware_utils::sequence::U64;

use crate::{Error, Result};

const PROPOSALS: &str = "proposals";
const EPOCH: &str = "epoch";
const BASE_KEY: u64 = 0;
const EPOCH_KEY: u64 = 1;
const FLOOR_KEY: u64 = 2;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Record {
    pub height: u64,
    pub epoch: u64,
    pub view: u64,
    pub proposal: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Epoch {
    pub number: u64,
    pub base: u64,
}

type Records<E> = variable::Journal<E, Vec<u8>>;
type Meta<E> = Metadata<E, U64, Vec<u8>>;

pub struct Journal<E: Context> {
    records: Option<Records<E>>,
    meta: Option<Meta<E>>,
    base: u64,
}

pub fn partition(name: &str, kind: &str) -> String {
    format!("{}-{kind}", abi::hex(name.as_bytes()))
}

impl<E> Journal<E>
where
    E: Context + Supervisor,
{
    pub async fn open(context: E, name: &str) -> Result<Journal<E>> {
        let page_cache = CacheRef::from_pooler(
            &context,
            NonZeroU16::new(128).expect("nonzero"),
            NonZeroUsize::new(64).expect("nonzero"),
        );
        let records = Records::init(
            context.child(PROPOSALS),
            variable::Config {
                partition: partition(name, PROPOSALS),
                items_per_section: NonZeroU64::new(64).expect("nonzero"),
                compression: None,
                codec_config: (RangeCfg::from(0..=usize::MAX), ()),
                page_cache,
                write_buffer: NonZeroUsize::new(1 << 16).expect("nonzero"),
                replay_buffer: NonZeroUsize::new(1 << 20).expect("nonzero"),
            },
        )
        .await
        .map_err(storage)?;
        let meta = Meta::init(
            context.child(EPOCH),
            metadata::Config {
                partition: partition(name, EPOCH),
                codec_config: (RangeCfg::from(0..=usize::MAX), ()),
            },
        )
        .await
        .map_err(storage)?;
        let base = match meta.get(&U64::from(BASE_KEY)) {
            Some(bytes) => {
                abi::decode(bytes).map_err(|refusal| Error::Journal(refusal.sentence))?
            }
            None => 0,
        };
        Ok(Journal {
            records: Some(records),
            meta: Some(meta),
            base,
        })
    }

    pub fn heights(&self) -> Result<Range<u64>> {
        let bounds = self.records()?.bounds();
        Ok((self.base + bounds.start + 1)..(self.base + bounds.end + 1))
    }

    pub async fn read(&self, height: u64) -> Result<Option<Record>> {
        let heights = self.heights()?;
        if !heights.contains(&height) {
            return Ok(None);
        }
        let bytes = self
            .records()?
            .read(height - self.base - 1)
            .await
            .map_err(storage)?;
        let record: Record =
            abi::decode(&bytes).map_err(|refusal| Error::Journal(refusal.sentence))?;
        Ok(Some(record))
    }

    pub async fn read_range(&self, heights: Range<u64>) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for height in heights {
            let Some(record) = self.read(height).await? else {
                break;
            };
            records.push(record);
        }
        Ok(records)
    }

    pub async fn append(&mut self, record: &Record) -> Result<()> {
        let records = self.take_records()?;
        let expected = self.base + records.size() + 1;
        let contiguous = record.height == expected;
        if !contiguous {
            self.records = Some(records);
            return Err(Error::Journal(format!(
                "block {} is out of sequence; the journal expects {expected}",
                record.height
            )));
        }
        let (records, _) = records
            .append(&abi::encode(record))
            .await
            .map_err(storage)?;
        let records = records.sync().await.map_err(storage)?;
        self.records = Some(records);
        Ok(())
    }

    pub async fn set_base(&mut self, base: u64) -> Result<()> {
        let empty = self.records()?.size() == 0;
        if !empty {
            return Err(Error::Journal(
                "the base moves only under an empty journal".into(),
            ));
        }
        self.base = base;
        self.put(BASE_KEY, abi::encode(&base)).await
    }

    pub fn epoch(&self) -> Result<Epoch> {
        match self.meta()?.get(&U64::from(EPOCH_KEY)) {
            Some(bytes) => abi::decode(bytes).map_err(|refusal| Error::Journal(refusal.sentence)),
            None => Ok(Epoch::default()),
        }
    }

    pub async fn set_epoch(&mut self, epoch: Epoch) -> Result<()> {
        self.put(EPOCH_KEY, abi::encode(&epoch)).await
    }

    pub fn floor(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.meta()?.get(&U64::from(FLOOR_KEY)).cloned())
    }

    pub async fn set_floor(&mut self, certificate: Vec<u8>) -> Result<()> {
        self.put(FLOOR_KEY, certificate).await
    }

    async fn put(&mut self, key: u64, value: Vec<u8>) -> Result<()> {
        let meta = self.meta.take().ok_or_else(lost)?;
        let meta = meta
            .put_sync(U64::from(key), value)
            .await
            .map_err(storage)?;
        self.meta = Some(meta);
        Ok(())
    }

    fn records(&self) -> Result<&Records<E>> {
        self.records.as_ref().ok_or_else(lost)
    }

    fn take_records(&mut self) -> Result<Records<E>> {
        self.records.take().ok_or_else(lost)
    }

    fn meta(&self) -> Result<&Meta<E>> {
        self.meta.as_ref().ok_or_else(lost)
    }
}

fn lost() -> Error {
    Error::Journal("the journal was lost to a storage fault".into())
}

fn storage(error: impl std::fmt::Display) -> Error {
    Error::Journal(error.to_string())
}
