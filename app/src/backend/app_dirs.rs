//! Where the app keeps what is ITS OWN — its preferences, its log, its forge
//! mirrors — as distinct from a network's files, which live in that network's
//! workspace under the ducktape home. Nothing of the app's is stored under the
//! home: the home holds workspaces and nothing else, so two networks on one
//! machine share no file, and the app's state outlives any one of them.
//!
//! The platform's own conventions, no library: XDG on Linux
//! (`$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, else
//! `~/.config`, `~/.local/state`, `~/.cache`) and `~/Library/Application
//! Support`, `~/Library/Logs`, `~/Library/Caches` on macOS. An XDG variable
//! set explicitly wins on EVERY platform: that is how a rig or a test points
//! the app at a scratch directory without touching the real one.

use std::ffi::OsString;
use std::path::PathBuf;

const APP: &str = "ducktape";

/// preferences (`prefs.json`).
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    platform_dir("XDG_CONFIG_HOME", ".config", "Library/Application Support")
}

/// the app's log.
pub(crate) fn state_dir() -> Result<PathBuf, String> {
    platform_dir("XDG_STATE_HOME", ".local/state", "Library/Logs")
}

/// rebuildable mirrors (a forge remote's bare clone).
pub(crate) fn cache_dir() -> Result<PathBuf, String> {
    platform_dir("XDG_CACHE_HOME", ".cache", "Library/Caches")
}

/// what the launcher installs: `releases/`, `current`, `previous`
/// (`$XDG_DATA_HOME`, else `~/.local/share`; Linux only — macOS keeps
/// releases beside the update state).
pub(crate) fn data_dir() -> Result<PathBuf, String> {
    platform_dir(
        "XDG_DATA_HOME",
        ".local/share",
        "Library/Application Support",
    )
}

/// the app's rotating log, under the state directory.
pub fn app_log_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("app.log"))
}

fn platform_dir(
    xdg_var: &str,
    linux_default: &str,
    macos_default: &str,
) -> Result<PathBuf, String> {
    let default = match cfg!(target_os = "macos") {
        true => macos_default,
        false => linux_default,
    };
    resolve(
        std::env::var_os(xdg_var),
        std::env::var_os("HOME"),
        xdg_var,
        default,
    )
}

/// the rule over explicit inputs, so a test reads no process env: a
/// non-empty override names the base outright, else the platform default
/// under `$HOME`; set-but-empty is unset, like the ducktape home's own
/// override.
fn resolve(
    explicit: Option<OsString>,
    home: Option<OsString>,
    xdg_var: &str,
    default: &str,
) -> Result<PathBuf, String> {
    if let Some(base) = explicit.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(base).join(APP));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("cannot resolve the app directory — set ${xdg_var} or $HOME"))?;
    Ok(PathBuf::from(home).join(default).join(APP))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An explicit XDG variable names the directory outright — the one knob a
    /// rig has to keep the app off the real one — and an empty one is unset.
    #[test]
    fn an_explicit_xdg_variable_wins_and_an_empty_one_is_unset() {
        let explicit = resolve(
            Some("/scratch/cfg".into()),
            Some("/home/op".into()),
            "XDG_CONFIG_HOME",
            ".config",
        );
        assert_eq!(explicit.unwrap(), PathBuf::from("/scratch/cfg/ducktape"));
        let empty = resolve(
            Some("".into()),
            Some("/home/op".into()),
            "XDG_CONFIG_HOME",
            ".config",
        );
        assert_eq!(empty.unwrap(), PathBuf::from("/home/op/.config/ducktape"));
        let homeless = resolve(None, None, "XDG_CONFIG_HOME", ".config").unwrap_err();
        assert!(homeless.contains("XDG_CONFIG_HOME"), "{homeless}");
    }
}
