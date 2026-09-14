//! Where everything is, from explicit inputs, so a test reads no process env.
//!
//! Both platforms keep `state.json` (and the macOS swap journal) under the
//! app's config dir — `$XDG_CONFIG_HOME/ducktape/updates`, else
//! `~/.config/ducktape/updates` on Linux and `~/Library/Application
//! Support/ducktape/updates` on macOS; an explicitly set XDG variable wins on
//! every platform, exactly as `app/src/backend/app_dirs.rs` resolves it, so
//! the app and the launcher agree on one file.
//!
//! Linux (`<data>` = `$XDG_DATA_HOME/ducktape`, else `~/.local/share/ducktape`):
//! ```text
//! <data>/releases/<sha>/{ducktape-launcher, ducktape-app, views/*.wasm}
//! <data>/current  -> releases/<sha>     # the install path; flipped by rename(2)
//! <data>/previous -> releases/<sha>
//! ```
//! macOS (`<install>` = `$DUCKTAPE_INSTALL_DIR`, else `/Applications`):
//! ```text
//! <install>/Ducktape.app                 # the CURRENT release, whole
//! <updates>/releases/<sha>/Ducktape.app  # previous and staged releases
//! <updates>/previous -> releases/<sha>
//! ```
//! Every path here is absolute under those roots and never relative to the
//! running bundle: App Translocation may run a quarantined bundle from a
//! randomised path, and the launcher must still find the same state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use app_update::Sha;

const APP: &str = "ducktape";
pub const BUNDLE: &str = "Ducktape.app";
pub const APP_EXE: &str = "ducktape-app";
pub const LAUNCHER_EXE: &str = "ducktape-launcher";
pub const VIEWS_DIR: &str = "views";
pub const STATE_FILE: &str = "state.json";
const JOURNAL_FILE: &str = "swap.json";
const RELEASES_DIR: &str = "releases";
const CURRENT_LINK: &str = "current";
const PREVIOUS_LINK: &str = "previous";
const DESKTOP_ENTRY: &str = "dev.ducktape.app.desktop";
const MACOS_INSTALL_DIR: &str = "/Applications";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

impl Platform {
    pub const HOST: Platform = match cfg!(target_os = "macos") {
        true => Platform::MacOs,
        false => Platform::Linux,
    };
}

/// The process environment the layout reads, captured once.
#[derive(Debug, Default, Clone)]
pub struct EnvInputs {
    pub xdg_config_home: Option<OsString>,
    pub xdg_data_home: Option<OsString>,
    pub home: Option<OsString>,
    pub install_dir: Option<OsString>,
}

impl EnvInputs {
    pub fn from_process() -> Self {
        EnvInputs {
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            home: std::env::var_os("HOME"),
            install_dir: std::env::var_os("DUCKTAPE_INSTALL_DIR"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub platform: Platform,
    /// `<config>/ducktape/updates`.
    pub updates: PathBuf,
    /// Linux: `<data>/ducktape`, holding `releases/`, `current`, `previous`.
    /// macOS: the directory holding the installed `Ducktape.app`.
    pub install_dir: PathBuf,
    /// Linux only: `$XDG_DATA_HOME` or `~/.local/share`, for the desktop entry.
    data_home: PathBuf,
}

impl Layout {
    pub fn resolve(platform: Platform, env: &EnvInputs) -> Result<Layout, String> {
        let config = match platform {
            Platform::Linux => base_dir(&env.xdg_config_home, &env.home, ".config")?,
            Platform::MacOs => base_dir(
                &env.xdg_config_home,
                &env.home,
                "Library/Application Support",
            )?,
        };
        let data_home = base_dir(&env.xdg_data_home, &env.home, ".local/share")?;
        let install_dir = match platform {
            Platform::Linux => data_home.join(APP),
            Platform::MacOs => env
                .install_dir
                .as_ref()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(MACOS_INSTALL_DIR)),
        };
        Ok(Layout {
            platform,
            updates: config.join(APP).join("updates"),
            install_dir,
            data_home,
        })
    }

    pub fn state_path(&self) -> PathBuf {
        self.updates.join(STATE_FILE)
    }

    /// macOS: what the flip wrote before `RENAME_SWAP`, so a boot in
    /// `Swapping` can tell which bundle landed.
    pub fn journal_path(&self) -> PathBuf {
        self.updates.join(JOURNAL_FILE)
    }

    pub fn releases_dir(&self) -> PathBuf {
        match self.platform {
            Platform::Linux => self.install_dir.join(RELEASES_DIR),
            Platform::MacOs => self.updates.join(RELEASES_DIR),
        }
    }

    pub fn release_dir(&self, sha: Sha) -> PathBuf {
        self.releases_dir().join(sha.to_string())
    }

    /// The directory holding a staged release's executables.
    pub fn release_bin_dir(&self, sha: Sha) -> PathBuf {
        match self.platform {
            Platform::Linux => self.release_dir(sha),
            Platform::MacOs => bundle_bin_dir(&self.staged_bundle(sha)),
        }
    }

    /// macOS: `<updates>/releases/<sha>/Ducktape.app`.
    pub fn staged_bundle(&self, sha: Sha) -> PathBuf {
        self.release_dir(sha).join(BUNDLE)
    }

    /// macOS: the installed bundle, which IS the current release.
    pub fn installed_bundle(&self) -> PathBuf {
        self.install_dir.join(BUNDLE)
    }

    /// Linux: the install path, a symlink into `releases/`.
    pub fn current_link(&self) -> PathBuf {
        self.install_dir.join(CURRENT_LINK)
    }

    pub fn previous_link(&self) -> PathBuf {
        match self.platform {
            Platform::Linux => self.install_dir.join(PREVIOUS_LINK),
            Platform::MacOs => self.updates.join(PREVIOUS_LINK),
        }
    }

    /// What `current`/`previous` point at: relative, so the root can move.
    pub fn link_target(sha: Sha) -> PathBuf {
        PathBuf::from(RELEASES_DIR).join(sha.to_string())
    }

    /// The executable a boot `exec`s: the install path's `ducktape-app`.
    pub fn app_exe(&self) -> PathBuf {
        match self.platform {
            Platform::Linux => self.current_link().join(APP_EXE),
            Platform::MacOs => bundle_bin_dir(&self.installed_bundle()).join(APP_EXE),
        }
    }

    /// A staged release's own launcher, for `--qualify`.
    pub fn launcher_of(&self, sha: Sha) -> PathBuf {
        self.release_bin_dir(sha).join(LAUNCHER_EXE)
    }

    /// Linux: the desktop entry that points the session at the launcher.
    pub fn desktop_entry(&self) -> PathBuf {
        self.data_home.join("applications").join(DESKTOP_ENTRY)
    }

    /// Linux: what the desktop entry's `Exec=` names.
    pub fn launcher_exec_path(&self) -> PathBuf {
        self.current_link().join(LAUNCHER_EXE)
    }
}

/// `Contents/MacOS` of a bundle.
pub fn bundle_bin_dir(bundle: &Path) -> PathBuf {
    bundle.join("Contents").join("MacOS")
}

/// A non-empty override names the base outright, else the default under
/// `$HOME`; set-but-empty is unset.
fn base_dir(
    explicit: &Option<OsString>,
    home: &Option<OsString>,
    default: &str,
) -> Result<PathBuf, String> {
    if let Some(base) = explicit.as_ref().filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(base));
    }
    let home = home
        .as_ref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "cannot resolve the app directories — set $HOME".to_string())?;
    Ok(PathBuf::from(home).join(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(config: Option<&str>, data: Option<&str>, home: Option<&str>) -> EnvInputs {
        EnvInputs {
            xdg_config_home: config.map(OsString::from),
            xdg_data_home: data.map(OsString::from),
            home: home.map(OsString::from),
            install_dir: None,
        }
    }

    #[test]
    fn linux_defaults_under_home_and_xdg_wins() {
        let layout = Layout::resolve(Platform::Linux, &env(None, None, Some("/home/op"))).unwrap();
        assert_eq!(
            layout.state_path(),
            PathBuf::from("/home/op/.config/ducktape/updates/state.json")
        );
        assert_eq!(
            layout.current_link(),
            PathBuf::from("/home/op/.local/share/ducktape/current")
        );
        assert_eq!(
            layout.desktop_entry(),
            PathBuf::from("/home/op/.local/share/applications/dev.ducktape.app.desktop")
        );
        let scratch = Layout::resolve(
            Platform::Linux,
            &env(Some("/s/cfg"), Some("/s/data"), Some("/home/op")),
        )
        .unwrap();
        assert_eq!(scratch.updates, PathBuf::from("/s/cfg/ducktape/updates"));
        assert_eq!(
            scratch.releases_dir(),
            PathBuf::from("/s/data/ducktape/releases")
        );
        let empty_is_unset =
            Layout::resolve(Platform::Linux, &env(Some(""), None, Some("/home/op")));
        assert_eq!(empty_is_unset.unwrap().updates, layout.updates);
        assert!(Layout::resolve(Platform::Linux, &env(None, None, None)).is_err());
    }

    #[test]
    fn macos_keeps_releases_beside_the_state_and_installs_to_applications() {
        let layout = Layout::resolve(Platform::MacOs, &env(None, None, Some("/Users/op"))).unwrap();
        assert_eq!(
            layout.updates,
            PathBuf::from("/Users/op/Library/Application Support/ducktape/updates")
        );
        assert_eq!(
            layout.installed_bundle(),
            PathBuf::from("/Applications/Ducktape.app")
        );
        let sha = Sha::digest(b"r");
        assert_eq!(
            layout.launcher_of(sha),
            layout
                .updates
                .join("releases")
                .join(sha.to_string())
                .join("Ducktape.app/Contents/MacOS/ducktape-launcher")
        );
        assert_eq!(layout.previous_link(), layout.updates.join("previous"));
        let mut overridden = env(None, None, Some("/Users/op"));
        overridden.install_dir = Some("/scratch/apps".into());
        let scratch = Layout::resolve(Platform::MacOs, &overridden).unwrap();
        assert_eq!(
            scratch.installed_bundle(),
            PathBuf::from("/scratch/apps/Ducktape.app")
        );
    }
}
