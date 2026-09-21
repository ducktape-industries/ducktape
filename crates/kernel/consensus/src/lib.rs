mod anchor;
mod cadence;
mod catchup;
mod chain;
mod engine;
mod lanes;
mod marshal;
mod membership;
mod roster;

use commonware_runtime::{BufferPooler, Clock, Metrics, Spawner, Storage};

pub use anchor::Anchor;
pub use cadence::Cadence;
pub use chain::{App, Chain};
pub use lanes::{EngineChannels, EngineMux, MarshalLanes, channel};
pub use marshal::{Certificate, Marshal, MarshalMailbox, Transport};
pub use membership::{Error as MembershipError, Membership, Standing};
pub use roster::{Roster, validators_of};

#[cfg(feature = "sim")]
pub use lanes::SimMesh;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub epoch_length: u64,
    pub cadence: Cadence,
}

impl Network {
    pub fn epoch_after(&self, height: u64) -> u64 {
        (height + 1) / self.epoch_length
    }

    pub fn anchor(&self, epoch: u64) -> u64 {
        (epoch * self.epoch_length).saturating_sub(1)
    }

    pub fn closes_an_epoch(&self, height: u64) -> bool {
        (height + 1).is_multiple_of(self.epoch_length)
    }
}

pub trait Context:
    Spawner + Clock + Storage + Metrics + BufferPooler + rand_core::CryptoRng + Send + Sync + 'static
{
}

impl<E> Context for E where
    E: Spawner
        + Clock
        + Storage
        + Metrics
        + BufferPooler
        + rand_core::CryptoRng
        + Send
        + Sync
        + 'static
{
}
