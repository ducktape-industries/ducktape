//! User key lifecycle and signing command surface.
//!
//! These commands are synchronous operator tooling. Keeping them outside the
//! live node entrypoint makes their stdin-only secret handling and output
//! contracts independently testable without mixing them into runtime boot.
//! Account membership (create, add/remove keys, name, profile) is the
//! `ducktape account` family ([`crate::account_cli`]); this family is the KEY:
//! its encrypted file, and the signatures it produces over other planes'
//! artifacts (gateway routes, submit frames, admin requests, credentials).

use std::path::PathBuf;

use commonware_codec::Encode as _;
use commonware_cryptography::{Signer as _, ed25519};

use keystore::userkey;

use crate::config;
use config::hex_bytes;

type CommandResult = Result<(), Box<dyn std::error::Error>>;

/// the typed `ducktape user` grammar — clap derive. this is only the SHAPE
/// (verbs, flags, help); the handlers below own the work. every secret
/// (password, mnemonic) still crosses the process boundary via STDIN ONLY,
/// one newline-delimited field per line — NEVER a flag, which would leak into
/// shell history / `ps`. clap enforces the required flags; numeric flags are
/// typed so clap validates them at parse.
// each variant carries its verb's typed args; parsed once and consumed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, clap::Subcommand)]
pub(crate) enum UserCmd {
    /// encrypted user key lifecycle
    Key(UserKeyArgs),
    /// sign one canonical gateway route statement
    SignGatewayRoute(GatewayRouteArgs),
    /// unlock the key once, then wrap each requested module op payload in a
    /// user-signed submit frame (hex)
    SignFrame(FrameArgs),
    /// sign one owner control-plane request (the `/v1/admin` per-request PoP)
    SignAdmin(AdminArgs),
    /// sign one gateway request as this key's account (the `x-duck-user-*`
    /// per-request PoP an `owner`/`accounts` audience route checks)
    SignCaller(CallerArgs),
    /// named, grantable API credentials co-hosted through this node's gateway
    Cred(crate::cred_cli::CredArgs),
}

/// `user key` lifecycle subcommands.
#[derive(Debug, clap::Args)]
pub(crate) struct UserKeyArgs {
    #[command(subcommand)]
    sub: UserKeyCmd,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum UserKeyCmd {
    /// generate a fresh seed, write encrypted v1, print mnemonic then pubkey
    Init(KeyOutArgs),
    /// restore from a mnemonic line, write encrypted v1, print pubkey
    Restore(KeyOutArgs),
    /// verify a key file's password; print its pubkey
    Unlock(KeyPathArgs),
    /// print the 24-word mnemonic for a key file
    Reveal(KeyPathArgs),
    /// report `absent` or `encrypted <pubkey>`
    Status(KeyPathArgs),
}

/// verbs that WRITE a new key file: the destination path (default `user.key`).
#[derive(Debug, clap::Args)]
pub(crate) struct KeyOutArgs {
    /// write the key file here
    #[arg(long, value_name = "PATH", default_value = "user.key")]
    out: PathBuf,
}

/// verbs that READ an existing key file.
#[derive(Debug, clap::Args)]
pub(crate) struct KeyPathArgs {
    /// path to the user key file
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
}

#[derive(Debug, clap::Args)]
pub(crate) struct GatewayRouteArgs {
    /// path to the user key file
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
    /// the canonical route statement (json), byte-capped
    #[arg(long, value_name = "JSON")]
    statement: String,
}

#[derive(Debug, clap::Args)]
pub(crate) struct FrameArgs {
    /// path to the user key file
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
}

/// One `<target> <seq> <payload-hex> [required-blob-hex]` request line off the signer's stdin.
/// `seq` is the frame's ordering/dedup tie-breaker (any u64); it is NOT
/// tracked in state.
struct FrameRequest {
    target: String,
    seq: u64,
    payload: Vec<u8>,
    required_blob: Option<[u8; 32]>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AdminArgs {
    /// path to the user key file
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
    /// the HTTP method of the admin request
    #[arg(long, value_name = "M")]
    method: String,
    /// the request path and query
    #[arg(long, value_name = "PATH-AND-QUERY")]
    path: String,
    /// the target node's consensus key (hex)
    #[arg(long = "node-key", value_name = "HEX")]
    node_key: String,
}

/// `user sign-caller` — every field the gateway's caller preimage binds, so
/// the proof can never be replayed against another route, publisher, method
/// or path.
#[derive(Debug, clap::Args)]
pub(crate) struct CallerArgs {
    /// path to the user key file
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
    /// the route's publisher node (hex consensus key)
    #[arg(long = "publisher-node", value_name = "HEX")]
    publisher_node: String,
    /// JSON ProxyRequestHead for the exact request (without user_pop)
    #[arg(long, value_name = "PATH")]
    head: PathBuf,
    /// Raw request body file; omitted for an empty body
    #[arg(long, value_name = "PATH")]
    body: Option<PathBuf>,
}

/// Run one verb of the `ducktape user` family. secrets cross via stdin only
/// (see the module header), so `run` opens stdin once and every handler reads
/// its secret fields from it — never from `cmd`.
pub(super) fn run(cmd: UserCmd) -> CommandResult {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    match cmd {
        UserCmd::Key(args) => cmd_user_key(args, &mut stdin),
        UserCmd::SignGatewayRoute(args) => cmd_user_sign_gateway_route(args, &mut stdin),
        UserCmd::SignFrame(args) => cmd_user_sign_frame(args, &mut stdin),
        UserCmd::SignAdmin(args) => cmd_user_sign_admin(args, &mut stdin),
        UserCmd::SignCaller(args) => cmd_user_sign_caller(args, &mut stdin),
        UserCmd::Cred(args) => crate::cred_cli::run(args, &mut stdin),
    }
}

// ============================================================================
// user-key lifecycle verbs (init/restore/unlock/reveal/encrypt/status). the
// binding stdin/stdout contract: every secret (password, mnemonic) crosses
// the process boundary via STDIN ONLY, one newline-delimited field per line
// in the order each verb documents — never argv/env, which would leak into
// shell history / `ps`. each verb below is split into
// a `user_key_*` core (takes the parsed stdin, returns the value to print —
// directly unit-testable without capturing stdout) and a thin `cmd_user_key_*`
// wrapper that prints it; the wrapper is what `run()`'s dispatch calls.
// ============================================================================

/// read one line from `stdin`, minus its trailing newline — the stdin-only
/// convention every secret field crosses the process boundary through.
/// errors only on true EOF (nothing at all, not even a newline): a caller
/// that doesn't pipe the expected field. an explicit empty line (just `\n`)
/// is NOT an error here — callers that need a non-empty value (passwords)
/// reject that on their own terms (`check_password_len`), with a clearer
/// message than a generic "missing" would give.
fn read_stdin_line(stdin: &mut impl std::io::BufRead, field: &str) -> Result<String, String> {
    let mut line = String::new();
    let n = stdin
        .read_line(&mut line)
        .map_err(|e| format!("read {field} from stdin: {e}"))?;
    if n == 0 {
        return Err(format!("missing {field} on stdin"));
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(line)
}

/// True when this process's stdin is an interactive terminal. Only then do we
/// prompt / mask — a pipe stays byte-for-byte the old plain-stdin contract.
fn stdin_is_tty() -> bool {
    // SAFETY: isatty only queries a descriptor's mode; no memory is touched.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// A field name that implies key material — echo is masked when it's typed at a
/// terminal. `payload-hex` is NOT a secret (it's a public op body) and is not
/// masked.
fn field_is_secret(field: &str) -> bool {
    field.contains("password") || field.contains("mnemonic") || field.contains("token")
}

/// RAII guard that disables terminal echo on stdin and restores it on Drop —
/// including panic unwind and early-return error paths. `engage` returns `None`
/// when stdin has no termios (restore is then a no-op), so the caller reads
/// unmasked rather than failing.
struct EchoOff {
    saved: libc::termios,
}

impl EchoOff {
    fn engage() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: termios is a plain C struct of integers/arrays; tcgetattr
        // fills the zeroed value before we read it.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        let saved = term;
        term.c_lflag &= !libc::ECHO;
        // SAFETY: term is a valid termios we just read back and edited.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return None;
        }
        Some(Self { saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: `saved` is the termios we captured; best-effort restore.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

/// Run `read` to consume one stdin field. When `is_tty`, first prompt
/// `<field>: ` to stderr and — for secret fields — mask echo for the read
/// (restoring on the way out and emitting the newline the mask swallowed).
/// When not a tty this is exactly `read()`: no prompt, no termios, so piped
/// stdin is unchanged. `is_tty` is a parameter so the non-tty path is unit
/// testable without a controlling terminal.
fn with_prompt<T>(field: &str, is_tty: bool, read: impl FnOnce() -> T) -> T {
    if !is_tty {
        return read();
    }
    eprint!("{field}: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let secret = field_is_secret(field);
    let _echo_off = if secret { EchoOff::engage() } else { None };
    let out = read();
    if secret {
        // masked echo swallowed the user's Enter — supply the missing newline.
        eprintln!();
    }
    out
}

/// [`read_stdin_line`] fronted by the tty prompt/mask wrapper — the entry point
/// every secret-bearing verb reads its fields through.
pub(crate) fn prompt_stdin_line(
    stdin: &mut impl std::io::BufRead,
    field: &str,
) -> Result<String, String> {
    with_prompt(field, stdin_is_tty(), || read_stdin_line(stdin, field))
}

/// Resolve the user signer from an encrypted key. Password is stdin's first
/// line for every signing verb.
pub(crate) fn load_user_signer(
    key_path: &std::path::Path,
    stdin: &mut impl std::io::BufRead,
) -> Result<ed25519::PrivateKey, Box<dyn std::error::Error>> {
    let password = prompt_stdin_line(stdin, "password")?;
    Ok(userkey::open_user_key_at(key_path, &password)?)
}

/// `user-key init` core — see [`cmd_user_key_init`] for the print contract.
/// returns `(mnemonic, pubkey-hex)` so tests can assert both independently.
fn user_key_init(
    args: KeyOutArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (words, key) = mint_user_key(&args.out, stdin)?;
    Ok((words, hex_bytes(key.public_key().as_ref())))
}

/// Read the password from `stdin`, then mint — the ceremony itself
/// ([`userkey::mint_user_key`]) is the library's, because the desktop app
/// performs the same one without a pipe to read it from.
pub(crate) fn mint_user_key(
    out: &std::path::Path,
    stdin: &mut impl std::io::BufRead,
) -> Result<(String, ed25519::PrivateKey), Box<dyn std::error::Error>> {
    let password = prompt_stdin_line(stdin, "password")?;
    Ok(userkey::mint_user_key(out, &password)?)
}

/// `user-key init --out <path>` — stdin: password. Generates a fresh seed,
/// writes the encrypted shape (refuses to overwrite via `create_new`), and prints the 24-word
/// mnemonic line THEN the pubkey-hex line — pubkey is the LAST stdout line
/// (the `run_verb`/`last_line` contract), mnemonic is the line before it.
fn cmd_user_key_init(args: KeyOutArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    let (words, pubkey_hex) = user_key_init(args, stdin)?;
    println!("{words}");
    println!("{pubkey_hex}");
    Ok(())
}

/// restore core over an explicit destination — `wallet import` reuses it.
pub(crate) fn restore_user_key_at(
    out: &std::path::Path,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    let mnemonic = prompt_stdin_line(stdin, "mnemonic")?;
    let password = prompt_stdin_line(stdin, "password")?;
    let key = userkey::restore_user_key_at(out, &mnemonic, &password)?;
    Ok(hex_bytes(key.public_key().as_ref()))
}

/// `user-key restore` core — see [`cmd_user_key_restore`].
fn user_key_restore(
    args: KeyOutArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    restore_user_key_at(&args.out, stdin)
}

/// `user-key restore --out <path>` — stdin: mnemonic line, then password
/// line. Validates the BIP39 checksum, writes the encrypted shape (refuses to overwrite),
/// prints the pubkey (the only stdout line).
fn cmd_user_key_restore(args: KeyOutArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    println!("{}", user_key_restore(args, stdin)?);
    Ok(())
}

/// `user-key unlock` core — see [`cmd_user_key_unlock`].
fn user_key_unlock(
    args: KeyPathArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    let password = prompt_stdin_line(stdin, "password")?;
    let key = userkey::open_user_key_at(&args.key, &password)?;
    Ok(hex_bytes(key.public_key().as_ref()))
}

/// `user-key unlock --key <path>` — stdin: password. Pure verification
/// (nothing persists); prints the pubkey on success, a clean error + nonzero
/// exit on a wrong password or a corrupt file.
fn cmd_user_key_unlock(args: KeyPathArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    println!("{}", user_key_unlock(args, stdin)?);
    Ok(())
}

/// `user-key reveal` core — see [`cmd_user_key_reveal`].
fn user_key_reveal(
    args: KeyPathArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    let password = prompt_stdin_line(stdin, "password")?;
    let key = userkey::open_user_key_at(&args.key, &password)?;
    let seed_bytes = key.encode();
    let seed: [u8; 32] = seed_bytes
        .as_ref()
        .try_into()
        .map_err(|_| "decoded key is not a 32-byte seed".to_string())?;
    Ok(userkey::mnemonic_of_seed(&seed))
}

/// `user-key reveal --key <path>` — stdin: password. Prints the 24-word
/// mnemonic — the same encoding `init`/`restore` use, so it round-trips
/// through `user-key restore` to the identical pubkey.
fn cmd_user_key_reveal(args: KeyPathArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    println!("{}", user_key_reveal(args, stdin)?);
    Ok(())
}

/// `user-key status` core — see [`cmd_user_key_status`].
fn user_key_status(args: KeyPathArgs) -> Result<String, Box<dyn std::error::Error>> {
    if !args.key.exists() {
        return Ok("absent".to_string());
    }
    let enc = userkey::read_user_key_file(&args.key)?;
    Ok(format!("encrypted {}", hex_bytes(&enc.pubkey)))
}

/// `user-key status --key <path>` — no stdin. Prints `absent` or
/// `encrypted <pubkey-hex>` without touching a password.
fn cmd_user_key_status(args: KeyPathArgs) -> CommandResult {
    println!("{}", user_key_status(args)?);
    Ok(())
}

/// `user key [init|restore|unlock|reveal|status]`.
fn cmd_user_key(args: UserKeyArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    match args.sub {
        UserKeyCmd::Init(a) => cmd_user_key_init(a, stdin),
        UserKeyCmd::Restore(a) => cmd_user_key_restore(a, stdin),
        UserKeyCmd::Unlock(a) => cmd_user_key_unlock(a, stdin),
        UserKeyCmd::Reveal(a) => cmd_user_key_reveal(a, stdin),
        UserKeyCmd::Status(a) => cmd_user_key_status(a),
    }
}

/// Sign one canonical gateway route and return a ready-to-submit `GatewayMsg`.
/// This remains a namespace-specific oracle: the CLI parses and validates the
/// complete bounded route before the user key signs it.
fn user_sign_gateway_route(
    args: GatewayRouteArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    if args.statement.len() > gateway::MAX_ROUTE_STATEMENT_JSON_BYTES {
        return Err("user-sign-gateway-route statement exceeds the byte cap".into());
    }
    let statement: gateway::RouteStatement = serde_json::from_str(&args.statement)?;
    let preimage = gateway::route_signing_preimage(&statement)?;
    let user = load_user_signer(&args.key, stdin)?;
    let message = gateway::GatewayMsg::SetRoute {
        statement,
        authorization: gateway::MemberAuthorization {
            signer: user.public_key().as_ref().to_vec(),
            signature: user
                .sign(gateway::GATEWAY_ROUTE_NS, &preimage)
                .as_ref()
                .to_vec(),
        },
    };
    Ok(String::from_utf8(gateway::encode_msg(&message)).expect("json is utf-8"))
}

fn cmd_user_sign_gateway_route(
    args: GatewayRouteArgs,
    stdin: &mut impl std::io::BufRead,
) -> CommandResult {
    println!("{}", user_sign_gateway_route(args, stdin)?);
    Ok(())
}

/// Parse one request line. Whitespace-separated, three fields and an optional blob digest — the
/// target and seq ride the line rather than flags precisely because the
/// unlock is per PROCESS and the requests are per OP.
fn parse_frame_request(line: &str) -> Result<FrameRequest, String> {
    let mut fields = line.split_whitespace();
    let (Some(target), Some(seq), Some(payload_hex), required_blob, None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Err(
            "frame request must be `<target> <seq> <payload-hex> [required-blob-hex]`".into(),
        );
    };
    Ok(FrameRequest {
        target: target.to_string(),
        seq: seq.parse().map_err(|_| format!("frame seq: {seq:?}"))?,
        payload: config::unhex(payload_hex).map_err(|e| format!("payload hex: {e}"))?,
        required_blob: required_blob
            .map(|digest| {
                duckfs_core::from_hex_32(digest).ok_or_else(|| {
                    "required blob must be 64 lowercase hexadecimal characters".to_owned()
                })
            })
            .transpose()?,
    })
}

/// The next request, or `None` at end of input — the signer's own exit
/// condition. EOF is not an error here: a caller that signed everything it
/// had closes the pipe.
fn read_frame_request(stdin: &mut impl std::io::BufRead) -> Result<Option<FrameRequest>, String> {
    let mut line = String::new();
    let read = with_prompt("frame-request", stdin_is_tty(), || {
        stdin.read_line(&mut line)
    })
    .map_err(|e| format!("read frame-request from stdin: {e}"))?;
    if read == 0 {
        return Ok(None);
    }
    parse_frame_request(&line).map(Some)
}

/// `user-sign-frame` core — see [`cmd_user_sign_frame`].
fn user_sign_frame(
    args: FrameArgs,
    stdin: &mut impl std::io::BufRead,
    out: &mut impl std::io::Write,
) -> CommandResult {
    // stdin order: password first, then one request line per frame. The
    // payload is not a secret; it rides stdin because a 1 MiB chunk frame
    // would blow past OS argv limits.
    let user = load_user_signer(&args.key, stdin)?;
    while let Some(request) = read_frame_request(stdin)? {
        let frame = node::encode_frame_with_blob(
            &user,
            request.seq,
            &sdk::Msg {
                target: request.target,
                payload: request.payload,
            },
            request.required_blob,
        );
        writeln!(out, "{}", hex_bytes(&frame))?;
        out.flush()?;
    }
    Ok(())
}

/// ONE module op wrapped in a frame `user` signed — the bytes every
/// user-authored submit in this binary POSTs to `/v1/submit/frame`
/// ([`crate::node_http::submit_frame`]). The frame's verified signer becomes
/// the op's `Origin::External`, which is how an identity, gateway or saga op
/// is attributed to `user`'s account. `seq` is the frame's ordering/dedup
/// tie-breaker: a fresh one per call, so resubmitting the same payload never
/// trips the consensus lane's content-digest replay guard.
pub(crate) fn user_frame(user: &ed25519::PrivateKey, target: &str, payload: Vec<u8>) -> Vec<u8> {
    user_frame_at(user, frame_seq(), target, payload)
}

/// the `seq` a one-shot CLI frame carries: wall-clock nanoseconds, so two
/// frames from one key never collide as a byte-identical replay.
pub(crate) fn frame_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// [`user_frame`] at an explicit `seq` — the `sign-frame` verb takes the seq
/// off its request line so a caller holding one signer can order its own frames.
fn user_frame_at(user: &ed25519::PrivateKey, seq: u64, target: &str, payload: Vec<u8>) -> Vec<u8> {
    node::encode_frame(
        user,
        seq,
        &sdk::Msg {
            target: target.to_string(),
            payload,
        },
    )
}

/// `user-sign-frame --key <path>` — stdin: one password line, then one
/// `<target> <seq> <payload-hex> [required-blob-hex]` request line per frame; stdout: one frame
/// hex line per request, in order.
///
/// Wraps each payload in a `node` op frame signed by the user key. POSTed raw
/// to `/v1/submit/frame`, the frame's verified signer becomes the op's
/// `Origin::External` — the authenticated authorship the frameless
/// `/v1/submit`'s plaintext `origin` convention cannot provide. `seq` is the
/// frame's ordering/dedup tie-breaker (same-payload resubmits need a fresh seq
/// to survive the consensus lane's content-digest replay guard).
///
/// The loop is the point: opening the key costs one argon2id pass over 64 MiB
/// (`userkey::open_user_key`), so the desktop app keeps ONE of these alive for
/// the session and writes another request line per write. A per-op process
/// paid that KDF on every reaction tap.
fn cmd_user_sign_frame(args: FrameArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    user_sign_frame(args, stdin, &mut std::io::stdout().lock())
}

/// `user-sign-admin` core — see [`cmd_user_sign_admin`].
///
/// signs one owner control-plane request: the per-request PoP the
/// node's `/v1/admin/*` gate checks under `Public` exposure. the signed bytes
/// are `noded::admin::sign_admin`'s — the SAME function the verifier uses, so
/// the two can never drift. the freshness timestamp is minted here and returned
/// alongside the signature so the caller stamps the exact `ts` that was signed.
fn user_sign_admin(
    args: AdminArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    // the TARGET node's consensus key (hex): folded into the signed bytes so
    // this signature can never be replayed against another node.
    let node_key = config::unhex(&args.node_key).map_err(|e| format!("--node-key hex: {e}"))?;

    // stdin: password only — there is no payload.
    let user = load_user_signer(&args.key, stdin)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sig = noded::admin::sign_admin(&user, &args.method, &args.path, &node_key, ts);
    let out = serde_json::json!({
        "key": hex_bytes(user.public_key().as_ref()),
        "ts": ts.to_string(),
        "sig": hex_bytes(sig.as_ref()),
    });
    Ok(out.to_string())
}

/// `user-sign-admin --key <path> --method <M> --path <path-and-query>
/// --node-key <hex>` — stdin: password line.
/// Prints one JSON line `{"key","ts","sig"}` the app turns into the
/// `x-ducktape-admin-*` headers of an owner control request. Fresh `ts` per
/// call (replay-bounded); node-key-bound (no cross-node replay).
fn cmd_user_sign_admin(args: AdminArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    println!("{}", user_sign_admin(args, stdin)?);
    Ok(())
}

/// `user-sign-caller` core — see [`cmd_user_sign_caller`].
///
/// signs one gateway request as this key's ACCOUNT: the per-request PoP the
/// publisher checks before an `owner`/`accounts` audience admits the caller
/// (`gateway::caller_pop_preimage`, the SAME preimage the verifier rebuilds,
/// under `GATEWAY_CALLER_NS`). Fresh `ts` per call — the publisher accepts it
/// for 30 s — returned alongside so the caller stamps the exact `ts` signed.
fn user_sign_caller(
    args: CallerArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, Box<dyn std::error::Error>> {
    let publisher_node =
        config::unhex(&args.publisher_node).map_err(|e| format!("--publisher-node hex: {e}"))?;
    let head = gateway::decode_proxy_request_head(&std::fs::read(&args.head)?)?;
    let body = match args.body {
        Some(path) => std::fs::read(path)?,
        None => Vec::new(),
    };
    let user = load_user_signer(&args.key, stdin)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let preimage =
        gateway::caller_pop_preimage(&publisher_node, &head, &gateway::body_digest(&body), ts);
    let sig = user.sign(gateway::GATEWAY_CALLER_NS, &preimage);
    let out = serde_json::json!({
        "key": hex_bytes(user.public_key().as_ref()),
        "ts": ts.to_string(),
        "sig": hex_bytes(sig.as_ref()),
    });
    Ok(out.to_string())
}

/// `user sign-caller --key <path> --publisher-node <hex> --head <json-file>
/// [--body <bytes-file>]` — stdin: password line. Prints the key, timestamp
/// and signature for the exact gateway request.
fn cmd_user_sign_caller(args: CallerArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    println!("{}", user_sign_caller(args, stdin)?);
    Ok(())
}

#[cfg(test)]
mod userkey_verb_tests {
    use super::*;
    // the tests still build signers straight out of seed bytes; the verbs
    // themselves get theirs from `keystore` now.
    use commonware_codec::DecodeExt as _;
    use std::io::Cursor;

    #[test]
    fn frame_request_declares_a_signed_generic_blob() {
        let request =
            parse_frame_request(&format!("new-product 7 ff00 {}", "ab".repeat(32))).unwrap();
        assert_eq!(request.required_blob, Some([0xab; 32]));
        let signer = ed25519::PrivateKey::decode(&[9u8; 32][..]).unwrap();
        let frame = node::encode_frame_with_blob(
            &signer,
            request.seq,
            &sdk::Msg {
                target: request.target,
                payload: request.payload,
            },
            request.required_blob,
        );
        let (_, message, digest) = node::decode_frame_with_blob(&frame).unwrap();
        assert_eq!(message.target, "new-product");
        assert_eq!(message.payload, vec![255, 0]);
        assert_eq!(digest, Some([0xab; 32]));
        assert_eq!(
            parse_frame_request("new-product 7 ff00")
                .unwrap()
                .required_blob,
            None
        );
        for suffix in [
            "AB".repeat(32),
            "ab".repeat(31),
            "gg".repeat(32),
            format!("{} extra", "ab".repeat(32)),
        ] {
            assert!(parse_frame_request(&format!("new-product 7 ff00 {suffix}")).is_err());
        }
    }

    /// a Parser wrapper so tests can exercise the derived verb SHAPE (kebab
    /// spellings, parse rejection) the same way `main.rs`'s integrator will.
    #[derive(clap::Parser)]
    struct TestUserCli {
        #[command(subcommand)]
        #[allow(dead_code)]
        cmd: UserCmd,
    }

    /// a stdin double: one line per element, in order.
    fn stdin_of(lines: &[&str]) -> Cursor<Vec<u8>> {
        let mut s = String::new();
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        Cursor::new(s.into_bytes())
    }

    fn empty_stdin() -> Cursor<Vec<u8>> {
        Cursor::new(Vec::new())
    }

    const TEST_PASSWORD: &str = "test password";

    fn write_encrypted(path: &std::path::Path, seed: &[u8; 32]) {
        let line = userkey::seal_user_key(seed, TEST_PASSWORD).unwrap();
        userkey::write_user_key_new(path, &line).unwrap();
    }

    /// clap derives every verb's kebab name from its CamelCase variant; pin
    /// the exact prior spellings so a rename can't silently break a caller.
    /// The account verbs (`account-init`, `sign-bind`, the member family) are
    /// gone on purpose: membership lives under `ducktape account`.
    #[test]
    fn verb_names_keep_prior_kebab_spelling() {
        use clap::CommandFactory as _;
        let cmd = TestUserCli::command();
        for name in [
            "key",
            "sign-gateway-route",
            "sign-frame",
            "sign-admin",
            "sign-caller",
            "cred",
        ] {
            assert!(cmd.find_subcommand(name).is_some(), "verb {name} missing");
        }
        for gone in ["account-init", "sign-bind", "sign-add-member"] {
            assert!(cmd.find_subcommand(gone).is_none(), "verb {gone} lingers");
        }
    }

    #[test]
    fn init_writes_encrypted_v1_and_outputs_mnemonic_then_pubkey() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let mut stdin = stdin_of(&["correct horse battery"]);

        let (words, pubkey_hex) =
            user_key_init(KeyOutArgs { out: path.clone() }, &mut stdin).unwrap();

        assert_eq!(words.split_whitespace().count(), 24);
        assert_eq!(pubkey_hex.len(), 64);
        let enc = userkey::read_user_key_file(&path).unwrap();
        assert_eq!(hex_bytes(&enc.pubkey), pubkey_hex);
    }

    #[test]
    fn restore_round_trips_init_mnemonic_to_identical_pubkey() {
        let dir = tempfile::tempdir().unwrap();
        let init_path = dir.path().join("a.key");
        let mut init_stdin = stdin_of(&["password one"]);
        let (words, pubkey_hex) =
            user_key_init(KeyOutArgs { out: init_path }, &mut init_stdin).unwrap();

        let restore_path = dir.path().join("b.key");
        let mut restore_stdin = stdin_of(&[&words, "password two"]);
        let restored_pubkey =
            user_key_restore(KeyOutArgs { out: restore_path }, &mut restore_stdin).unwrap();

        assert_eq!(restored_pubkey, pubkey_hex);
    }

    #[test]
    fn unlock_verifies_and_rejects_wrong_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let mut init_stdin = stdin_of(&["right password"]);
        let (_, pubkey_hex) =
            user_key_init(KeyOutArgs { out: path.clone() }, &mut init_stdin).unwrap();

        let mut ok_stdin = stdin_of(&["right password"]);
        let unlocked = user_key_unlock(KeyPathArgs { key: path.clone() }, &mut ok_stdin).unwrap();
        assert_eq!(unlocked, pubkey_hex);

        let mut bad_stdin = stdin_of(&["wrong password"]);
        assert!(user_key_unlock(KeyPathArgs { key: path }, &mut bad_stdin).is_err());
    }

    #[test]
    fn reveal_returns_the_init_mnemonic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let mut init_stdin = stdin_of(&["a password"]);
        let (words, _) = user_key_init(KeyOutArgs { out: path.clone() }, &mut init_stdin).unwrap();
        let mut reveal_stdin = stdin_of(&["a password"]);
        let revealed = user_key_reveal(KeyPathArgs { key: path }, &mut reveal_stdin).unwrap();
        assert_eq!(revealed, words);
    }

    #[test]
    fn status_reports_absent_or_encrypted_and_rejects_bare_hex() {
        let dir = tempfile::tempdir().unwrap();

        let absent_path = dir.path().join("absent.key");
        assert_eq!(
            user_key_status(KeyPathArgs { key: absent_path }).unwrap(),
            "absent"
        );

        let bare_path = dir.path().join("bare.key");
        let seed = [3u8; 32];
        userkey::write_user_key_new(&bare_path, &hex_bytes(&seed)).unwrap();
        assert!(user_key_status(KeyPathArgs { key: bare_path }).is_err());

        let encrypted_path = dir.path().join("encrypted.key");
        let mut stdin = stdin_of(&["a password"]);
        let (_, init_pubkey_hex) = user_key_init(
            KeyOutArgs {
                out: encrypted_path.clone(),
            },
            &mut stdin,
        )
        .unwrap();
        assert_eq!(
            user_key_status(KeyPathArgs {
                key: encrypted_path
            })
            .unwrap(),
            format!("encrypted {init_pubkey_hex}")
        );
    }

    #[test]
    fn short_password_rejected_in_init_and_restore() {
        let dir = tempfile::tempdir().unwrap();

        let init_path = dir.path().join("init.key");
        let mut stdin = stdin_of(&["short1"]);
        assert!(
            user_key_init(
                KeyOutArgs {
                    out: init_path.clone()
                },
                &mut stdin
            )
            .is_err()
        );
        assert!(
            !init_path.exists(),
            "a rejected password must not write a file"
        );

        let words = userkey::mnemonic_of_seed(&[1u8; 32]);
        let restore_path = dir.path().join("restore.key");
        let mut stdin = stdin_of(&[&words, "short1"]);
        assert!(
            user_key_restore(
                KeyOutArgs {
                    out: restore_path.clone()
                },
                &mut stdin
            )
            .is_err()
        );
        assert!(!restore_path.exists());
    }

    #[test]
    fn sign_gateway_route_is_namespace_scoped_strict_and_decodable() {
        let dir = tempfile::tempdir().unwrap();
        let seed = [81u8; 32];
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &seed);
        let signer = ed25519::PrivateKey::decode(seed.as_slice()).unwrap();
        let statement = gateway::RouteStatement {
            chain_id: "test-chain".into(),
            account_id: 7,
            name: gateway::RouteName::named("api"),
            publisher_node: pubkey_bytes(&[82u8; 32]),
            revision: 1,
            route: Some(gateway::RouteDefinition {
                target: gateway::RouteTarget::LoopbackHttp,
                policy: gateway::RoutePolicy {
                    audience: gateway::RouteAudience::Network,
                    methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                    max_request_bytes: Some(1024),
                    max_response_bytes: 4096,
                    allow_authorization: false,
                    allow_upgrade: false,
                },
            }),
        };
        let mut stdin = stdin_of(&[TEST_PASSWORD]);
        let json = user_sign_gateway_route(
            GatewayRouteArgs {
                key: key_path.clone(),
                statement: serde_json::to_string(&statement).unwrap(),
            },
            &mut stdin,
        )
        .unwrap();
        let gateway::GatewayMsg::SetRoute {
            statement: decoded,
            authorization,
        } = gateway::decode_msg(json.as_bytes()).unwrap()
        else {
            panic!("userkey mints a SetRoute");
        };
        assert_eq!(decoded, statement);
        assert_eq!(authorization.signer, signer.public_key().as_ref());
        assert!(identity::KeyScheme::Ed25519.verify(
            &authorization.signer,
            gateway::GATEWAY_ROUTE_NS,
            &gateway::route_signing_preimage(&decoded).unwrap(),
            &authorization.signature,
        ));

        let mut unsafe_statement = statement;
        unsafe_statement.name = gateway::RouteName::named("Api.Evil");
        let mut stdin = stdin_of(&[TEST_PASSWORD]);
        assert!(
            user_sign_gateway_route(
                GatewayRouteArgs {
                    key: key_path,
                    statement: serde_json::to_string(&unsafe_statement).unwrap(),
                },
                &mut stdin,
            )
            .is_err()
        );
    }

    /// `user_frame` is THE submit envelope every account/cred/sched verb POSTs:
    /// its verified origin is the user key, the payload rides verbatim, and
    /// two frames over the same payload differ (fresh seq) so a resubmit is
    /// never a replay.
    #[test]
    fn user_frame_is_signed_by_the_user_and_carries_the_payload_verbatim() {
        let signer = ed25519::PrivateKey::decode([7u8; 32].as_slice()).unwrap();
        let payload = b"{\"create\":{\"name\":\"a\",\"scheme\":\"ed25519\"}}".to_vec();
        let frame = user_frame(&signer, "identity", payload.clone());
        let (origin, msg) = node::decode_frame(&frame).expect("frame verifies");
        assert_eq!(
            origin,
            sdk::Origin::External(signer.public_key().as_ref().to_vec())
        );
        assert_eq!(msg.target, "identity");
        assert_eq!(msg.payload, payload);
        assert_ne!(frame, user_frame(&signer, "identity", payload));
    }

    fn pubkey_bytes(seed: &[u8; 32]) -> Vec<u8> {
        ed25519::PrivateKey::decode(seed.as_slice())
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec()
    }

    #[test]
    fn unknown_key_subcommand_is_rejected_at_parse() {
        // clap now owns the rejection an unknown `user key <x>` used to hit in
        // the hand dispatch: an unexpected token under `key` fails to parse.
        use clap::Parser as _;
        assert!(TestUserCli::try_parse_from(["user", "key", "bogus"]).is_err());
        assert!(TestUserCli::try_parse_from(["user", "key"]).is_err());
    }

    /// Run the signer over `lines` and hand back its stdout lines.
    fn sign_frames(key: &std::path::Path, lines: &[&str]) -> Vec<String> {
        let mut stdin = stdin_of(lines);
        let mut out = Vec::new();
        user_sign_frame(
            FrameArgs {
                key: key.to_path_buf(),
            },
            &mut stdin,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn sign_frame_round_trips_through_decode_frame() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &[7u8; 32]);

        let payload: &[u8] = b"\x00raw chunk bytes";
        let frames = sign_frames(
            &key_path,
            &[
                TEST_PASSWORD,
                &format!("files 42 {}", hex_bytes(payload)),
                &format!("new-product 43 {} {}", hex_bytes(payload), "ab".repeat(32)),
            ],
        );
        assert_eq!(frames.len(), 2);

        let (origin, msg) =
            node::decode_frame(&config::unhex(&frames[0]).unwrap()).expect("frame verifies");
        let signer = ed25519::PrivateKey::decode([7u8; 32].as_slice()).unwrap();
        assert_eq!(
            origin,
            sdk::Origin::External(signer.public_key().as_ref().to_vec())
        );
        assert_eq!(msg.target, "files");
        assert_eq!(msg.payload, payload);
        let (blob_origin, blob_msg, required_blob) =
            node::decode_frame_with_blob(&config::unhex(&frames[1]).unwrap()).unwrap();
        assert_eq!(blob_origin, origin);
        assert_eq!(blob_msg.target, "new-product");
        assert_eq!(blob_msg.payload, payload);
        assert_eq!(required_blob, Some([0xab; 32]));
    }

    /// THE session property: one password line, one key open, N frames — each
    /// answering its own request line, in order, and each verifying against
    /// the same user key. This is what lets the desktop app hold one signer
    /// for the session instead of paying argon2id per write.
    #[test]
    fn one_unlock_signs_every_request_line_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &[7u8; 32]);
        let signer = ed25519::PrivateKey::decode([7u8; 32].as_slice()).unwrap();

        let requests: Vec<String> = (0..8)
            .map(|i| format!("chat {i} {}", hex_bytes(format!("op {i}").as_bytes())))
            .collect();
        let mut lines = vec![TEST_PASSWORD.to_string()];
        lines.extend(requests);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();

        let frames = sign_frames(&key_path, &lines);
        assert_eq!(frames.len(), 8);
        for (i, frame_hex) in frames.iter().enumerate() {
            let (origin, msg) =
                node::decode_frame(&config::unhex(frame_hex).unwrap()).expect("frame verifies");
            assert_eq!(
                origin,
                sdk::Origin::External(signer.public_key().as_ref().to_vec())
            );
            assert_eq!(msg.target, "chat");
            assert_eq!(msg.payload, format!("op {i}").into_bytes());
        }
    }

    #[test]
    fn a_malformed_request_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &[7u8; 32]);

        for bad in [
            "chat 1",
            "chat notanumber 00",
            "chat 1 nothex",
            "chat 1 00 x",
        ] {
            let mut stdin = stdin_of(&[TEST_PASSWORD, bad]);
            assert!(
                user_sign_frame(
                    FrameArgs {
                        key: key_path.clone()
                    },
                    &mut stdin,
                    &mut Vec::new(),
                )
                .is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn sign_caller_returns_the_pop_a_publisher_would_accept() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &[9u8; 32]);
        let publisher = [0xabu8; 32];
        let head_path = dir.path().join("head.json");
        let body_path = dir.path().join("body.bin");
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 7,
            name: gateway::RouteName::named("api"),
            revision: 3,
            method: gateway::RouteMethod::Post,
            path_and_query: "/item?x=1".into(),
            headers: vec![gateway::ProxyHeader {
                name: "content-type".into(),
                value: "application/octet-stream".into(),
            }],
            upgrade: false,
            user_pop: None,
        };
        std::fs::write(
            &head_path,
            gateway::encode_proxy_request_head(&head).unwrap(),
        )
        .unwrap();
        std::fs::write(&body_path, [0, 1, 255]).unwrap();
        let out = user_sign_caller(
            CallerArgs {
                key: key_path,
                publisher_node: hex_bytes(&publisher),
                head: head_path,
                body: Some(body_path),
            },
            &mut stdin_of(&[TEST_PASSWORD]),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let signer = ed25519::PrivateKey::decode([9u8; 32].as_slice()).unwrap();
        assert_eq!(parsed["key"], hex_bytes(signer.public_key().as_ref()));
        let ts: u64 = parsed["ts"].as_str().unwrap().parse().unwrap();
        let sig = config::unhex(parsed["sig"].as_str().unwrap()).unwrap();
        let verifies = |head: &gateway::ProxyRequestHead, body: &[u8]| {
            identity::KeyScheme::Ed25519.verify(
                signer.public_key().as_ref(),
                gateway::GATEWAY_CALLER_NS,
                &gateway::caller_pop_preimage(&publisher, head, &gateway::body_digest(body), ts),
                &sig,
            )
        };
        assert!(verifies(&head, &[0, 1, 255]));
        assert!(
            !verifies(&head, &[0, 2, 255]),
            "same-length body substitution must fail"
        );
        let mut changed = head.clone();
        changed.revision += 1;
        assert!(!verifies(&changed, &[0, 1, 255]));
        changed = head.clone();
        changed.headers[0].value = "text/plain".into();
        assert!(!verifies(&changed, &[0, 1, 255]));
        changed = head.clone();
        changed.path_and_query = "/item?x=2".into();
        assert!(!verifies(&changed, &[0, 1, 255]));
        changed = head.clone();
        changed.upgrade = true;
        assert!(!verifies(&changed, &[0, 1, 255]));
    }

    #[test]
    fn sign_admin_returns_owner_pop_the_verifier_would_accept() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        write_encrypted(&key_path, &[7u8; 32]);

        let node_key = [0xabu8; 32];
        let mut stdin = stdin_of(&[TEST_PASSWORD]);
        let out = user_sign_admin(
            AdminArgs {
                key: key_path,
                method: "POST".to_string(),
                path: "/v1/admin/shutdown".to_string(),
                node_key: hex_bytes(&node_key),
            },
            &mut stdin,
        )
        .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("one json line");
        let signer = ed25519::PrivateKey::decode([7u8; 32].as_slice()).unwrap();
        // the reported key is the user's own account key.
        assert_eq!(parsed["key"], hex_bytes(signer.public_key().as_ref()));
        // the signature is exactly what the node's verifier reconstructs over
        // the SAME (method, path, node_key, ts) — proving the verb signed the
        // right bytes with the right key (one source: noded::admin::sign_admin).
        let ts: u64 = parsed["ts"].as_str().unwrap().parse().unwrap();
        let expect = noded::admin::sign_admin(&signer, "POST", "/v1/admin/shutdown", &node_key, ts);
        assert_eq!(parsed["sig"], hex_bytes(expect.as_ref()));
        // and it is node-bound: the same tuple against a different node differs.
        let other =
            noded::admin::sign_admin(&signer, "POST", "/v1/admin/shutdown", &[0xcd; 32], ts);
        assert_ne!(parsed["sig"], hex_bytes(other.as_ref()));
    }

    #[test]
    fn sign_frame_reads_the_password_first_for_encrypted_keys() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("user.key");
        let line = userkey::seal_user_key(&[9u8; 32], "hunter2duck").unwrap();
        userkey::write_user_key_new(&key_path, &line).unwrap();

        let request = format!("files 0 {}", hex_bytes(b"{}"));
        let frames = sign_frames(&key_path, &["hunter2duck", &request]);
        assert!(node::decode_frame(&config::unhex(&frames[0]).unwrap()).is_ok());

        // wrong password: refused before any request line is read.
        let mut stdin = stdin_of(&["wrong password", &request]);
        assert!(user_sign_frame(FrameArgs { key: key_path }, &mut stdin, &mut Vec::new()).is_err());
    }

    /// The tty wrapper's non-tty path must be a transparent pass-through: no
    /// prompt, no termios, and it forwards the inner read's value AND its EOF
    /// error verbatim — the piped-stdin contract the sign/key verbs rely on.
    /// (The tty path toggles the real terminal and is untestable headless.)
    #[test]
    fn with_prompt_non_tty_is_a_bare_read() {
        let mut piped = stdin_of(&["hunter2duck"]);
        let got = with_prompt("password", false, || {
            read_stdin_line(&mut piped, "password")
        })
        .unwrap();
        assert_eq!(got, "hunter2duck");

        // a secret field name doesn't change the non-tty behavior either.
        assert!(field_is_secret("password") && field_is_secret("mnemonic"));
        assert!(!field_is_secret("payload-hex"));

        // EOF still errors, exactly as a bare read_stdin_line would.
        let mut empty = empty_stdin();
        assert!(
            with_prompt("password", false, || read_stdin_line(
                &mut empty, "password"
            ))
            .is_err()
        );
    }
}
