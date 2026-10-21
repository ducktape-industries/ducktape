use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use abi::role::validators::{Member, QueryPolicy};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use consensus::{Anchor, Cadence, CadenceError, Timeouts};
use host::{Founding as FoundingProgram, FoundingView, Genesis, Limits};
use node::Block;
use rand_core::CryptoRng;
use serde::Deserialize;

use crate::config::Settings;
use crate::egress::{Config as EgressConfig, Refused as EgressRefused};
use crate::endpoint::Endpoint;
use crate::{Error, Result};

const IDENTITY: &str = "identity.key";
const DESCRIPTOR: &str = "network";
const ANCHOR: &str = "anchor";
const HTTP: &str = "http";
const SOURCE: &str = "source";
const RUNTIME: &str = "runtime";
const EGRESS: &str = "egress.toml";
const EGRESS_FRESH: &str = "egress.toml.fresh";
const EGRESS_GRANTS: &str = "egress-grants";
const FORWARD: &str = "forward.toml";
const NODE: &str = "node.toml";
const MACHINE: &str = "machine.key";
const STAMP: &str = "contract";
const APPLIED: [&str; 9] = [
    host::STATE_DIR,
    host::BLOBS_DIR,
    crate::history::DIR,
    crate::activity::DIR,
    crate::snapshot::DIR,
    RUNTIME,
    ANCHOR,
    HTTP,
    STAMP,
];
const UNSTAMPED: Stamp = Stamp {
    contract: 9,
    from: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Stamp {
    pub contract: u32,
    pub from: u64,
}

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

    pub fn cadence(&self, timeouts: Timeouts) -> std::result::Result<Cadence, CadenceError> {
        Cadence::new(Duration::from_millis(self.block_time_ms), timeouts)
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
        self.key(IDENTITY)
    }

    pub fn identity_or_create(&self, rng: impl CryptoRng) -> Result<ed25519::PrivateKey> {
        self.minted(IDENTITY, rng)?;
        self.identity()
    }

    pub fn machine_key(&self) -> Result<ed25519::PrivateKey> {
        let key = self.key(MACHINE)?;
        let has_identity = self.dir.join(IDENTITY).exists();
        if !has_identity {
            return Ok(key);
        }
        let shared = self.identity()?.public_key() == key.public_key();
        if shared {
            return Err(Error::MachineIsIdentity);
        }
        Ok(key)
    }

    pub fn machine_key_or_create(&self, rng: impl CryptoRng) -> Result<ed25519::PrivateKey> {
        self.minted(MACHINE, rng)?;
        self.machine_key()
    }

    fn key(&self, name: &str) -> Result<ed25519::PrivateKey> {
        let text = std::fs::read_to_string(self.dir.join(name))?;
        let seed = hex::decode(text.trim()).map_err(|error| Error::Corrupt(error.to_string()))?;
        ed25519::PrivateKey::decode(seed.as_slice())
            .map_err(|error| Error::Corrupt(error.to_string()))
    }

    fn minted(&self, name: &str, mut rng: impl CryptoRng) -> Result<()> {
        let exists = self.dir.join(name).exists();
        if exists {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        write_private(&self.dir.join(name), hex::encode(seed).as_bytes())
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

    pub fn stamp(&self) -> Result<Stamp> {
        match std::fs::read(self.dir.join(STAMP)) {
            Ok(bytes) => abi::decode(&bytes).map_err(|refusal| Error::Corrupt(refusal.sentence)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UNSTAMPED),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write_stamp(&self, stamp: &Stamp) -> Result<()> {
        self.write_record(STAMP, stamp)
    }

    pub(crate) fn set_aside(&self, name: &str) -> Result<PathBuf> {
        let aside = self.dir.join(name);
        std::fs::create_dir(&aside)?;
        for applied in APPLIED {
            let path = self.dir.join(applied);
            if path.exists() {
                std::fs::rename(path, aside.join(applied))?;
            }
        }
        Ok(aside)
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

    pub fn source(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(self.dir.join(SOURCE)) {
            Ok(text) => Ok(Some(text.trim().to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write_source(&self, base: &str) -> Result<()> {
        Ok(std::fs::write(self.dir.join(SOURCE), base)?)
    }

    pub(crate) fn egress(&self) -> std::result::Result<EgressConfig, EgressRefused> {
        let text = match std::fs::read_to_string(self.dir.join(EGRESS)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EgressConfig::default());
            }
            Err(error) => return Err(EgressRefused::Unread(error)),
        };
        self.egress_from(&text)
    }

    pub(crate) fn egress_from(
        &self,
        text: &str,
    ) -> std::result::Result<EgressConfig, EgressRefused> {
        EgressConfig::parse(
            text,
            |variable| std::env::var(variable).ok(),
            |file| std::fs::read_to_string(self.dir.join(file)),
        )
    }

    pub(crate) fn write_egress(&self, text: &str) -> std::io::Result<()> {
        let fresh = self.dir.join(EGRESS_FRESH);
        std::fs::write(&fresh, text)?;
        std::fs::rename(fresh, self.dir.join(EGRESS))
    }

    pub(crate) fn egress_grants(&self) -> std::io::Result<Vec<Vec<u8>>> {
        let text = match std::fs::read_to_string(self.dir.join(EGRESS_GRANTS)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                hex::decode(line).map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{line}: {error}"))
                })
            })
            .collect()
    }

    pub(crate) fn forward_offers(&self) -> PathBuf {
        self.dir.join(FORWARD)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.dir.join(NODE)
    }

    pub fn settings(&self) -> Result<Settings> {
        let path = self.settings_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => return Err(error.into()),
        };
        Settings::parse(&text).map_err(|reason| Error::Settings { path, reason })
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
    /// How many members the network ever holds at once, 1 to 128; 16 when
    /// the file leaves it out.
    #[serde(default = "default_member_cap")]
    pub member_cap: u32,
    pub validators: Vec<Validator>,
    pub programs: Vec<Program>,
    #[serde(default)]
    pub views: Vec<View>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub limits: Metering,
    #[serde(default)]
    pub queries: Queries,
}

fn default_member_cap() -> u32 {
    16
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Queries {
    #[default]
    Signed,
    Open,
}

impl Queries {
    fn policy(&self) -> QueryPolicy {
        match self {
            Queries::Signed => QueryPolicy::Signed,
            Queries::Open => QueryPolicy::Open,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Metering {
    pub fuel: Option<u64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Validator {
    pub key: String,
    pub address: Option<String>,
    pub halo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Program {
    pub id: String,
    pub code: PathBuf,
    pub params: Option<PathBuf>,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// A view with no program behind it, listed by the programs role under `name`.
#[derive(Debug, Deserialize)]
pub struct View {
    pub name: String,
    pub code: PathBuf,
}

impl Founding {
    pub fn read(path: &Path) -> Result<(Founding, PathBuf)> {
        let text = std::fs::read_to_string(path)?;
        let founding: Founding = toml::from_str(&text)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok((founding, base))
    }

    pub fn seat_halo(&mut self, key: &[u8], halo: &str) -> Result<()> {
        let halo = halo_address(halo)?;
        for validator in &mut self.validators {
            let mine = hex::decode(&validator.key).is_ok_and(|named| named == key);
            if !mine {
                continue;
            }
            let named = validator.halo.get_or_insert_with(|| halo.clone());
            if *named != halo {
                return Err(Error::Address(format!(
                    "the founding file seats this node at halo {named}, not {halo}"
                )));
            }
        }
        Ok(())
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
                let address = validator
                    .address
                    .as_deref()
                    .map(str::parse::<Endpoint>)
                    .transpose()
                    .map_err(Error::Address)?;
                let Some(halo) = &validator.halo else {
                    return Err(Error::Address(format!(
                        "validator {} names no halo address",
                        validator.key
                    )));
                };
                Ok(Member {
                    key: hex::decode(&validator.key)?,
                    address: address.as_ref().map(Endpoint::to_string),
                    halo: halo_address(halo)?,
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
                    roles: program.roles.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let views = self
            .views
            .iter()
            .map(|view| {
                Ok(FoundingView {
                    name: view.name.clone(),
                    view: std::fs::read(base.join(&view.code))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Genesis {
            network: self.network.as_bytes().to_vec(),
            validators,
            programs,
            views,
            hooks: self.hooks.clone(),
            limits: Limits {
                fuel: self.limits.fuel,
                memory_bytes: self.limits.memory_bytes,
            },
            epoch_length: self.epoch_length,
            time: self.time,
            member_cap: self.member_cap,
            queries: self.queries.policy(),
        })
    }
}

pub fn halo_address(text: &str) -> Result<String> {
    abi::role::validators::registered_halo(text).map_err(Error::Refused)?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use abi::role::validators::QueryPolicy;
    use commonware_cryptography::Signer as _;
    use commonware_runtime::Runner as _;

    use super::{Founding, Workspace, halo_address};
    use crate::Error;

    fn genesis_of(text: &str) -> host::Genesis {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.wasm"), b"code").unwrap();
        let founding: Founding = toml::from_str(text).unwrap();
        founding.genesis(dir.path()).unwrap()
    }

    fn refused_for_its_halo(outcome: crate::Result<impl std::fmt::Debug>) {
        let Err(Error::Refused(refusal)) = outcome else {
            panic!("not refused: {outcome:?}");
        };
        assert_eq!(refusal.reason, abi::reason::INVALID_INPUT);
        assert!(
            refusal.sentence.starts_with("halo: "),
            "{}",
            refusal.sentence
        );
    }

    const FILE: &str = "network = \"n\"\ntime = 0\nepoch_length = 4\nblock_time_ms = 1000\n\
         HOOKS\
         [[validators]]\nkey = \"00\"\nhalo = \"n0.example.org:1\"\n\
         [[programs]]\nid = \"activity\"\ncode = \"code.wasm\"\nROLES";

    #[test]
    fn a_founding_program_declares_the_roles_it_fills_or_none() {
        let filling = genesis_of(
            &FILE
                .replace("HOOKS", "")
                .replace("ROLES", "roles = [\"egress\", \"later\"]\n"),
        );
        assert_eq!(filling.programs[0].roles, ["egress", "later"]);
        let plain = genesis_of(&FILE.replace("HOOKS", "").replace("ROLES", ""));
        assert!(plain.programs[0].roles.is_empty());
    }

    #[test]
    fn the_founding_file_names_its_hooks_or_none() {
        let hooked = genesis_of(
            &FILE
                .replace("HOOKS", "hooks = [\"activity\"]\n")
                .replace("ROLES", ""),
        );
        assert_eq!(hooked.hooks, ["activity"]);
        let plain = genesis_of(&FILE.replace("HOOKS", "").replace("ROLES", ""));
        assert!(plain.hooks.is_empty());
    }

    #[test]
    fn a_founding_answers_only_signed_queries_unless_it_opens_them() {
        let plain = genesis_of(&FILE.replace("HOOKS", "").replace("ROLES", ""));
        assert_eq!(plain.queries, QueryPolicy::Signed);
        let open = genesis_of(
            &FILE
                .replace("HOOKS", "queries = \"open\"\n")
                .replace("ROLES", ""),
        );
        assert_eq!(open.queries, QueryPolicy::Open);
    }

    #[test]
    fn a_library_caller_registers_only_a_halo_address_other_members_dial() {
        assert_eq!(
            halo_address("n0.example.org:9100").unwrap(),
            "n0.example.org:9100"
        );
        for unregistrable in [
            "127.0.0.1:9100",
            "0.0.0.0:9100",
            "[::1]:9100",
            "localhost:9100",
        ] {
            refused_for_its_halo(halo_address(unregistrable));
            let dir = tempfile::tempdir().unwrap();
            let file = FILE
                .replace("HOOKS", "")
                .replace("ROLES", "")
                .replace("n0.example.org:1", unregistrable);
            let founding: Founding = toml::from_str(&file).unwrap();
            refused_for_its_halo(
                founding
                    .genesis(dir.path())
                    .map(|genesis| genesis.validators),
            );
            let mut unseated: Founding =
                toml::from_str(&file.replace(&format!("halo = \"{unregistrable}\"\n"), ""))
                    .unwrap();
            refused_for_its_halo(unseated.seat_halo(&[0], unregistrable));
        }
    }

    #[test]
    fn a_machine_key_is_private_reads_back_and_is_never_the_identity_key() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::at(dir.path());
        commonware_runtime::deterministic::Runner::default().start(|mut context| async move {
            let identity = workspace.identity_or_create(&mut context).unwrap();
            let machine = workspace.machine_key_or_create(&mut context).unwrap();
            assert_ne!(machine.public_key(), identity.public_key());
            let again = workspace.machine_key_or_create(&mut context).unwrap();
            assert_eq!(again.public_key(), machine.public_key());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(dir.path().join("machine.key"))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            std::fs::copy(
                dir.path().join("identity.key"),
                dir.path().join("machine.key"),
            )
            .unwrap();
            assert!(matches!(
                workspace.machine_key(),
                Err(Error::MachineIsIdentity)
            ));
            let refused = workspace.machine_key_or_create(&mut context);
            assert!(matches!(refused, Err(Error::MachineIsIdentity)));
        });
    }
}
