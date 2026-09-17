//! The wallet keystore: named encrypted user keys under `<workspace>/keys/`,
//! with ONE `active` pointer file naming the wallet every keyless verb
//! signs with. The file name IS the wallet name (`<name>.key`) — there is
//! no index to drift from the directory.
//!
//! Per workspace, like everything else a node keeps: a wallet is an identity
//! ON a network, and two networks on one machine share no file. Every
//! function takes the workspace explicitly; the one env read
//! (`DUCKTAPE_USER_KEY`, [`env_user_key`]) is the rig override that bypasses
//! the keystore altogether.

use std::path::{Path, PathBuf};

use crate::userkey;

/// the active-pointer file inside `keys/` — one line, the wallet's name.
pub const ACTIVE_FILE: &str = "active";

/// Lowercase hex for a listed pubkey.
///
/// Local on purpose: the formatter this used to call lives in the node
/// binary, and depending on that binary's crate is the coupling this one was
/// extracted to shed.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

const MAX_NAME_LEN: usize = 41;

pub fn keys_dir(workspace: &Path) -> PathBuf {
    workspace.join("keys")
}

pub fn key_file(workspace: &Path, name: &str) -> PathBuf {
    keys_dir(workspace).join(format!("{name}.key"))
}

/// `[a-z0-9][a-z0-9._-]{0,40}` — filesystem-safe, lowercase, never a path.
pub fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let tail_ok = chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
    });
    let len_ok = name.len() <= MAX_NAME_LEN;
    if head_ok && tail_ok && len_ok {
        return Ok(());
    }
    Err(format!(
        "wallet name {name:?} is invalid — use [a-z0-9][a-z0-9._-]*, at most {MAX_NAME_LEN} chars"
    ))
}

/// Fold arbitrary display text into the wallet-name charset: lowercase,
/// runs of other characters collapse to one `-`, trimmed, truncated.
/// Output always satisfies `valid_name`.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-');
        match keep {
            true => out.push(c),
            false if out.ends_with('-') || out.is_empty() => {}
            false => out.push('-'),
        }
    }
    let out = out.trim_matches('-').to_string();
    let out = out.trim_start_matches(&['.',  '_', '-'][..]).to_string();
    let mut out = match out.is_empty() {
        true => "default".to_string(),
        false => out,
    };
    out.truncate(MAX_NAME_LEN);
    out
}

/// One listed wallet. `state` is [`userkey::KeyFileState::as_str`] — the same
/// classification the app's key-state rows show (the app renders its refusal
/// plate for anything but `"encrypted"`).
pub struct WalletRow {
    pub name: String,
    pub path: PathBuf,
    pub pubkey: String,
    pub state: &'static str,
    pub active: bool,
}

/// Every `keys/*.key`, sorted by name, with the active flag applied.
pub fn list(workspace: &Path) -> Result<Vec<WalletRow>, String> {
    let keys = keys_dir(workspace);
    let mut rows = Vec::new();
    let entries = match std::fs::read_dir(&keys) {
        Ok(entries) => entries,
        Err(_) => return Ok(rows), // no keystore yet = no wallets
    };
    let active = active_name(workspace);
    for entry in entries.flatten() {
        let path = entry.path();
        let is_key_file = path.extension().is_some_and(|e| e == "key");
        if !is_key_file {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let (pubkey, state) = match userkey::read_user_key_file(&path) {
            Ok(enc) => (hex(&enc.pubkey), userkey::KeyFileState::Encrypted.as_str()),
            Err(_) => (String::new(), userkey::KeyFileState::Unreadable.as_str()),
        };
        rows.push(WalletRow {
            active: active.as_deref() == Some(name.as_str()),
            name,
            path,
            pubkey,
            state,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// The `active` pointer's content, when present and non-empty.
pub fn active_name(workspace: &Path) -> Option<String> {
    let text = std::fs::read_to_string(keys_dir(workspace).join(ACTIVE_FILE)).ok()?;
    let name = text.trim();
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Point `active` at `name` — which must exist. Atomic (tmp + rename) so a
/// concurrent reader never sees a torn pointer, and the tmp is per-process so
/// two writers (the CLI and the app activating at once) never share one inode:
/// each renames its own file, and the last rename wins whole.
pub fn set_active(workspace: &Path, name: &str) -> Result<(), String> {
    valid_name(name)?;
    if !key_file(workspace, name).exists() {
        return Err(format!("no wallet named {name:?} — see `ducktape wallet list`"));
    }
    let keys = keys_dir(workspace);
    std::fs::create_dir_all(&keys).map_err(|e| format!("create {}: {e}", keys.display()))?;
    let tmp = keys.join(format!("{ACTIVE_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{name}\n")).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, keys.join(ACTIVE_FILE)).map_err(|e| format!("activate {name}: {e}"))
}

/// The active wallet's key file. The errors ARE the onboarding: each names
/// the command that fixes it.
pub fn active_key_path(workspace: &Path) -> Result<PathBuf, String> {
    let rows = list(workspace)?;
    if rows.is_empty() {
        return Err("no wallet — run `ducktape wallet new <name>` first".into());
    }
    let Some(name) = active_name(workspace) else {
        return Err("no active wallet — run `ducktape wallet use <name>`".into());
    };
    let path = key_file(workspace, &name);
    if !path.exists() {
        return Err(format!(
            "active wallet {name:?} has no key file — run `ducktape wallet use <name>`"
        ));
    }
    Ok(path)
}

/// The rig/scripted override: `$DUCKTAPE_USER_KEY` names a key file outright,
/// and no keystore is consulted while it is set.
pub fn env_user_key() -> Option<PathBuf> {
    std::env::var_os("DUCKTAPE_USER_KEY").map(PathBuf::from)
}

/// THE key resolver every keyless CLI verb signs through: [`env_user_key`],
/// else the workspace keystore's active wallet.
pub fn active_user_key(workspace: &Path) -> Result<PathBuf, String> {
    match env_user_key() {
        Some(path) => Ok(path),
        None => active_key_path(workspace),
    }
}

// ============================================================================
// the wallet ceremonies — mint, import, activate
// ============================================================================

/// Mint a named wallet under `password`. Returns `(24 words, pubkey-hex,
/// activated)` — `activated` is false only when the mint itself succeeded but
/// the active-pointer write did not.
///
/// THE WORDS ARE THE ONLY BACKUP. Once this returns, a sealed key exists whose
/// sole recovery path is the phrase in the first field — so the pointer write
/// that follows is deliberately NOT allowed to fail the call.
pub fn create(
    workspace: &Path,
    name: &str,
    password: &str,
) -> Result<(String, String, bool), String> {
    let path = new_wallet_path(workspace, name)?;
    let (words, key) = userkey::mint_user_key(&path, password)?;
    // The doc comment above is the contract: once the key file is on disk, its
    // only recovery path is `words`, so a pointer-write failure here must
    // never swallow them by propagating an `Err` — the caller (and the
    // operator) still needs the phrase for the file this call already wrote.
    // `ducktape wallet use <name>` is always available afterward to retry the
    // pointer alone; the mint is not. The failure is surfaced (not silently
    // dropped) so an operator sees it rather than assuming activation
    // happened.
    let activated = match activate_first_wallet(workspace, name) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                target: "ducktape::auth",
                reason = "active_pointer_write_failed",
                name,
                %error,
                "minted wallet but could not activate it"
            );
            false
        }
    };
    use commonware_cryptography::Signer as _;
    Ok((words, hex(key.public_key().as_ref()), activated))
}

/// Restore a named wallet from its 24 words, sealed under a fresh `password`.
/// Returns the pubkey-hex — the same identity the words were minted as.
pub fn import(
    workspace: &Path,
    name: &str,
    mnemonic: &str,
    password: &str,
) -> Result<String, String> {
    let path = new_wallet_path(workspace, name)?;
    let key = userkey::restore_user_key_at(&path, mnemonic, password)?;
    activate_first_wallet(workspace, name)?;
    use commonware_cryptography::Signer as _;
    Ok(hex(key.public_key().as_ref()))
}

/// The active-pointer write — what `ducktape wallet use <name>` and the launch
/// window's row pick both perform.
pub fn activate(workspace: &Path, name: &str) -> Result<(), String> {
    set_active(workspace, name)
}

/// validate the name and refuse an occupied slot loudly — `write_user_key_new`
/// would refuse too, but with an io error instead of the wallet's own
/// vocabulary.
fn new_wallet_path(workspace: &Path, name: &str) -> Result<PathBuf, String> {
    valid_name(name)?;
    let path = key_file(workspace, name);
    if path.exists() {
        return Err(format!(
            "wallet {name:?} already exists — pick another name"
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    Ok(path)
}

/// the first wallet in an empty keystore becomes active; later mints never
/// steal the pointer.
fn activate_first_wallet(workspace: &Path, name: &str) -> Result<(), String> {
    if active_name(workspace).is_some() {
        return Ok(());
    }
    set_active(workspace, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mint a real encrypted key file the way `user key init` would.
    fn seed_wallet(workspace: &std::path::Path, name: &str) -> String {
        let mut seed = [0u8; 32];
        seed[0] = name.len() as u8; // distinct per name
        let line = crate::userkey::seal_user_key(&seed, "password-123").unwrap();
        let path = key_file(workspace, name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::userkey::write_user_key_new(&path, &line).unwrap();
        // through the SAME formatter `list` renders a row's pubkey with, so a
        // test expectation cannot agree with a spelling the product does not use.
        hex(&crate::userkey::read_user_key_file(&path).unwrap().pubkey)
    }

    #[test]
    fn name_charset_is_enforced() {
        assert!(valid_name("demo").is_ok());
        assert!(valid_name("a.b-c_9").is_ok());
        assert!(valid_name("").is_err());
        assert!(valid_name("Demo").is_err());
        assert!(valid_name("../evil").is_err());
        assert!(valid_name(&"x".repeat(60)).is_err());
        assert_eq!(sanitize_name("Byeongsu Hong!"), "byeongsu-hong");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name("__"), "default");
        assert!(valid_name(&sanitize_name(".hidden")).is_ok());
    }

    #[test]
    fn list_is_sorted_and_marks_active() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        seed_wallet(workspace, "beta");
        let alpha_pub = seed_wallet(workspace, "alpha");
        set_active(workspace, "alpha").unwrap();
        let rows = list(workspace).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(rows[0].active && !rows[1].active);
        assert_eq!(rows[0].pubkey, alpha_pub);
        assert_eq!(rows[0].state, "encrypted");
    }

    /// The ceremonies, driven the way the desktop app drives them — a name and
    /// a password, no pipe. The first mint takes the pointer, a later one does
    /// not steal it, and an import of the first wallet's words reproduces its
    /// identity exactly.
    #[test]
    fn create_import_activate_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();

        let (words, pubkey, activated) = create(workspace, "alice", "password-123").unwrap();
        assert_eq!(words.split_whitespace().count(), 24);
        assert_eq!(pubkey.len(), 64);
        assert!(activated);
        assert_eq!(active_name(workspace).as_deref(), Some("alice"));

        create(workspace, "bob", "password-123").unwrap();
        assert_eq!(active_name(workspace).as_deref(), Some("alice"));

        assert!(create(workspace, "alice", "password-123").is_err());
        assert!(create(workspace, "Alice", "password-123").is_err());

        activate(workspace, "bob").unwrap();
        assert_eq!(active_name(workspace).as_deref(), Some("bob"));

        let imported = import(workspace, "alice2", &words, "password-456").unwrap();
        assert_eq!(imported, pubkey);
        assert_eq!(
            list(workspace)
                .unwrap()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["alice", "alice2", "bob"]
        );
    }

    /// The mint is not allowed to be undone by a pointer-write failure: the
    /// words are the only backup for a key file `create` already wrote to
    /// disk, so a broken `active` write must not turn into a discarded
    /// `Err(..)` that hides them. Forces the failure by occupying the
    /// pointer's rename target with a directory instead of a file.
    #[test]
    fn create_returns_words_even_when_activation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let keys = keys_dir(workspace);
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::create_dir_all(keys.join(ACTIVE_FILE)).unwrap();

        let (words, pubkey, activated) = create(workspace, "alice", "password-123").unwrap();
        assert_eq!(words.split_whitespace().count(), 24);
        assert_eq!(pubkey.len(), 64);
        assert!(
            !activated,
            "the pointer write failed, so create must say so"
        );
        assert!(
            key_file(workspace, "alice").exists(),
            "the mint must still land on disk"
        );
        assert_ne!(
            active_name(workspace).as_deref(),
            Some("alice"),
            "the pointer write failed, so alice must not read back as active"
        );
    }

    #[test]
    fn set_active_refuses_an_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        seed_wallet(dir.path(), "alpha");
        assert!(set_active(dir.path(), "ghost").is_err());
    }

    /// Two workspaces are two keystores: a wallet minted in one is invisible to
    /// the other, and each `active` pointer names its own.
    #[test]
    fn keystores_are_per_workspace() {
        let home = tempfile::tempdir().unwrap();
        let (first, second) = (home.path().join("a"), home.path().join("b"));
        create(&first, "alice", "password-123").unwrap();
        create(&second, "bob", "password-123").unwrap();
        assert_eq!(active_name(&first).as_deref(), Some("alice"));
        assert_eq!(active_name(&second).as_deref(), Some("bob"));
        assert_eq!(list(&first).unwrap().len(), 1);
        assert!(
            set_active(&first, "bob").is_err(),
            "bob lives in the other workspace"
        );
    }

    #[test]
    fn active_key_path_errors_name_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let empty = active_key_path(workspace).unwrap_err();
        assert!(empty.contains("wallet new"), "{empty}");
        seed_wallet(workspace, "alpha");
        std::fs::remove_file(keys_dir(workspace).join(ACTIVE_FILE)).ok();
        let dangling = active_key_path(workspace).unwrap_err();
        assert!(dangling.contains("wallet use"), "{dangling}");
        set_active(workspace, "alpha").unwrap();
        assert_eq!(
            active_key_path(workspace).unwrap(),
            key_file(workspace, "alpha")
        );
    }

    #[test]
    fn unreadable_file_is_listed_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        seed_wallet(workspace, "alpha");
        std::fs::write(key_file(workspace, "junk"), "not a key").unwrap();
        let rows = list(workspace).unwrap();
        let junk = rows.iter().find(|r| r.name == "junk").unwrap();
        assert_eq!(junk.state, "unreadable");
        assert!(junk.pubkey.is_empty());
    }
}
