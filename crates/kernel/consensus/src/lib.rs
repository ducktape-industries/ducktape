mod cadence;
mod chain;
mod engine;
mod lanes;
mod marshal;
mod membership;
mod roster;

use commonware_runtime::{BufferPooler, Clock, Metrics, Spawner, Storage};

pub use commonware_consensus::marshal::Start;

pub use cadence::Cadence;
pub use chain::{App, Chain};
pub use lanes::{EngineChannels, EngineMux, MarshalLanes};
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
