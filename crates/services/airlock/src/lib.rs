//! The lender half of the airlock: a node-local credential store, and the
//! gateway that serves it.
//!
//! ## the store
//!
//! One directory per credential under `<storage>/airlock-creds/<name>/`, holding
//! the vendor's own login artifact (`.credentials.json` for claude, `auth.json`
//! for codex; for apple-codesign the four files [`apple_codesign_files`]
//! names) plus a `kind` marker naming which. `ducktape user cred add` writes
//! them; nothing here authors a credential.
//!
//! Beside them sits `seal.key` (0600): the x25519 secret this gateway seals
//! session tokens under. Its PUBLIC half is what `cred add` publishes on
//! consensus and what a borrower's broker pins, so it must be STABLE across
//! restarts — hence disk, not memory.
//!
//! ## what this crate does NOT do
//!
//! No consensus reads: the grant gate is injected as an [`airlock::server::GrantCheck`]
//! by whoever owns a lane to the node. No route publication: that is a signed
//! ownership act by the user. No listener binding: the caller owns the port, so
//! it can register it before serving. Those three are `bin/node`'s, and keeping
//! them out is what makes everything here testable with a tempdir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use airlock::seal::SealKeypair;
use airlock::server::{AttestMode, GatewayConfig, GrantCheck, ReloadCredential, StoreLoad};
use airlock::wire::{CredentialKind, CredentialPayload};

/// how long a scoped session token this gateway mints stays valid.
const SESSION_TTL_SECS: u64 = 3600;

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const OPENAI_BASE: &str = "https://chatgpt.com/backend-api/codex";
const OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// The credential store root: one dir per credential under
/// `<storage>/airlock-creds/`, plus `seal.key` at the top.
pub fn cred_store_root(storage: &Path) -> PathBuf {
    storage.join("airlock-creds")
}

/// Everything the daemon serves from, resolved from disk before anything runs.
pub struct Store {
    root: PathBuf,
    seal: SealKeypair,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
}

impl Store {
    /// Open (creating on first use) the store under `<storage>/airlock-creds/`.
    ///
    /// An EMPTY store is a normal, serving state — not a reason to stay down:
    /// the seal keypair is minted here so `cred add` has a stable public key to
    /// publish, and the lazy loader serves whatever is written afterwards
    /// without a restart.
    pub fn open(storage: &Path) -> Result<Self, String> {
        let root = cred_store_root(storage);
        std::fs::create_dir_all(&root)
            .map_err(|error| format!("create airlock store {}: {error}", root.display()))?;
        let seal = load_or_create_seal_keypair(&root)?;
        let seeds = load_seeds(&root)?;
        Ok(Self { root, seal, seeds })
    }

    /// how many credentials the store held at open. A count, never a name — a
    /// credential name is operator-chosen and ends up in logs otherwise.
    pub fn len(&self) -> usize {
        self.seeds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seeds.is_empty()
    }

    /// the seal PUBLIC key this gateway seals under — the value a borrower's
    /// broker pins from the on-chain credential record.
    pub fn seal_pk(&self) -> [u8; 32] {
        self.seal.public_bytes()
    }

    /// Build the gateway router. `grant_check` is the committed-state gate: a
    /// session claiming an account that is neither the owner nor a grantee is
    /// refused HERE, at the owner's own gateway, not only at the borrower's
    /// broker. It is mandatory — a lending gateway that cannot prove a grant
    /// must lend nothing, so there is no ungated build path.
    pub fn router(self, grant_check: GrantCheck) -> Result<axum::Router, String> {
        let cfg = GatewayConfig {
            attest: AttestMode::SelfHost,
            seal_keypair: Some(self.seal),
            anthropic_base: env_or("DUCKTAPE_AIRLOCK_ANTHROPIC_BASE", ANTHROPIC_BASE),
            openai_base: env_or("DUCKTAPE_AIRLOCK_OPENAI_BASE", OPENAI_BASE),
            oauth_token_url: env_or("DUCKTAPE_AIRLOCK_OAUTH_TOKEN_URL", OAUTH_TOKEN_URL),
            oauth_client_id: env_or("DUCKTAPE_AIRLOCK_OAUTH_CLIENT_ID", OAUTH_CLIENT_ID),
            session_ttl_secs: SESSION_TTL_SECS,
            clock: airlock::server::Clock::system(),
            // the self-host lender holds no `rcodesign`; the enclave image does.
            sign: None,
        };
        let (router, _vendor) = airlock::server::build_self_host_reloadable(
            cfg,
            self.seeds,
            Some(grant_check),
            reload_from_store(&self.root),
        )
        .map_err(|error| format!("airlock gateway: {error}"))?;
        Ok(router)
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var_os(key)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The lazy store loader the running gateway consults before every session and
/// every proxied request.
///
/// It reports what the store holds for a name against what it last handed over,
/// in one stat: [`StoreLoad::Loaded`] for an artifact the live entry was NOT
/// built from (first sight of a name, or one whose mtime moved),
/// [`StoreLoad::Unchanged`] for the one already being served, and
/// [`StoreLoad::Absent`] when there is no artifact at all. All three
/// restart-free cases ride that: a credential `cred add` wrote after boot, a
/// re-login that ROTATED one in place, and one `cred remove`/`revoke` deleted —
/// which the gateway must stop serving to the sessions already holding a token
/// for it.
///
/// ponytail: mtime, not a content hash. A rotation that preserves mtime to the
/// nanosecond is not a thing a vendor CLI does; hash the artifact if one ever
/// starts.
fn reload_from_store(root: &Path) -> ReloadCredential {
    let root = root.to_path_buf();
    let seen: Mutex<HashMap<String, SystemTime>> = Mutex::new(HashMap::new());
    Arc::new(move |name: &str| {
        // The name is the session's caller-supplied `sub`, and this runs BEFORE
        // the grant gate — so it is a trust boundary. A credential is one plain
        // directory under the store root and nothing else: `..`, an absolute
        // path or any separator is refused rather than joined. The store holds
        // nothing under such a name, which is exactly `Absent`.
        if !is_store_dir_name(name) {
            return StoreLoad::Absent;
        }
        let dir = root.join(name);
        let Some(stamp) = artifact_mtime(&dir) else {
            // No dir, or an empty one: the operator removed or revoked the
            // credential. Forget the stamp too, so a later re-add of the same
            // name reads as first sight rather than as an unchanged artifact.
            if let Ok(mut stamps) = seen.lock() {
                stamps.remove(name);
            }
            return StoreLoad::Absent;
        };
        // A poisoned stamp map cannot tell live from stale, and guessing either
        // way is a credential decision: say nothing changed and serve on.
        let Ok(mut stamps) = seen.lock() else {
            return StoreLoad::Unchanged;
        };
        let unchanged = stamps.get(name) == Some(&stamp);
        if unchanged {
            return StoreLoad::Unchanged;
        }
        // A dir that is there but half-written (no `kind` marker yet, an
        // artifact mid-rewrite) is not a removal: leave the live entry alone and
        // leave the stamp unrecorded, so the next call tries again.
        let Some((kind, payload)) = load_cred_dir(&dir) else {
            return StoreLoad::Unchanged;
        };
        stamps.insert(name.to_string(), stamp);
        StoreLoad::Loaded(kind, payload)
    })
}

/// Whether `name` addresses one directory directly inside the store root.
/// Exactly one normal path component — no separators, no `.`, no `..`, no root.
fn is_store_dir_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    let Some(std::path::Component::Normal(only)) = components.next() else {
        return false;
    };
    components.next().is_none() && only == std::ffi::OsStr::new(name)
}

/// The newest mtime among a credential dir's files — the artifact and its `kind`
/// marker both count, since `cred add` writes the marker last.
fn artifact_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(modified) = entry.ok()?.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        newest = Some(newest.map_or(modified, |old: SystemTime| old.max(modified)));
    }
    newest
}

/// How many credentials the store holds, WITHOUT opening one.
///
/// A caller that wants only the NUMBER must never take the [`load_seeds`] path
/// to get it: that parses every vendor login artifact into a live refresh/access
/// token inside the asking process, and logs the operator-chosen credential NAME
/// for each incomplete dir. `ducktape user cred add` writes the `kind` marker
/// last, so a dir carrying one is a registered credential and a `read_dir` is
/// the whole answer.
///
/// A store root that cannot be read counts ZERO — an unreadable store is not
/// evidence of lending.
pub fn count_credentials(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("kind").is_file())
        .count()
}

/// Load every credential in the store as a gateway seed. A dir missing its `kind`
/// marker or its login artifact is SKIPPED with a warn (never a hard error — one
/// broken credential must not stop the rest being served). A missing store root
/// is simply an empty store. Order-stable by name so boot is deterministic.
pub fn load_seeds(root: &Path) -> Result<Vec<(String, CredentialKind, CredentialPayload)>, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("read {}: {err}", root.display())),
    };
    let mut seeds = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", root.display()))?;
        let path = entry.path();
        let is_cred_dir = path.is_dir();
        if !is_cred_dir {
            continue; // seal.key and any stray files
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        match load_cred_dir(&path) {
            Some((kind, payload)) => seeds.push((name, kind, payload)),
            None => tracing::warn!(
                target: "ducktape::gateway",
                reason = "airlock_cred_incomplete",
                credential = %name,
                "airlock credential dir skipped: missing kind marker or login artifact"
            ),
        }
    }
    seeds.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(seeds)
}

/// One credential dir → its seed, or `None` when incomplete. The `kind` marker
/// selects which artifact to read: claude's `.credentials.json` yields a rotating
/// `Refresh`, codex's `auth.json` a static `Bearer`, apple-codesign's four
/// files the signing identity the gateway re-validates at admission.
fn load_cred_dir(dir: &Path) -> Option<(CredentialKind, CredentialPayload)> {
    let kind = read_kind(dir)?;
    let payload = match kind {
        CredentialKind::Claude => claude_refresh_payload(dir)?,
        CredentialKind::Codex => codex_bearer_payload(dir)?,
        CredentialKind::AppleCodesign => apple_codesign_payload(dir)?,
    };
    Some((kind, payload))
}

/// The `kind` marker token for each kind — what `cred add` writes and
/// [`load_cred_dir`] reads back.
pub fn kind_token(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::Claude => "claude",
        CredentialKind::Codex => "codex",
        CredentialKind::AppleCodesign => "apple-codesign",
    }
}

fn read_kind(dir: &Path) -> Option<CredentialKind> {
    let raw = std::fs::read_to_string(dir.join("kind")).ok()?;
    match raw.trim() {
        "claude" => Some(CredentialKind::Claude),
        "codex" => Some(CredentialKind::Codex),
        "apple-codesign" => Some(CredentialKind::AppleCodesign),
        _ => None,
    }
}

/// The files an `apple-codesign` credential dir holds beside its `kind`
/// marker, each written 0600 by `cred add`: the Developer ID Application
/// identity as PKCS#12, its password, the App Store Connect key JSON, and the
/// Team ID the identity was enrolled under.
pub struct AppleCodesignFiles {
    pub p12: &'static str,
    pub p12_password: &'static str,
    pub api_key: &'static str,
    pub team_id: &'static str,
}

pub const fn apple_codesign_files() -> AppleCodesignFiles {
    AppleCodesignFiles {
        p12: "identity.p12",
        p12_password: "p12.password",
        api_key: "api-key.json",
        team_id: "team-id",
    }
}

/// The signing identity out of its four files, as the sealed upload would
/// carry it. Every file is required: a dir missing one is incomplete, not a
/// credential with a defaulted field. The gateway's admission (`codesign`)
/// re-runs every check on adopt, so a file edited by hand after `cred add` is
/// refused there rather than served.
fn apple_codesign_payload(dir: &Path) -> Option<CredentialPayload> {
    use base64::Engine as _;
    let files = apple_codesign_files();
    let p12 = std::fs::read(dir.join(files.p12)).ok()?;
    let p12_password = std::fs::read_to_string(dir.join(files.p12_password)).ok()?;
    let api_key_json = std::fs::read_to_string(dir.join(files.api_key)).ok()?;
    let team_id = std::fs::read_to_string(dir.join(files.team_id)).ok()?;
    Some(CredentialPayload::AppleCodesign {
        p12_b64: base64::engine::general_purpose::STANDARD.encode(p12),
        p12_password: p12_password.trim_end_matches(['\r', '\n']).to_string(),
        api_key_json,
        team_id: team_id.trim().to_string(),
    })
}

/// The claude login artifact (`.credentials.json`, `claudeAiOauth`) as a
/// refresh credential carrying the CURRENT access token + its expiry alongside
/// the rotating refresh token. Seeding the live access token means the gateway
/// serves it as-is until it expires — no refresh fires meanwhile, so the owner's
/// own local login (sharing the refresh chain) is not rotation-invalidated
/// during that window. `expiresAt` is epoch MILLISECONDS in the artifact.
fn claude_refresh_payload(dir: &Path) -> Option<CredentialPayload> {
    let raw = std::fs::read_to_string(dir.join(".credentials.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let oauth = &json["claudeAiOauth"];
    let refresh_token = oauth["refreshToken"]
        .as_str()
        .filter(|value| !value.is_empty())?
        .to_string();
    let access_token = oauth["accessToken"].as_str().unwrap_or("").to_string();
    let expires_at = oauth["expiresAt"].as_u64().map(|ms| ms / 1000).unwrap_or(0);
    Some(CredentialPayload::Refresh {
        refresh_token,
        access_token,
        expires_at,
    })
}

/// The access token out of a codex login artifact (`auth.json`,
/// `tokens.access_token`, mirroring the host-codex broker read) — codex is
/// bearer-only, no rotation.
fn codex_bearer_payload(dir: &Path) -> Option<CredentialPayload> {
    let raw = std::fs::read_to_string(dir.join("auth.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let token = json["tokens"]["access_token"]
        .as_str()
        .filter(|value| !value.is_empty())?;
    Some(CredentialPayload::Bearer {
        access_token: token.to_string(),
    })
}

/// Load the store's seal keypair, minting and persisting it (0600) on first use.
/// The PUBLIC key is what `cred add` publishes on consensus and the borrower's
/// broker pins, so this secret must be STABLE across boots — hence disk.
pub fn load_or_create_seal_keypair(root: &Path) -> Result<SealKeypair, String> {
    let path = root.join("seal.key");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let secret: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                format!(
                    "{}: seal.key must be 32 bytes, got {}",
                    path.display(),
                    bytes.len()
                )
            })?;
            Ok(SealKeypair::from_secret_bytes(secret))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
            let keypair = SealKeypair::generate();
            write_secret_0600(&path, &keypair.secret_bytes())?;
            Ok(keypair)
        }
        Err(err) => Err(format!("read {}: {err}", path.display())),
    }
}

/// Write secret bytes to a fresh 0600 file (mirrors `userkey::write_user_key_new`).
/// `pub` so `bin/node`'s `cred add` can write the vendor OAuth artifact through
/// the same hardened path `seal.key` uses, instead of `std::fs::copy` (which
/// replicates the SOURCE file's mode onto the destination).
pub fn write_secret_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    if let Err(e) = std::io::Write::write_all(&mut file, bytes) {
        let _ = std::fs::remove_file(path);
        return Err(format!("write {}: {e}", path.display()));
    }
    Ok(())
}

/// Create a directory only this user can read (mirrors `provider-host`'s
/// `create_private_dir`). Used for `airlock-creds/` and each credential's
/// `<name>/` subdirectory: the vendor OAuth artifact inside is worth strictly
/// more than the seal key beside it, so it gets the same 0700 dir + 0600 file
/// treatment.
pub fn create_private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restrict {} permissions: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn seed_claude(root: &Path, name: &str, refresh: &str) {
        let dir = root.join(name);
        write(&dir.join("kind"), "claude\n");
        write(
            &dir.join(".credentials.json"),
            &format!(r#"{{"claudeAiOauth":{{"refreshToken":"{refresh}"}}}}"#),
        );
    }

    fn seed_codex(root: &Path, name: &str, access: &str) {
        let dir = root.join(name);
        write(&dir.join("kind"), "codex\n");
        write(
            &dir.join("auth.json"),
            &format!(r#"{{"tokens":{{"access_token":"{access}"}}}}"#),
        );
    }

    /// Move a file's mtime a minute into the future, so "the artifact changed"
    /// is a fact the test states rather than one it hopes the clock provides.
    fn stamp_forward(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let ahead =
            file.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(60);
        file.set_times(std::fs::FileTimes::new().set_modified(ahead))
            .unwrap();
    }

    fn refresh_of(payload: &CredentialPayload) -> &str {
        match payload {
            CredentialPayload::Refresh { refresh_token, .. } => refresh_token,
            CredentialPayload::Bearer { .. } | CredentialPayload::AppleCodesign { .. } => {
                panic!("expected a refresh payload")
            }
        }
    }

    fn seed_apple(root: &Path, name: &str) {
        let dir = root.join(name);
        let files = apple_codesign_files();
        write(&dir.join("kind"), "apple-codesign\n");
        std::fs::write(dir.join(files.p12), b"\x30\x82p12bytes").unwrap();
        write(&dir.join(files.p12_password), "hunter2\n");
        write(
            &dir.join(files.api_key),
            r#"{"key_id":"k","issuer_id":"i","private_key":"pem"}"#,
        );
        write(&dir.join(files.team_id), "ABCDE12345\n");
    }

    #[test]
    fn apple_codesign_dir_loads_its_four_files_into_the_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_apple(&root, "release-sign");
        let seeds = load_seeds(&root).unwrap();
        assert_eq!(seeds.len(), 1);
        let (name, kind, payload) = &seeds[0];
        assert_eq!(name, "release-sign");
        assert_eq!(*kind, CredentialKind::AppleCodesign);
        let CredentialPayload::AppleCodesign {
            p12_b64,
            p12_password,
            api_key_json,
            team_id,
        } = payload
        else {
            panic!("expected the apple-codesign arm");
        };
        assert_eq!(p12_b64, "MIJwMTJieXRlcw==");
        assert_eq!(
            p12_password, "hunter2",
            "one trailing newline is the file's, not the password's"
        );
        assert!(api_key_json.contains("\"key_id\""));
        assert_eq!(team_id, "ABCDE12345");
    }

    #[test]
    fn apple_codesign_dir_missing_one_file_is_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_apple(&root, "release-sign");
        std::fs::remove_file(
            root.join("release-sign")
                .join(apple_codesign_files().team_id),
        )
        .unwrap();
        assert!(load_seeds(&root).unwrap().is_empty());
        assert_eq!(count_credentials(&root), 1, "registered, but not servable");
    }

    #[test]
    fn empty_root_yields_no_seeds() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        assert!(load_seeds(&root).unwrap().is_empty());
    }

    #[test]
    fn claude_dir_loads_a_refresh_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_claude(&root, "alice-claude-1", "rt-alice");
        let seeds = load_seeds(&root).unwrap();
        assert_eq!(seeds.len(), 1);
        let (name, kind, payload) = &seeds[0];
        assert_eq!(name, "alice-claude-1");
        assert_eq!(*kind, CredentialKind::Claude);
        assert_eq!(refresh_of(payload), "rt-alice");
    }

    #[test]
    fn codex_dir_loads_a_bearer_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_codex(&root, "alice-codex-1", "tok-codex");
        let seeds = load_seeds(&root).unwrap();
        assert_eq!(seeds.len(), 1);
        let (name, kind, payload) = &seeds[0];
        assert_eq!(name, "alice-codex-1");
        assert_eq!(*kind, CredentialKind::Codex);
        assert!(
            matches!(payload, CredentialPayload::Bearer { access_token } if access_token == "tok-codex")
        );
    }

    /// The node's boot diagnostic wants a COUNT. Taking `load_seeds` for it
    /// would materialize every credential's live tokens in the node process and
    /// log the operator-chosen names — so the count must come off `read_dir`,
    /// and this proves it does: a dir with no login artifact at all is still
    /// counted, which only a path that never opens one can do.
    #[test]
    fn credentials_are_counted_without_opening_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        assert_eq!(
            count_credentials(&root),
            0,
            "a store that does not exist lends nothing"
        );

        load_or_create_seal_keypair(&root).unwrap(); // writes seal.key beside them
        seed_claude(&root, "alice-claude-1", "rt-alice");
        seed_codex(&root, "alice-codex-1", "tok-codex");
        write(&root.join("registered-but-broken").join("kind"), "claude\n");

        assert_eq!(
            count_credentials(&root),
            3,
            "seal.key is not a credential; a broken one is"
        );
        assert_eq!(
            load_seeds(&root).unwrap().len(),
            2,
            "load_seeds knows the third is broken only because it OPENED the other two"
        );
    }

    #[test]
    fn seeds_are_order_stable_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_claude(&root, "b", "rt-b");
        seed_claude(&root, "a", "rt-a");
        seed_codex(&root, "c", "tok-c");
        let names: Vec<_> = load_seeds(&root)
            .unwrap()
            .into_iter()
            .map(|(n, ..)| n)
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn dir_missing_its_artifact_is_skipped_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        // a kind marker with no login artifact beside it
        write(&root.join("broken").join("kind"), "claude\n");
        seed_claude(&root, "good", "rt-good");
        let seeds = load_seeds(&root).unwrap();
        assert_eq!(
            seeds.len(),
            1,
            "the broken dir is skipped, the good one survives"
        );
        assert_eq!(seeds[0].0, "good");
    }

    #[test]
    fn seal_key_file_is_not_mistaken_for_a_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        let _kp = load_or_create_seal_keypair(&root).unwrap(); // writes seal.key
        seed_claude(&root, "alice-claude-1", "rt-alice");
        let seeds = load_seeds(&root).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].0, "alice-claude-1");
    }

    #[test]
    fn seal_keypair_is_created_once_and_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        let first = load_or_create_seal_keypair(&root).unwrap();
        let second = load_or_create_seal_keypair(&root).unwrap();
        assert_eq!(first.public_bytes(), second.public_bytes());
        assert_eq!(first.secret_bytes(), second.secret_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn seal_key_is_written_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        load_or_create_seal_keypair(&root).unwrap();
        let mode = std::fs::metadata(root.join("seal.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_is_0700_and_secret_write_is_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        let cred_dir = root.join("alice-claude-1");
        create_private_dir(&root).unwrap();
        create_private_dir(&cred_dir).unwrap();
        let root_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let cred_dir_mode = std::fs::metadata(&cred_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(root_mode, 0o700);
        assert_eq!(cred_dir_mode, 0o700);

        let artifact = cred_dir.join(".credentials.json");
        write_secret_0600(&artifact, b"{}").unwrap();
        let artifact_mode = std::fs::metadata(&artifact).unwrap().permissions().mode() & 0o777;
        assert_eq!(artifact_mode, 0o600);
    }

    #[test]
    fn an_empty_store_opens_and_mints_a_stable_seal_key() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Store::open(tmp.path()).unwrap();
        assert!(
            first.is_empty(),
            "an empty store is a serving state, not a failure"
        );
        let second = Store::open(tmp.path()).unwrap();
        assert_eq!(
            first.seal_pk(),
            second.seal_pk(),
            "the published seal_pk must survive a restart or every pinned record breaks"
        );
    }

    #[test]
    fn the_loader_answers_once_per_change_and_stays_quiet_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_claude(&root, "alice-claude-1", "rt-first");
        let reload = reload_from_store(&root);

        // first sight: a credential the gateway has never loaded.
        let StoreLoad::Loaded(_, first) = reload("alice-claude-1") else {
            panic!("a credential is loaded on first sight");
        };
        assert_eq!(refresh_of(&first), "rt-first");
        // unchanged on disk: no reload, so a session costs one stat and no parse.
        assert!(
            matches!(reload("alice-claude-1"), StoreLoad::Unchanged),
            "an unchanged credential must not reload"
        );

        // a re-login ROTATES the artifact. Without this the gateway would serve
        // the dead token until the daemon was restarted.
        //
        // The mtime is stamped explicitly rather than left to the clock: a
        // rewrite inside one filesystem timestamp tick would otherwise decide
        // the assertion, which is a test waiting on time.
        let dir = root.join("alice-claude-1");
        seed_claude(&root, "alice-claude-1", "rt-rotated");
        stamp_forward(&dir.join(".credentials.json"));
        let StoreLoad::Loaded(_, rotated) = reload("alice-claude-1") else {
            panic!("a rotated artifact reloads");
        };
        assert_eq!(refresh_of(&rotated), "rt-rotated");
        assert!(
            matches!(reload("alice-claude-1"), StoreLoad::Unchanged),
            "and then goes quiet again"
        );
    }

    /// `user cred remove` deletes the dir. The loader must say ABSENT, not
    /// "unchanged" — the gateway evicts on the first answer and keeps serving
    /// the deleted credential on the second.
    #[test]
    fn a_removed_credential_reads_as_absent_and_a_re_add_as_first_sight() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        seed_claude(&root, "alice-claude-1", "rt-first");
        let reload = reload_from_store(&root);
        assert!(matches!(reload("alice-claude-1"), StoreLoad::Loaded(..)));

        std::fs::remove_dir_all(root.join("alice-claude-1")).unwrap();
        assert!(
            matches!(reload("alice-claude-1"), StoreLoad::Absent),
            "a removed dir is a removal"
        );

        // the same name added again is a credential the gateway does not hold.
        seed_claude(&root, "alice-claude-1", "rt-second");
        let StoreLoad::Loaded(_, re_added) = reload("alice-claude-1") else {
            panic!("a re-added credential loads on first sight");
        };
        assert_eq!(refresh_of(&re_added), "rt-second");
    }

    #[test]
    fn an_absent_credential_never_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = cred_store_root(tmp.path());
        std::fs::create_dir_all(&root).unwrap();
        let reload = reload_from_store(&root);
        assert!(matches!(reload("ghost"), StoreLoad::Absent));
    }

    #[test]
    fn a_credential_name_is_one_plain_directory() {
        assert!(is_store_dir_name("alice-claude-1"));
        // the loader runs on a caller-supplied `sub`, BEFORE the grant gate.
        for escape in ["..", ".", "", "/etc", "../../etc", "a/b", "./a"] {
            assert!(
                !is_store_dir_name(escape),
                "{escape:?} must not address the store"
            );
        }
    }
}
