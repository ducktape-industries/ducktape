//! The node's own settings: `node.toml` in its workspace, overridden by `ducktape run` flags,
//! resolved against the network descriptor into what `run` starts with.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use consensus::{Cadence, Timeouts};
use serde::{Deserialize, Deserializer};

use crate::cores::CpuList;
use crate::mesh;
use crate::run::{HaloConfig, Listen, LoadConfig};
use crate::workspace::{Descriptor, Workspace};
use crate::{Error, Result};

pub const DEFAULT_HTTP: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
pub const DEFAULT_LOG: &str = "info";

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub listen: Option<SocketAddr>,
    pub http: Option<SocketAddr>,
    pub tls: Option<SocketAddr>,
    pub halo: Option<SocketAddr>,
    pub live: Option<SocketAddr>,
    pub log: Option<String>,
    #[serde(default, deserialize_with = "cpu_list")]
    pub block_cpus: Option<CpuList>,
    #[serde(default)]
    pub load: LoadSettings,
    #[serde(default)]
    pub consensus: ConsensusSettings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSettings {
    pub halo_egress: Option<u64>,
    #[serde(default, deserialize_with = "held_permits")]
    pub halo_inbound_streams: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub halo_datagrams: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub blob_serves_in_flight: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub blob_backlog: Option<NonZeroUsize>,
    pub blob_gatherers: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_session_inbound: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_session_outbound: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_group_queue: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_peer_queue: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_lane_backlog: Option<NonZeroUsize>,
    #[serde(default, deserialize_with = "held_permits")]
    pub live_lane_per_sender: Option<NonZeroUsize>,
    pub live_linger_ms: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsensusSettings {
    pub leader_timeout_ms: Option<u64>,
    pub certification_timeout_ms: Option<u64>,
    pub timeout_retry_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    pub listen: Listen,
    pub log: String,
    pub halo: HaloConfig,
    pub load: LoadConfig,
    pub cadence: Cadence,
    pub block_cpus: Option<CpuList>,
}

pub fn permits(count: NonZeroUsize) -> Result<NonZeroUsize> {
    let most = tokio::sync::Semaphore::MAX_PERMITS;
    let held = count.get() <= most;
    if !held {
        return Err(Error::PastPermits { count, most });
    }
    Ok(count)
}

fn held_permits<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<NonZeroUsize>, D::Error> {
    let count = NonZeroUsize::deserialize(deserializer)?;
    permits(count).map(Some).map_err(serde::de::Error::custom)
}

fn cpu_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<CpuList>, D::Error> {
    let text = String::deserialize(deserializer)?;
    CpuList::parse(&text)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl Settings {
    pub fn parse(text: &str) -> std::result::Result<Settings, String> {
        toml::from_str(text).map_err(|error| refusal(text, &error))
    }

    pub fn over(self, under: Settings) -> Settings {
        Settings {
            listen: self.listen.or(under.listen),
            http: self.http.or(under.http),
            tls: self.tls.or(under.tls),
            halo: self.halo.or(under.halo),
            live: self.live.or(under.live),
            log: self.log.or(under.log),
            block_cpus: self.block_cpus.or(under.block_cpus),
            load: self.load.over(under.load),
            consensus: self.consensus.over(under.consensus),
        }
    }

    pub fn resolve(self, descriptor: &Descriptor) -> Result<NodeConfig> {
        let p2p = self.listen.ok_or(Error::NoListen)?;
        let halo = self.halo.ok_or(Error::NoHalo)?;
        let cadence = descriptor.cadence(self.consensus.timeouts())?;
        Ok(NodeConfig {
            listen: Listen {
                p2p,
                http: self.http.unwrap_or(DEFAULT_HTTP),
                tls: self.tls,
                halo,
                live: self.live,
            },
            log: self.log.unwrap_or_else(|| DEFAULT_LOG.to_string()),
            halo: self.load.halo(),
            load: self.load.load(),
            cadence,
            block_cpus: self.block_cpus,
        })
    }
}

impl LoadSettings {
    fn over(self, under: LoadSettings) -> LoadSettings {
        LoadSettings {
            halo_egress: self.halo_egress.or(under.halo_egress),
            halo_inbound_streams: self.halo_inbound_streams.or(under.halo_inbound_streams),
            halo_datagrams: self.halo_datagrams.or(under.halo_datagrams),
            blob_serves_in_flight: self.blob_serves_in_flight.or(under.blob_serves_in_flight),
            blob_backlog: self.blob_backlog.or(under.blob_backlog),
            blob_gatherers: self.blob_gatherers.or(under.blob_gatherers),
            live_session_inbound: self.live_session_inbound.or(under.live_session_inbound),
            live_session_outbound: self.live_session_outbound.or(under.live_session_outbound),
            live_group_queue: self.live_group_queue.or(under.live_group_queue),
            live_peer_queue: self.live_peer_queue.or(under.live_peer_queue),
            live_lane_backlog: self.live_lane_backlog.or(under.live_lane_backlog),
            live_lane_per_sender: self.live_lane_per_sender.or(under.live_lane_per_sender),
            live_linger_ms: self.live_linger_ms.or(under.live_linger_ms),
        }
    }

    fn halo(self) -> HaloConfig {
        let default = HaloConfig::default();
        HaloConfig {
            egress_bytes_per_sec: self.halo_egress.or(default.egress_bytes_per_sec),
            inbound_streams: self.halo_inbound_streams.unwrap_or(default.inbound_streams),
            datagrams: self.halo_datagrams.unwrap_or(default.datagrams),
        }
    }

    fn load(self) -> LoadConfig {
        let default = LoadConfig::default();
        LoadConfig {
            blob_backlog: self.blob_backlog.unwrap_or(default.blob_backlog),
            blob_serves_in_flight: self
                .blob_serves_in_flight
                .unwrap_or(default.blob_serves_in_flight),
            blob_gatherers: self.blob_gatherers.unwrap_or(default.blob_gatherers),
            live_session_inbound: self
                .live_session_inbound
                .unwrap_or(default.live_session_inbound),
            live_session_outbound: self
                .live_session_outbound
                .unwrap_or(default.live_session_outbound),
            live_group_queue: self.live_group_queue.unwrap_or(default.live_group_queue),
            live_peer_queue: self.live_peer_queue.unwrap_or(default.live_peer_queue),
            live_lane_backlog: self.live_lane_backlog.unwrap_or(default.live_lane_backlog),
            live_lane_per_sender: self
                .live_lane_per_sender
                .unwrap_or(default.live_lane_per_sender),
            live_linger: self
                .live_linger_ms
                .map_or(default.live_linger, |ms| Duration::from_millis(ms.get())),
            rpc_cpus: None,
            rpc_blocking_threads: None,
        }
    }
}

impl ConsensusSettings {
    fn over(self, under: ConsensusSettings) -> ConsensusSettings {
        ConsensusSettings {
            leader_timeout_ms: self.leader_timeout_ms.or(under.leader_timeout_ms),
            certification_timeout_ms: self
                .certification_timeout_ms
                .or(under.certification_timeout_ms),
            timeout_retry_ms: self.timeout_retry_ms.or(under.timeout_retry_ms),
        }
    }

    fn timeouts(self) -> Timeouts {
        Timeouts {
            leader: self.leader_timeout_ms.map(Duration::from_millis),
            certification: self.certification_timeout_ms.map(Duration::from_millis),
            retry: self.timeout_retry_ms.map(Duration::from_millis),
        }
    }
}

impl NodeConfig {
    pub fn load(workspace: &Workspace, flags: Settings) -> Result<NodeConfig> {
        let file = workspace.settings()?;
        let descriptor = workspace.descriptor()?;
        flags.over(file).resolve(&descriptor)
    }

    pub fn link_timeout(&self) -> Duration {
        mesh::link_timeout(self.cadence)
    }
}

fn refusal(text: &str, error: &toml::de::Error) -> String {
    let Some(span) = error.span() else {
        return error.message().to_string();
    };
    let line = text[..span.start].matches('\n').count() + 1;
    format!("line {line}: {}", error.message())
}

#[cfg(test)]
mod tests;
