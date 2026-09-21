//! THE ducktape home resolution: `$DUCKTAPE_HOME` when set to a non-empty
//! value, else `$HOME/.ducktape`.
//!
//! One function in a zero-dependency leaf, so every reader resolves the same
//! root and a set-but-empty override reads as unset everywhere. The home
//! holds one directory per workspace and nothing else; what a workspace holds
//! is `noded::Workspace`'s to say.

use std::ffi::OsString;
use std::path::PathBuf;

/// the directory this operator's workspaces live in: `$DUCKTAPE_HOME` when
/// the override is set to a non-empty value (tests, portable setups), else
/// `~/.ducktape`.
///
/// set-but-empty is unset: honouring an empty value would resolve every root
/// under it to a RELATIVE path in whatever directory the process happened to
/// start in.
pub fn root() -> Result<PathBuf, String> {
    root_from(std::env::var_os("DUCKTAPE_HOME"), std::env::var_os("HOME"))
}

/// the resolution above with both variables passed in, so a test covers set /
/// set-but-empty / unset without mutating this process's environment.
fn root_from(root: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, String> {
    let overridden = root.filter(|root| !root.is_empty());
    match overridden {
        Some(root) => Ok(PathBuf::from(root)),
        None => {
            let home =
                home.ok_or("cannot resolve the ducktape home — set $DUCKTAPE_HOME or $HOME")?;
            Ok(PathBuf::from(home).join(".ducktape"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty-means-unset is the half that is easy to get wrong and the half a
    /// shell reader gets for free: with `DUCKTAPE_HOME=` exported, a resolver
    /// that only asks "is it set" hands back a relative path in whatever
    /// directory the process happened to start in.
    #[test]
    fn takes_an_override_and_reads_an_empty_one_as_unset() {
        let home = Some(OsString::from("/home/duck"));
        let default_root = PathBuf::from("/home/duck/.ducktape");
        assert_eq!(
            root_from(Some(OsString::from("/srv/duck")), home.clone()),
            Ok(PathBuf::from("/srv/duck")),
            "a non-empty override is the root"
        );
        assert_eq!(
            root_from(Some(OsString::new()), home.clone()),
            Ok(default_root.clone()),
            "set-but-empty is unset, the way the shell readers take it"
        );
        assert_eq!(root_from(None, home), Ok(default_root), "the default");
        let neither = root_from(None, None).expect_err("no root without either variable");
        assert!(
            neither.contains("DUCKTAPE_HOME") && neither.contains("HOME"),
            "the error names both variables an operator can set: {neither}"
        );
    }
}
