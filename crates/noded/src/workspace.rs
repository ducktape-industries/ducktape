use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use abi::validators::Member;
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use consensus::{Anchor, Cadence};
use host::{Founding as FoundingProgram, Genesis, Limits};
use node::Block;
use rand_core::CryptoRng;
use serde::Deserialize;

use crate::{Error, Result};

const IDENTITY: &str = "identity.key";
const DESCRIPTOR: &str = "network";
const ANCHOR: &str = "anchor";
const HTTP: &str = "http";
const RUNTIME: &str = "runtime";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Descriptor {
    pub network: String,
    pub time: u64,
    pub block_time_ms: u64,
}

impl Descriptor {
    pub fn id(&self) -> Vec<u8> {
        self.network.as_bytes().to_vec()
    }

    pub fn genesis_block(&self) -> Block {
        Block::genesis(self.network.as_bytes(), self.time)
    }

    pub fn cadence(&self) -> Cadence {
        Cadence::from_millis(self.block_time_ms)
    }
}

#[derive(Clone, Debug)]
pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    pub fn at(dir: impl Into<PathBuf>) -> Workspace {
        Workspace { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.dir.join(RUNTIME)
    }

    pub fn identity(&self) -> Result<ed25519::PrivateKey> {
        let text = std::fs::read_to_string(self.dir.join(IDENTITY))?;
        let seed = hex::decode(text.trim()).map_err(|error| Error::Corrupt(error.to_string()))?;
        ed25519::PrivateKey::decode(seed.as_slice())
            .map_err(|error| Error::Corrupt(error.to_string()))
    }

    pub fn identity_or_create(&self, mut rng: impl CryptoRng) -> Result<ed25519::PrivateKey> {
        let exists = self.dir.join(IDENTITY).exists();
        if exists {
            return self.identity();
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        write_private(&self.dir.join(IDENTITY), hex::encode(seed).as_bytes())?;
        self.identity()
    }

    pub fn descriptor(&self) -> Result<Descriptor> {
        self.read_record(DESCRIPTOR)
    }

    pub fn write_descriptor(&self, descriptor: &Descriptor) -> Result<()> {
        self.write_record(DESCRIPTOR, descriptor)
    }

    pub fn anchor(&self) -> Result<Anchor> {
        self.read_record(ANCHOR)
    }

    pub fn write_anchor(&self, anchor: &Anchor) -> Result<()> {
        self.write_record(ANCHOR, anchor)
    }

    pub fn http(&self) -> Result<SocketAddr> {
        let text = std::fs::read_to_string(self.dir.join(HTTP))?;
        text.trim()
            .parse()
            .map_err(|error: std::net::AddrParseError| Error::Corrupt(error.to_string()))
    }

    pub fn write_http(&self, address: SocketAddr) -> Result<()> {
        Ok(std::fs::write(self.dir.join(HTTP), address.to_string())?)
    }

    fn read_record<T: BorshDeserialize>(&self, name: &str) -> Result<T> {
        let bytes = std::fs::read(self.dir.join(name))?;
        abi::decode(&bytes).map_err(|refusal| Error::Corrupt(refusal.sentence))
    }

    fn write_record<T: BorshSerialize>(&self, name: &str, record: &T) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(std::fs::write(self.dir.join(name), abi::encode(record))?)
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(file.write_all(bytes)?)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    Ok(std::fs::write(path, bytes)?)
}

#[derive(Debug, Deserialize)]
pub struct Founding {
    pub network: String,
    pub time: u64,
    pub epoch_length: u64,
    pub block_time_ms: u64,
    pub modules: PathBuf,
    pub valset: PathBuf,
    pub validators: Vec<Validator>,
    pub programs: Vec<Program>,
    #[serde(default)]
    pub limits: Metering,
}

#[derive(Debug, Default, Deserialize)]
pub struct Metering {
    pub fuel: Option<u64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Validator {
    pub key: String,
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct Program {
    pub id: String,
    pub code: PathBuf,
    pub params: Option<PathBuf>,
}

impl Founding {
    pub fn read(path: &Path) -> Result<(Founding, PathBuf)> {
        let text = std::fs::read_to_string(path)?;
        let founding: Founding = toml::from_str(&text)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok((founding, base))
    }

    pub fn descriptor(&self) -> Descriptor {
        Descriptor {
            network: self.network.clone(),
            time: self.time,
            block_time_ms: self.block_time_ms,
        }
    }

    pub fn genesis(&self, base: &Path) -> Result<Genesis> {
        let validators = self
            .validators
            .iter()
            .map(|validator| {
                Ok(Member {
                    key: hex::decode(&validator.key)?,
                    address: validator.address.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let programs = self
            .programs
            .iter()
            .map(|program| {
                let params = match &program.params {
                    Some(path) => std::fs::read(base.join(path))?,
                    None => Vec::new(),
                };
                Ok(FoundingProgram {
                    program: program.id.clone(),
                    code: std::fs::read(base.join(&program.code))?,
                    params,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Genesis {
            modules: std::fs::read(base.join(&self.modules))?,
            valset: std::fs::read(base.join(&self.valset))?,
            validators,
            programs,
            limits: Limits {
                fuel: self.limits.fuel,
                memory_bytes: self.limits.memory_bytes,
            },
            epoch_length: self.epoch_length,
            time: self.time,
        })
    }
}
