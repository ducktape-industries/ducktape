//! Compiles the kernel fixture programs in crates/kernel/fixtures for wasm32, into a target
//! directory of its own under OUT_DIR, so fixture-wasm hands the test suites fresh bytes.
use std::env::{self, VarError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::Deserialize;

const TARGET: &str = "wasm32-unknown-unknown";
const BUILD: &[&str] = &[
    "build",
    "--workspace",
    "--release",
    "--locked",
    "--target",
    TARGET,
];
const METADATA: &[&str] = &[
    "metadata",
    "--format-version",
    "1",
    "--locked",
    "--filter-platform",
    TARGET,
];
const OUTER_BUILD_SETTINGS: &[&str] = &[
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "RUSTFLAGS",
    "RUSTC_WORKSPACE_WRAPPER",
];

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("build environment variable {name} is not set")]
    Env {
        name: &'static str,
        #[source]
        source: VarError,
    },
    #[error("could not run cargo")]
    Spawn(#[from] io::Error),
    #[error("cargo {step} on the fixtures workspace exited with {status}")]
    Cargo {
        step: &'static str,
        status: ExitStatus,
    },
    #[error("cargo metadata printed something other than its JSON")]
    Metadata(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    source: Option<String>,
    manifest_path: PathBuf,
}

fn main() -> Result<(), Error> {
    let cargo = var("CARGO")?;
    let out = PathBuf::from(var("OUT_DIR")?);
    let fixtures = PathBuf::from(var("CARGO_MANIFEST_DIR")?).join("../fixtures");
    let manifest = fixtures.join("Cargo.toml");

    watch(&manifest);
    watch(&fixtures.join("Cargo.lock"));
    for dir in source_dirs(&cargo, &manifest)? {
        watch(&dir);
    }

    let target_dir = out.join("target");
    let status = nested(&cargo)
        .args(BUILD)
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .stdout(Stdio::from(io::stderr()))
        .status()?;
    if !status.success() {
        return Err(Error::Cargo {
            step: "build",
            status,
        });
    }

    let artifacts = target_dir.join(TARGET).join("release");
    println!("cargo::rustc-env=FIXTURE_WASM={}", artifacts.display());
    Ok(())
}

fn source_dirs(cargo: &str, manifest: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = nested(cargo)
        .args(METADATA)
        .arg("--manifest-path")
        .arg(manifest)
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(Error::Cargo {
            step: "metadata",
            status: output.status,
        });
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let local = metadata
        .packages
        .into_iter()
        .filter(|package| package.source.is_none());
    let dirs = local.filter_map(|package| package.manifest_path.parent().map(Path::to_path_buf));
    Ok(dirs.collect())
}

fn nested(cargo: &str) -> Command {
    let mut command = Command::new(cargo);
    for setting in OUTER_BUILD_SETTINGS {
        command.env_remove(setting);
    }
    command
}

fn watch(path: &Path) {
    println!("cargo::rerun-if-changed={}", path.display());
}

fn var(name: &'static str) -> Result<String, Error> {
    env::var(name).map_err(|source| Error::Env { name, source })
}
