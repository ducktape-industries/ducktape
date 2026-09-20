//! `ducktape account` — the ACCOUNT this user key belongs to: a number-keyed
//! principal (`identity` module) that a set of keys of mixed schemes associate
//! under. No node is ever bound to an account; attribution comes from a
//! user-signed origin, so every write here is ONE frame the active wallet key
//! signs, POSTed to `/v1/submit/frame` ([`crate::node_http::submit_frame`]).
//!
//! - `create --name <s>` founds an account for the active key.
//! - `show [--number N | --pubkey <hex>]` reads one (default: this key's).
//! - `key list|approve|add|join|remove` — admitting a second device is a
//!   two-sided ceremony: the NEW device prints its key (`approve`), an EXISTING
//!   member consents to it (`add`, which prints the `AddKey` ticket signed at
//!   the new key's current generation), and the new device submits that ticket
//!   under ITS OWN signature (`join`) — the frame origin is the key being
//!   admitted, so no possession proof rides the payload.
//! - `set-name`, `set-profile` — display text on the origin's account.
//! - `set-handle` — the account's `.duck` handle (a gateway op, same origin).
//! - `key add --passkey|--eth`, `create --eth`, `login` — the browser
//!   ceremonies ([`authpage`]): a passkey or an Ethereum wallet becomes a
//!   member key by signing its own `AddKey` frame AS ORIGIN (registration is
//!   two touches: the page yields the key, then the key proves possession);
//!   `login` is the reverse — a passkey's assertion over THIS key's `AddKey`
//!   preimage is the consent that admits this device, and it takes two touches
//!   because the consent names the account (touch 1 asks the passkey which).
//!
//! A minted consent is account-bound and short-lived ([`CONSENT_TTL`]): there
//! is no revoke verb, so the expiry is how a mis-issued ticket dies.
//!
//! Program output stays `println!` (a CLI's stdout is not logging); the
//! password crosses on stdin, never a flag.

use std::io::BufRead;
use std::path::PathBuf;

use authpage::{Outcome, Request};
use commonware_cryptography::{Signer as _, ed25519};
use identity::{AccountView, IdentityMsg, IdentityQuery, IdentityReply, KeyScheme};

use crate::cli_args::NodeAddr;
use crate::config::{self, hex_bytes};
use crate::cred_cli::VerbCtx;
use crate::node_http;
use crate::userkey_cli::{frame_seq, user_frame};

type AccountResult = Result<(), Box<dyn std::error::Error>>;

/// `ducktape account <verb>`. `--node`/`-n` are the shared [`NodeAddr`] group,
/// `global` so they attach in any position.
#[derive(Debug, clap::Args)]
pub(crate) struct AccountArgs {
    #[command(subcommand)]
    cmd: AccountCmd,
    #[command(flatten)]
    addr: NodeAddr,
    /// path to the user key file (defaults to the keystore's active wallet)
    #[arg(long, value_name = "PATH", global = true)]
    key: Option<PathBuf>,
    /// the relying-party page the passkey/wallet ceremonies open
    #[arg(long, value_name = "URL", default_value = authpage::AUTH_PAGE, global = true)]
    auth_page: String,
    /// do the ceremony on a phone: print the page URL and a QR to scan
    /// instead of opening a browser (a headless box); the result comes back
    /// through the auth host's relay
    #[arg(long, global = true)]
    no_browser: bool,
}

/// how a verb reaches the browser: which page, and whether to open it.
struct AuthCtx {
    page: String,
    browser: bool,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum AccountCmd {
    /// found an account for this user key (a user-signed `create`)
    Create {
        /// the account's display name (not unique — the number is the id)
        #[arg(long, value_name = "NAME")]
        name: String,
        /// found it for an Ethereum wallet instead (two wallet touches; no
        /// user key or password involved)
        #[arg(long)]
        eth: bool,
    },
    /// admit THIS device into an account by a passkey's consent (a browser
    /// touch); the account is the one the passkey was registered for
    Login {
        /// a human label for this device's key (e.g. "laptop")
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
    },
    /// print one account: this key's by default, else by number, member key,
    /// or display name
    Show {
        /// the account number
        #[arg(long, value_name = "N", conflicts_with_all = ["pubkey", "name"])]
        number: Option<u64>,
        /// a member key (hex) of the account to show
        #[arg(long, value_name = "HEX", conflicts_with = "name")]
        pubkey: Option<String>,
        /// a display name to look up — DISPLAY ONLY: a name is not unique and
        /// is freely rewritable, so this refuses when more than one account
        /// currently carries it rather than guessing.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// the keys associated with this account
    #[command(subcommand)]
    Key(KeyCmd),
    /// rename this key's account
    SetName {
        #[arg(long, value_name = "NAME")]
        name: String,
    },
    /// claim this key's account `.duck` handle: `<handle>.duck`, and the
    /// `<owner>` of a `duck://<network>/forge/<owner>/<repo>` address
    SetHandle {
        #[arg(long, value_name = "HANDLE")]
        handle: String,
    },
    /// set this key's account avatar reference and/or bio
    SetProfile {
        /// a duckfs path (`/shared/attachments/avatars/<sha16>.<ext>`)
        #[arg(long, value_name = "PATH")]
        avatar: Option<String>,
        /// a short status line
        #[arg(long, value_name = "TEXT")]
        bio: Option<String>,
    },
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum KeyCmd {
    /// list this key's account keys
    List,
    /// print THIS device's public key, for an existing member to `key add`
    Approve,
    /// (an existing member) admit a key: `--pubkey` prints the ticket for
    /// `key join`; `--passkey`/`--eth` run the browser ceremony and submit
    Add {
        /// the key (hex) being admitted
        #[arg(long, value_name = "HEX", required_unless_present_any = ["passkey", "eth", "ssh"])]
        pubkey: Option<String>,
        /// the key's scheme (with --pubkey)
        #[arg(long, value_enum, default_value_t = SchemeArg::Ed25519)]
        scheme: SchemeArg,
        /// register a NEW passkey in the browser and admit it
        #[arg(long, conflicts_with_all = ["pubkey", "eth", "ssh"])]
        passkey: bool,
        /// link an Ethereum wallet from the browser and admit it
        #[arg(long, conflicts_with_all = ["pubkey", "ssh"])]
        eth: bool,
        /// admit an SSH ed25519 key (its `.pub` file; the private key or
        /// ssh-agent signs via `ssh-keygen -Y sign`) — for `git push --signed`
        #[arg(long, value_name = "PATH", conflicts_with = "pubkey")]
        ssh: Option<PathBuf>,
        /// a human label for the key (e.g. "phone")
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
    },
    /// (the new device) submit an add-key ticket, signed by ITS key
    Join {
        /// the ticket `key add` printed
        #[arg(long, value_name = "JSON")]
        ticket: String,
    },
    /// drop a key from this account (never the last one)
    Remove {
        /// the key (hex) to drop
        #[arg(long, value_name = "HEX")]
        pubkey: String,
    },
}

/// `--scheme` as clap sees it; the wire type is [`KeyScheme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SchemeArg {
    Ed25519,
    Secp256k1,
    Secp256r1,
}

impl From<SchemeArg> for KeyScheme {
    fn from(arg: SchemeArg) -> Self {
        match arg {
            SchemeArg::Ed25519 => KeyScheme::Ed25519,
            SchemeArg::Secp256k1 => KeyScheme::Secp256k1,
            SchemeArg::Secp256r1 => KeyScheme::Secp256r1,
        }
    }
}

/// the scheme's wire token, for prose output.
fn scheme_token(scheme: KeyScheme) -> &'static str {
    match scheme {
        KeyScheme::Ed25519 => "ed25519",
        KeyScheme::Secp256k1 => "secp256k1",
        KeyScheme::Secp256r1 => "secp256r1",
    }
}

/// where `key add` gets the key it admits — the one discriminant the verb
/// branches on.
enum NewKey {
    /// pasted: the ticket path (`key join` on the other device submits)
    Hex { pubkey: String, scheme: KeyScheme },
    /// a passkey registered in the browser, submitted by its own assertion
    Passkey,
    /// a wallet linked in the browser, submitted by its own signature
    Wallet,
    /// an SSH ed25519 key (`.pub` path), submitted by its own `ssh-keygen`
    /// signature
    Ssh(PathBuf),
}

/// the four flags, resolved once (clap already refused a mix).
fn new_key(
    pubkey: Option<String>,
    scheme: SchemeArg,
    passkey: bool,
    eth: bool,
    ssh: Option<PathBuf>,
) -> NewKey {
    match (pubkey, passkey, eth, ssh) {
        (_, _, _, Some(path)) => NewKey::Ssh(path),
        (Some(pubkey), _, _, None) => NewKey::Hex {
            pubkey,
            scheme: scheme.into(),
        },
        (None, true, _, None) => NewKey::Passkey,
        (None, false, _, None) => NewKey::Wallet,
    }
}

/// Dispatch one `account` verb. ONE visible dispatch, nothing in the arms but
/// delegation; the password is read off stdin by the verbs that sign.
pub(crate) fn run(args: AccountArgs) -> AccountResult {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let AccountArgs {
        cmd,
        addr,
        key,
        auth_page,
        no_browser,
    } = args;
    let ctx = VerbCtx { addr, key };
    let auth = AuthCtx {
        page: auth_page,
        browser: !no_browser,
    };
    match cmd {
        AccountCmd::Create { name, eth: false } => cmd_create(&ctx, name, &mut stdin),
        AccountCmd::Create { name, eth: true } => cmd_create_eth(&ctx, &auth, name),
        AccountCmd::Login { label } => cmd_login(&ctx, &auth, label, &mut stdin),
        AccountCmd::Show {
            number,
            pubkey,
            name,
        } => cmd_show(&ctx, number, pubkey, name),
        AccountCmd::Key(KeyCmd::List) => cmd_key_list(&ctx),
        AccountCmd::Key(KeyCmd::Approve) => cmd_key_approve(&ctx),
        AccountCmd::Key(KeyCmd::Add {
            pubkey,
            scheme,
            passkey,
            eth,
            ssh,
            label,
        }) => cmd_key_add(
            &ctx,
            &auth,
            new_key(pubkey, scheme, passkey, eth, ssh),
            label,
            &mut stdin,
        ),
        AccountCmd::Key(KeyCmd::Join { ticket }) => cmd_key_join(&ctx, ticket, &mut stdin),
        AccountCmd::Key(KeyCmd::Remove { pubkey }) => cmd_key_remove(&ctx, pubkey, &mut stdin),
        AccountCmd::SetName { name } => cmd_set_name(&ctx, name, &mut stdin),
        AccountCmd::SetHandle { handle } => cmd_set_handle(&ctx, handle, &mut stdin),
        AccountCmd::SetProfile { avatar, bio } => cmd_set_profile(&ctx, avatar, bio, &mut stdin),
    }
}

// ============================================================================
// verbs
// ============================================================================

fn cmd_create(ctx: &VerbCtx, name: String, stdin: &mut impl BufRead) -> AccountResult {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let height = submit_identity(&base, &user, &create_msg(name))?;
    let account = account_of_key(&base, user.public_key().as_ref())?.ok_or_else(|| {
        format!("committed at height {height}, but the key resolves to no account")
    })?;
    println!("account {} created at height {height}", account.number);
    print_account(&account);
    Ok(())
}

fn cmd_show(
    ctx: &VerbCtx,
    number: Option<u64>,
    pubkey: Option<String>,
    name: Option<String>,
) -> AccountResult {
    let base = ctx.http_base()?;
    let query = match (number, pubkey, name) {
        (Some(number), _, _) => IdentityQuery::Get { number },
        (None, Some(hex), _) => IdentityQuery::OfKey {
            key: config::unhex(&hex).map_err(|e| format!("--pubkey hex: {e}"))?,
        },
        // display only — `resolve_account` refuses an ambiguous name instead
        // of guessing; never route this into an authority decision.
        (None, None, Some(name)) => IdentityQuery::Get {
            number: resolve_account(&base, &name)?,
        },
        (None, None, None) => IdentityQuery::OfKey {
            key: active_pubkey(ctx)?,
        },
    };
    let account = account_reply(query_identity(&base, &query)?)?
        .ok_or("no such account — found one with `ducktape account create --name <you>`")?;
    print_account(&account);
    Ok(())
}

fn cmd_key_list(ctx: &VerbCtx) -> AccountResult {
    let base = ctx.http_base()?;
    let account = own_account(&base, &active_pubkey(ctx)?)?;
    print_keys(&account);
    Ok(())
}

/// `key approve` — no node, no password: the encrypted key file carries its
/// pubkey in the clear, and that is all an existing member needs.
fn cmd_key_approve(ctx: &VerbCtx) -> AccountResult {
    let pubkey = hex_bytes(&active_pubkey(ctx)?);
    println!("ed25519 {pubkey}");
    eprintln!(
        "an existing member admits it with:\n    ducktape account key add --pubkey {pubkey} --scheme ed25519"
    );
    Ok(())
}

fn cmd_key_add(
    ctx: &VerbCtx,
    auth: &AuthCtx,
    new_key: NewKey,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    match new_key {
        NewKey::Hex { pubkey, scheme } => cmd_key_add_hex(ctx, pubkey, scheme, label, stdin),
        NewKey::Passkey => cmd_key_add_passkey(ctx, auth, label, stdin),
        NewKey::Wallet => cmd_key_add_wallet(ctx, auth, label, stdin),
        NewKey::Ssh(path) => cmd_key_add_ssh(ctx, &path, label, stdin),
    }
}

/// `key add --ssh <id_ed25519.pub>`: this device consents, and the SSH key
/// signs its own `AddKey` frame AS ITS ORIGIN through `ssh-keygen -Y sign -n
/// ducktape` (the private key file beside the `.pub`, or ssh-agent) — the
/// OpenSSH envelope the `Ed25519` scheme accepts. Once a member, the key's
/// `git push --signed` speaks for this account.
fn cmd_key_add_ssh(
    ctx: &VerbCtx,
    pub_path: &std::path::Path,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let line = std::fs::read_to_string(pub_path)
        .map_err(|e| format!("reading {}: {e}", pub_path.display()))?;
    let pubkey = keyscheme::sshsig::authorized_key(&line)?;
    let base = ctx.http_base()?;
    let chain_id = ctx.workspace()?.service.chain_id;
    let user = ctx.signer(stdin)?;
    own_account(&base, user.public_key().as_ref())?;
    let msg = consented_add_key(&base, &user, &chain_id, KeyScheme::Ed25519, &pubkey, label)?;
    let preimage = node::frame_preimage(
        KeyScheme::Ed25519,
        &pubkey,
        frame_seq(),
        &identity_msg(&msg),
    );
    let armored = ssh_keygen_sign(
        pub_path,
        &keyscheme::sshsig::ssh_message(node::FRAME_NS, &preimage),
    )?;
    let height = node_http::submit_frame(&base, &ssh_frame(preimage, &armored)?)?;
    println!("ssh key {} added at height {height}", hex_bytes(&pubkey));
    print_keys(&own_account(&base, user.public_key().as_ref())?);
    eprintln!(
        "signed pushes now speak for this account:\n    git config gpg.format ssh\n    \
         git config user.signingkey {}\n    git config push.gpgSign true",
        pub_path.display()
    );
    Ok(())
}

/// `ssh-keygen -Y sign -n ducktape -f <pub>` over `message` on stdin; the
/// armored signature it prints. `-f` takes the `.pub` when the private key
/// sits beside it or in ssh-agent — ssh-keygen resolves that itself.
fn ssh_keygen_sign(pub_path: &std::path::Path, message: &[u8]) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-n", keyscheme::sshsig::DUCKTAPE_SSH_NS, "-f"])
        .arg(pub_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("running ssh-keygen: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(message)
        .map_err(|e| format!("feeding ssh-keygen: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for ssh-keygen: {e}"))?;
    if !out.status.success() {
        return Err(format!("ssh-keygen -Y sign failed ({})", out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| "ssh-keygen printed non-utf-8".to_string())
}

/// the SSH key's frame: preimage ‖ the dearmored SSHSIG — what
/// `/v1/submit/frame` verifies under `Ed25519`'s OpenSSH envelope.
fn ssh_frame(mut preimage: Vec<u8>, armored: &str) -> Result<Vec<u8>, String> {
    preimage.extend_from_slice(&keyscheme::sshsig::dearmor(armored)?);
    Ok(preimage)
}

fn cmd_key_add_hex(
    ctx: &VerbCtx,
    pubkey: String,
    scheme: KeyScheme,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let new_key = config::unhex(&pubkey).map_err(|e| format!("--pubkey hex: {e}"))?;
    let wellformed = scheme.pubkey_wellformed(&new_key);
    if !wellformed {
        return Err(format!("--pubkey is not a well-formed {} key", scheme_token(scheme)).into());
    }
    let base = ctx.http_base()?;
    let chain_id = ctx.workspace()?.service.chain_id;
    let user = ctx.signer(stdin)?;
    let msg = consented_add_key(&base, &user, &chain_id, scheme, &new_key, label)?;
    let ticket = String::from_utf8(identity::encode_msg(&msg)).expect("json is utf-8");
    println!("{ticket}");
    eprintln!("on the new device, run:\n    ducktape account key join --ticket '{ticket}'");
    Ok(())
}

/// `key add --passkey`: ceremony 1 registers the passkey (the page returns its
/// key), this device consents, ceremony 2 has the passkey sign the `AddKey`
/// frame AS ITS ORIGIN — the possession proof a create attestation lacks.
fn cmd_key_add_passkey(
    ctx: &VerbCtx,
    auth: &AuthCtx,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let base = ctx.http_base()?;
    let chain_id = ctx.workspace()?.service.chain_id;
    let user = ctx.signer(stdin)?;
    let account = own_account(&base, user.public_key().as_ref())?;
    let registered = ceremony(
        auth,
        &Request::Create {
            chain_id: chain_id.clone(),
            challenge: authpage::create_challenge(),
            user: account.number,
            name: account.name.clone(),
        },
    )?;
    let Outcome::Create { public_key, .. } = registered else {
        return Err("expected a passkey registration (op=create)".into());
    };
    let msg = consented_add_key(
        &base,
        &user,
        &chain_id,
        KeyScheme::Secp256r1,
        &public_key,
        label,
    )?;
    let (request, preimage) =
        authpage::passkey_frame_request(&public_key, frame_seq(), &identity_msg(&msg));
    let signed = ceremony(auth, &request)?;
    let height = node_http::submit_frame(&base, &authpage::passkey_frame(preimage, &signed)?)?;
    println!(
        "passkey {} added at height {height}",
        hex_bytes(&public_key)
    );
    print_keys(&own_account(&base, user.public_key().as_ref())?);
    Ok(())
}

/// `key add --eth`: touch 1 reveals the wallet's key, this device consents,
/// touch 2 has the wallet sign the `AddKey` frame AS ITS ORIGIN.
fn cmd_key_add_wallet(
    ctx: &VerbCtx,
    auth: &AuthCtx,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let base = ctx.http_base()?;
    let chain_id = ctx.workspace()?.service.chain_id;
    let user = ctx.signer(stdin)?;
    own_account(&base, user.public_key().as_ref())?;
    let pubkey = reveal_wallet(auth)?;
    let msg = consented_add_key(
        &base,
        &user,
        &chain_id,
        KeyScheme::Secp256k1,
        &pubkey,
        label,
    )?;
    let height = submit_as_wallet(&base, auth, &pubkey, &msg)?;
    println!("wallet {} added at height {height}", hex_bytes(&pubkey));
    print_keys(&own_account(&base, user.public_key().as_ref())?);
    Ok(())
}

/// `create --eth`: the wallet founds the account itself — no user key, no
/// password; touch 1 reveals its key, touch 2 signs the `Create` frame.
fn cmd_create_eth(ctx: &VerbCtx, auth: &AuthCtx, name: String) -> AccountResult {
    let base = ctx.http_base()?;
    let pubkey = reveal_wallet(auth)?;
    let msg = IdentityMsg::Create {
        name,
        scheme: KeyScheme::Secp256k1,
    };
    let height = submit_as_wallet(&base, auth, &pubkey, &msg)?;
    let account = account_of_key(&base, &pubkey)?.ok_or_else(|| {
        format!("committed at height {height}, but the wallet resolves to no account")
    })?;
    println!("account {} created at height {height}", account.number);
    print_account(&account);
    Ok(())
}

/// `login`: this device asks a passkey to admit it. TWO touches, because a
/// consent names the account it admits into and only the passkey knows which
/// that is: touch 1 asks (`userHandle`), touch 2 is the assertion over THIS
/// key's `AddKey` preimage for that account — the consent. The frame is signed
/// by this device, the key being admitted, as `key join`.
fn cmd_login(
    ctx: &VerbCtx,
    auth: &AuthCtx,
    label: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let base = ctx.http_base()?;
    let chain_id = ctx.workspace()?.service.chain_id;
    let user = ctx.signer(stdin)?;
    let device_key = user.public_key().as_ref().to_vec();
    let generation = gen_reply(query_identity(
        &base,
        &IdentityQuery::KeyGen {
            key: device_key.clone(),
        },
    )?)?;
    let number =
        authpage::assertion_account(&chain_id, &ceremony(auth, &authpage::account_request())?)?;
    let account = account_reply(query_identity(&base, &IdentityQuery::Get { number })?)?
        .ok_or_else(|| format!("the passkey names account {number}, unknown to this node"))?;
    let expires_at = consent_expiry(&base)?;
    eprintln!("account {number} — confirm once more to admit this device");
    let consent = ceremony(
        auth,
        &authpage::login_request(&chain_id, &device_key, generation, number, expires_at),
    )?;
    let (_, proof) = authpage::login_consent(&chain_id, &consent)?;
    let msg = identity::decode_msg(&authpage::login_add_key(
        &chain_id,
        &device_key,
        generation,
        &sdk::wire::encode(&account),
        label,
        proof,
        expires_at,
    )?)?;
    let height = submit_identity(&base, &user, &msg)?;
    println!("joined account {number} at height {height}");
    print_keys(&own_account(&base, &device_key)?);
    Ok(())
}

// ============================================================================
// browser ceremonies
// ============================================================================

/// How long a ceremony on a phone may take before the CLI gives up.
const PHONE_CEREMONY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// one ceremony round trip. With a browser: bind the loopback callback, open
/// the page, block until it POSTs its result. Without one (`--no-browser`):
/// the ceremony runs on a PHONE — print the page URL and a QR of it, with
/// the auth host's relay as the callback, and poll the relay. A phone cannot
/// reach a loopback listener, so a printed loopback URL would never answer.
fn ceremony(auth: &AuthCtx, request: &Request) -> Result<Outcome, Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            if !auth.browser {
                return phone_ceremony(auth, request).await;
            }
            let listener = authpage::Listener::bind().await?;
            let url = authpage::request_url(&auth.page, request, &listener.callback_url());
            let opened = authpage::open_browser(&url);
            if !opened {
                eprintln!("no browser opener on this machine — open this yourself:\n    {url}");
            }
            eprintln!("waiting for the browser…");
            Ok(listener.wait().await?)
        })
}

async fn phone_ceremony(
    auth: &AuthCtx,
    request: &Request,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let relay = authpage::Relay::at(&auth.page);
    let url = authpage::request_url(&auth.page, request, &relay.callback_url());
    eprintln!(
        "scan this with your phone (or open the URL there):\n\n{}\n    {url}\n",
        authpage::terminal_qr(&url)?
    );
    let outcome = relay
        .wait_reporting(PHONE_CEREMONY_TIMEOUT, |left| {
            eprint!(
                "\rwaiting for the phone… {} left ",
                authpage::countdown(left)
            );
        })
        .await;
    eprintln!();
    Ok(outcome?)
}

/// touch 1 of a wallet: it signs a nonce'd reveal message and its key is
/// recovered from the signature (a wallet never shows its public key).
fn reveal_wallet(auth: &AuthCtx) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let reveal = authpage::reveal_message();
    let touch = ceremony(
        auth,
        &Request::Eth {
            message: reveal.clone(),
        },
    )?;
    Ok(authpage::wallet_pubkey(&reveal, &touch)?)
}

/// touch 2 of a wallet: it signs the op frame as its origin; the commit height.
fn submit_as_wallet(
    base: &str,
    auth: &AuthCtx,
    pubkey: &[u8],
    msg: &IdentityMsg,
) -> Result<u64, Box<dyn std::error::Error>> {
    let (request, preimage) =
        authpage::wallet_frame_request(pubkey, frame_seq(), &identity_msg(msg));
    let touch = ceremony(auth, &request)?;
    node_http::submit_frame(base, &authpage::wallet_frame(preimage, &touch)?)
}

/// the `AddKey` this device consents to for `new_key`, at the key's CURRENT
/// generation (single-use: the module advances the generation on admission).
fn consented_add_key(
    base: &str,
    user: &ed25519::PrivateKey,
    chain_id: &str,
    scheme: KeyScheme,
    new_key: &[u8],
    label: Option<String>,
) -> Result<IdentityMsg, Box<dyn std::error::Error>> {
    let generation = gen_reply(query_identity(
        base,
        &IdentityQuery::KeyGen {
            key: new_key.to_vec(),
        },
    )?)?;
    let account = own_account(base, user.public_key().as_ref())?.number;
    Ok(add_key_msg(
        user,
        chain_id,
        scheme,
        new_key,
        label,
        generation,
        account,
        consent_expiry(base)?,
    )?)
}

/// How long a minted consent stays spendable on the validator/replica lanes,
/// in blocks — `consensus_time` IS the block height there, and a validator
/// network heartbeats about once a second, so this is roughly a day. A device
/// pairing is minutes of work; the day is the slack for a phone in another
/// timezone. There is no revoke op, so this number IS the revocation window:
/// it stays short enough that a mis-issued ticket ages out before anyone
/// remembers it exists.
const CONSENT_TTL_HEIGHT_UNITS: u64 = 86_400;

/// How long a minted consent stays spendable on the sim lane, in
/// milliseconds. [`identity::MAX_CONSENT_TTL`] is the module's hard ceiling
/// on `expires_at - now` in WHATEVER UNIT the network's `ConsensusTimePolicy`
/// uses — on the sim lane's millisecond epoch clock that ceiling is only
/// ~10 minutes, not the ~7 days it is on the validator/replica lanes, so
/// there is no "roughly a day" to mint here: pin to the ceiling itself, the
/// longest pairing window the module will accept.
const CONSENT_TTL_MILLIS_UNITS: u64 = identity::MAX_CONSENT_TTL;

/// the `expires_at` a consent minted right now carries: this node's current
/// `consensus_time` — NOT its height, the two diverge on the sim lane's
/// millisecond epoch clock — plus a TTL scaled to whichever unit
/// `consensus_time` turns out to be.
fn consent_expiry(base: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let (consensus_time, unit) = consensus_clock(base)?;
    Ok(expiry_from_clock(consensus_time, unit))
}

/// the pure derivation `consent_expiry` reduces to once it has the node's
/// clock reading: `consensus_time + a TTL scaled to that clock's unit`. split
/// out so the two [`noded::ConsensusTimeUnit`] shapes are unit-testable
/// without a node to dial.
fn expiry_from_clock(consensus_time: u64, unit: noded::ConsensusTimeUnit) -> u64 {
    let ttl = match unit {
        noded::ConsensusTimeUnit::Height => CONSENT_TTL_HEIGHT_UNITS,
        noded::ConsensusTimeUnit::Millis => CONSENT_TTL_MILLIS_UNITS,
    };
    consensus_time + ttl
}

/// this node's current `consensus_time` and the unit it is expressed in, from
/// `/v1/status`. the identity module compares `expires_at` against exactly
/// this value (`ctx.env().consensus_time`), so a consent must be minted from
/// it — reading `height` instead is only correct under `HeightIsTime`.
fn consensus_clock(
    base: &str,
) -> Result<(u64, noded::ConsensusTimeUnit), Box<dyn std::error::Error>> {
    let status = node_http::get_json(base, "/v1/status").map_err(|failure| failure.to_string())?;
    let consensus_time = status["consensus_time"]
        .as_u64()
        .ok_or_else(|| "node status carries no consensus_time".to_string())?;
    let unit: noded::ConsensusTimeUnit = status
        .get("consensus_time_unit")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("node status consensus_time_unit: {e}"))?
        .unwrap_or_default();
    Ok((consensus_time, unit))
}

fn identity_msg(msg: &IdentityMsg) -> sdk::Msg {
    sdk::Msg {
        target: "identity".into(),
        payload: identity::encode_msg(msg),
    }
}

fn cmd_key_join(ctx: &VerbCtx, ticket: String, stdin: &mut impl BufRead) -> AccountResult {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let height = node_http::submit_frame(&base, &join_frame(&user, &ticket)?)?;
    let account = own_account(&base, user.public_key().as_ref())?;
    println!("joined account {} at height {height}", account.number);
    print_keys(&account);
    Ok(())
}

fn cmd_key_remove(ctx: &VerbCtx, pubkey: String, stdin: &mut impl BufRead) -> AccountResult {
    let key = config::unhex(&pubkey).map_err(|e| format!("--pubkey hex: {e}"))?;
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let height = submit_identity(&base, &user, &IdentityMsg::RemoveKey { key })?;
    println!("removed at height {height}");
    Ok(())
}

fn cmd_set_name(ctx: &VerbCtx, name: String, stdin: &mut impl BufRead) -> AccountResult {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let height = submit_identity(&base, &user, &IdentityMsg::SetName { name })?;
    println!("renamed at height {height}");
    Ok(())
}

/// the handle lives in the gateway module (it is a `.duck` name), but it is
/// the account's, claimed by the same user-signed origin as its name.
fn cmd_set_handle(ctx: &VerbCtx, handle: String, stdin: &mut impl BufRead) -> AccountResult {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let message = gateway::GatewayMsg::SetHandle {
        handle: Some(handle.clone()),
    };
    let height = crate::cred_cli::submit_gateway(&base, &user, &message)?;
    println!("handle {handle} set at height {height}");
    Ok(())
}

fn cmd_set_profile(
    ctx: &VerbCtx,
    avatar: Option<String>,
    bio: Option<String>,
    stdin: &mut impl BufRead,
) -> AccountResult {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let height = submit_identity(&base, &user, &IdentityMsg::SetProfile { avatar, bio })?;
    println!("profile set at height {height}");
    Ok(())
}

// ============================================================================
// pure builders (unit-tested)
// ============================================================================

/// the founding op: the CLI's key is always ed25519, and the frame signature
/// is its possession proof.
fn create_msg(name: String) -> IdentityMsg {
    IdentityMsg::Create {
        name,
        scheme: KeyScheme::Ed25519,
    }
}

/// the `AddKey` an existing member consents to for `new_key` (of `scheme`)
/// into `account` at its current `generation` on `chain_id`, spendable until
/// `expires_at`. Encoded, it is the ticket `key add --pubkey` prints — ONE
/// json line, exactly the payload the new device submits.
#[allow(clippy::too_many_arguments)]
fn add_key_msg(
    user: &ed25519::PrivateKey,
    chain_id: &str,
    scheme: KeyScheme,
    new_key: &[u8],
    label: Option<String>,
    generation: u64,
    account: u64,
    expires_at: u64,
) -> Result<IdentityMsg, String> {
    let authorizer = config::ed25519_authorizer(
        user, chain_id, scheme, new_key, generation, account, expires_at,
    );
    Ok(IdentityMsg::AddKey {
        scheme,
        label,
        authorizer: sdk::wire::decode(&authorizer)?,
    })
}

/// the joining device's frame: the ticket bytes VERBATIM (the member's proof
/// is over them), signed by the key being admitted. A ticket that is not an
/// `AddKey` is refused here, before any unlock reaches the node.
fn join_frame(user: &ed25519::PrivateKey, ticket: &str) -> Result<Vec<u8>, String> {
    let is_add_key = matches!(
        identity::decode_msg(ticket.as_bytes())?,
        IdentityMsg::AddKey { .. }
    );
    if !is_add_key {
        return Err("--ticket is not an add-key ticket (from `ducktape account key add`)".into());
    }
    Ok(user_frame(user, "identity", ticket.as_bytes().to_vec()))
}

// ============================================================================
// node round-trips
// ============================================================================

/// sign `msg` with `user` and submit it as one frame; the commit height.
fn submit_identity(
    base: &str,
    user: &ed25519::PrivateKey,
    msg: &IdentityMsg,
) -> Result<u64, Box<dyn std::error::Error>> {
    node_http::submit_frame(
        base,
        &user_frame(user, "identity", identity::encode_msg(msg)),
    )
}

fn query_identity(
    base: &str,
    query: &IdentityQuery,
) -> Result<IdentityReply, Box<dyn std::error::Error>> {
    let value = node_http::query(base, "identity", serde_json::to_value(query)?)?;
    Ok(serde_json::from_value(value)?)
}

fn account_reply(reply: IdentityReply) -> Result<Option<AccountView>, Box<dyn std::error::Error>> {
    match reply {
        IdentityReply::Account(account) => Ok(account),
        IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
            Err(format!("unexpected identity reply: {reply:?}").into())
        }
    }
}

fn accounts_reply(reply: IdentityReply) -> Result<Vec<AccountView>, Box<dyn std::error::Error>> {
    match reply {
        IdentityReply::Accounts(accounts) => Ok(accounts),
        IdentityReply::Account(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
            Err(format!("unexpected identity reply: {reply:?}").into())
        }
    }
}

fn gen_reply(reply: IdentityReply) -> Result<u64, Box<dyn std::error::Error>> {
    match reply {
        IdentityReply::Gen(generation) => Ok(generation),
        IdentityReply::Account(_) | IdentityReply::Accounts(_) | IdentityReply::Resolved(_) => {
            Err(format!("unexpected identity reply: {reply:?}").into())
        }
    }
}

/// the account `key` belongs to, if any — THE resolver every consumer reads.
pub(crate) fn account_of_key(
    base: &str,
    key: &[u8],
) -> Result<Option<AccountView>, Box<dyn std::error::Error>> {
    account_reply(query_identity(
        base,
        &IdentityQuery::OfKey { key: key.to_vec() },
    )?)
}

/// [`account_of_key`] for the local user key, loud when it is on no account.
pub(crate) fn own_account(
    base: &str,
    key: &[u8],
) -> Result<AccountView, Box<dyn std::error::Error>> {
    account_of_key(base, key)?
        .ok_or("this user key belongs to no account — `ducktape account create` first".into())
}

/// every account, paged through `All` by number.
pub(crate) fn all_accounts(base: &str) -> Result<Vec<AccountView>, Box<dyn std::error::Error>> {
    let mut all = Vec::new();
    let mut from = 1;
    loop {
        let page = accounts_reply(query_identity(
            base,
            &IdentityQuery::All {
                from,
                limit: identity::MAX_QUERY_LIMIT,
            },
        )?)?;
        let short_page = (page.len() as u64) < identity::MAX_QUERY_LIMIT;
        let Some(last) = page.last() else {
            return Ok(all);
        };
        from = last.number + 1;
        all.extend(page);
        if short_page {
            return Ok(all);
        }
    }
}

/// Resolve an account named on the command line, for DISPLAY only: a decimal
/// number is used as is (never dialing), anything else is a display name
/// matched over every account — ambiguity and absence are loud errors. A
/// name is `AccountView.name` -- attacker-chosen, non-unique, and rewritable
/// at will by a free `SetName` frame — so this resolver is safe only where
/// the result decides what to PRINT, never what to grant. An authority
/// decision (`cred grant`/`revoke`, `node work admit`) MUST go through
/// [`resolve_account_authority`] instead.
pub(crate) fn resolve_account(base: &str, input: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Ok(number) = input.parse::<u64>() {
        gateway::validate_account_number(number)?;
        return Ok(number);
    }
    let accounts = all_accounts(base)?;
    let matches: Vec<&AccountView> = accounts.iter().filter(|a| a.name == input).collect();
    match matches.as_slice() {
        [only] => Ok(only.number),
        [] => Err(format!("no account named {input:?} (nor an account number)").into()),
        many => {
            let numbers: Vec<String> = many.iter().map(|a| a.number.to_string()).collect();
            Err(format!(
                "account name {input:?} is ambiguous across {} accounts — pass a number: {}",
                many.len(),
                numbers.join(", ")
            )
            .into())
        }
    }
}

/// Resolve an account NUMBER for an AUTHORITY decision — `cred
/// grant`/`revoke` (who may draw on a lent credential) and `node work admit`
/// (whose workload this node runs). A display NAME is refused outright, never
/// matched: `AccountView.name` is `SetName`-rewritable by its own holder at no
/// cost and gated by nothing but membership, so a name-based match lets a
/// renamed squatter's account collect an authority grant meant for someone
/// else (the rename can happen and reverse around the exact moment the
/// operator runs the granting command). The refusal lists the numbers the
/// name currently matches, if any, so the operator can pick the right one
/// without racing a mid-flight rename.
pub(crate) fn resolve_account_authority(
    base: &str,
    input: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    if let Ok(number) = input.parse::<u64>() {
        gateway::validate_account_number(number)?;
        return Ok(number);
    }
    let accounts = all_accounts(base)?;
    Err(authority_name_refusal(input, &accounts).into())
}

/// the refusal an authority resolver gives for a NAME: never a match, just
/// the numbers (if any) currently carrying it, so the operator can pick the
/// right one without racing a mid-flight rename. pure over an already-fetched
/// account list, so it is unit-testable without a node to dial.
fn authority_name_refusal(input: &str, accounts: &[AccountView]) -> String {
    let numbers: Vec<String> = accounts
        .iter()
        .filter(|a| a.name == input)
        .map(|a| a.number.to_string())
        .collect();
    let holders = if numbers.is_empty() {
        "no account currently carries it".to_string()
    } else {
        format!("account(s) {} currently carry it", numbers.join(", "))
    };
    format!(
        "{input:?} is a display name, not an account number — names are freely rewritable \
         and not unique, so an authority grant must name a NUMBER ({holders})"
    )
}

/// the active key's pubkey WITHOUT unlocking it: the encrypted file carries it
/// in the clear, so the read verbs never ask for a password.
fn active_pubkey(ctx: &VerbCtx) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(keystore::userkey::read_user_key_file(&ctx.key_path()?)?.pubkey)
}

fn print_account(account: &AccountView) {
    println!("number={} name={}", account.number, account.name);
    if let Some(avatar) = &account.avatar {
        println!("avatar={avatar}");
    }
    if let Some(bio) = &account.bio {
        println!("bio={bio}");
    }
    print_keys(account);
}

fn print_keys(account: &AccountView) {
    for key in &account.keys {
        let label = key.label.as_deref().unwrap_or("");
        println!(
            "key={} {} {label}",
            scheme_token(key.scheme),
            hex_bytes(&key.pubkey)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_codec::DecodeExt as _;

    /// a Parser wrapper so tests can exercise the derived verb SHAPE the same
    /// way `main.rs`'s integrator will.
    #[derive(clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        #[allow(dead_code)]
        cmd: AccountCmd,
    }

    fn signer(seed: u8) -> ed25519::PrivateKey {
        ed25519::PrivateKey::decode([seed; 32].as_slice()).unwrap()
    }

    /// `--no-browser` means a PHONE does the ceremony: its callback is the
    /// auth host's relay, never a loopback listener a phone cannot reach.
    #[test]
    fn a_phone_ceremony_relays_and_never_binds_a_loopback_listener() {
        let source = include_str!("account_cli.rs");
        let phone = source
            .split("\nasync fn phone_ceremony(")
            .nth(1)
            .expect("the phone ceremony")
            .split("\n}\n")
            .next()
            .unwrap();
        assert!(phone.contains("authpage::Relay::at(&auth.page)"));
        assert!(phone.contains("authpage::terminal_qr(&url)"));
        assert!(!phone.contains("Listener"));
        let browser = source
            .split("\nfn ceremony(")
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        let compact: String = browser.split_whitespace().collect();
        assert!(compact.contains("if!auth.browser{returnphone_ceremony(auth,request).await;"));
    }

    #[test]
    fn the_synchronous_cli_drives_the_async_phone_ceremony() {
        use std::io::{BufRead as _, BufReader, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let page = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&socket);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.starts_with("GET /r/"), "{request}");
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                let headers_complete = line == "\r\n";
                if headers_complete {
                    break;
                }
            }
            let body = r#"{"op":"get","error":"cancelled","message":"phone cancelled"}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let auth = AuthCtx {
            page,
            browser: false,
        };
        let result = ceremony(&auth, &Request::Get { challenge: [7; 32] });
        assert!(result.unwrap_err().to_string().contains("phone cancelled"));
        server.join().unwrap();
    }

    fn add_key_ticket(
        user: &ed25519::PrivateKey,
        chain_id: &str,
        scheme: KeyScheme,
        new_key: &[u8],
        label: Option<String>,
        generation: u64,
    ) -> String {
        let msg = add_key_msg(
            user,
            chain_id,
            scheme,
            new_key,
            label,
            generation,
            TEST_ACCOUNT,
            TEST_EXPIRES,
        )
        .unwrap();
        String::from_utf8(identity::encode_msg(&msg)).unwrap()
    }

    /// what a ticket in these tests is minted for.
    const TEST_ACCOUNT: u64 = 11;
    const TEST_EXPIRES: u64 = 900;

    #[test]
    fn the_verb_tree_is_create_show_key_login_set_name_set_profile() {
        use clap::CommandFactory as _;
        let cmd = TestCli::command();
        for name in [
            "create",
            "show",
            "key",
            "login",
            "set-name",
            "set-handle",
            "set-profile",
        ] {
            assert!(cmd.find_subcommand(name).is_some(), "verb {name} missing");
        }
        let key = cmd.find_subcommand("key").unwrap();
        for name in ["list", "approve", "add", "join", "remove"] {
            assert!(
                key.find_subcommand(name).is_some(),
                "key verb {name} missing"
            );
        }
    }

    /// `set-handle` claims exactly one handle, named by flag; the password
    /// is never an argument.
    #[test]
    fn set_handle_takes_one_handle_by_flag() {
        use clap::Parser as _;
        let parse = |line: &str| TestCli::try_parse_from(line.split(' ')).map(|cli| cli.cmd);
        assert!(matches!(
            parse("t set-handle --handle operator").unwrap(),
            AccountCmd::SetHandle { handle } if handle == "operator"
        ));
        assert!(parse("t set-handle").is_err(), "no handle");
        assert!(
            parse("t set-handle operator").is_err(),
            "a flag, not a positional"
        );
        assert!(
            parse("t set-handle --handle a --password x").is_err(),
            "no password flag"
        );
    }

    /// `key add` names its key ONE way: a pasted hex, a browser passkey, or a
    /// browser wallet — never none, never two.
    #[test]
    fn key_add_takes_exactly_one_key_source() {
        use clap::Parser as _;
        let parse = |line: &str| TestCli::try_parse_from(line.split(' ')).map(|cli| cli.cmd);
        let hex = "0011";
        let AccountCmd::Key(KeyCmd::Add { pubkey, scheme, .. }) =
            parse(&format!("t key add --pubkey {hex} --scheme secp256k1")).unwrap()
        else {
            panic!("an add");
        };
        assert_eq!(pubkey.as_deref(), Some(hex));
        assert_eq!(scheme, SchemeArg::Secp256k1);
        assert!(matches!(
            parse("t key add --passkey --label phone").unwrap(),
            AccountCmd::Key(KeyCmd::Add {
                passkey: true,
                eth: false,
                ..
            })
        ));
        assert!(matches!(
            parse("t key add --eth").unwrap(),
            AccountCmd::Key(KeyCmd::Add {
                passkey: false,
                eth: true,
                ..
            })
        ));
        assert!(matches!(
            parse("t key add --ssh /home/me/.ssh/id_ed25519.pub").unwrap(),
            AccountCmd::Key(KeyCmd::Add { ssh: Some(_), .. })
        ));
        assert!(
            parse("t key add --ssh a.pub --passkey").is_err(),
            "two sources"
        );
        assert!(parse("t key add").is_err(), "no source");
        assert!(parse("t key add --passkey --eth").is_err(), "two sources");
        assert!(parse(&format!("t key add --pubkey {hex} --eth")).is_err());
        assert!(matches!(
            parse("t create --name x --eth").unwrap(),
            AccountCmd::Create { eth: true, .. }
        ));
        assert!(matches!(
            parse("t login --label laptop").unwrap(),
            AccountCmd::Login { label: Some(_) }
        ));
        match new_key(None, SchemeArg::Ed25519, false, true, None) {
            NewKey::Wallet => {}
            NewKey::Hex { .. } | NewKey::Passkey | NewKey::Ssh(_) => panic!("--eth is the wallet"),
        }
    }

    /// an SSH key's admission: the frame the CLI submits carries the key's
    /// own `ssh-keygen -Y sign -n ducktape` (faked by the testkit, armored as
    /// ssh-keygen prints it) and decodes at the node with the SSH key as its
    /// verified origin.
    #[test]
    fn an_ssh_key_signs_its_own_admission() {
        use keyscheme::sshsig::{armor, ssh_message};
        use keyscheme::testkit::{ssh_key, ssh_proof, ssh_pubkey};
        let member = signer(5);
        let sk = ssh_key(3);
        let pubkey = ssh_pubkey(&sk);
        let msg = add_key_msg(
            &member,
            "chain-a",
            KeyScheme::Ed25519,
            &pubkey,
            None,
            0,
            TEST_ACCOUNT,
            TEST_EXPIRES,
        )
        .unwrap();
        let preimage = node::frame_preimage(KeyScheme::Ed25519, &pubkey, 9, &identity_msg(&msg));
        // what ssh-keygen prints for the bytes the CLI pipes into it.
        let armored = armor(&ssh_proof(&sk, node::FRAME_NS, &preimage));
        assert_eq!(
            ssh_message(node::FRAME_NS, &preimage),
            keyscheme::sshsig::ssh_message(node::FRAME_NS, &preimage)
        );
        let frame = ssh_frame(preimage, &armored).unwrap();
        let (origin, submitted) = node::decode_frame(&frame).expect("the node verifies it");
        assert_eq!(origin, sdk::Origin::External(pubkey));
        assert_eq!(identity::decode_msg(&submitted.payload).unwrap(), msg);
        assert!(ssh_frame(Vec::new(), "not armored").is_err());
    }

    /// every key this CLI mints a consent over is 33-byte compressed SEC1 —
    /// the one spelling the chain admits. `--eth` derives it by RECOVERY from
    /// the reveal touch, `--passkey` reads it off the page's registration
    /// result, and `--pubkey <hex>` is gated on the same well-formedness the
    /// decoder applies, so no path can put an uncompressed point on an account.
    #[test]
    fn every_pubkey_the_cli_derives_is_canonical_compressed_sec1() {
        use keyscheme::testkit::{eth_key, eth_sign_message, passkey, passkey_pubkey};

        let wallet = eth_key(5);
        let reveal = authpage::reveal_message();
        let revealed = authpage::wallet_pubkey(
            &reveal,
            &Outcome::Eth {
                address: "0x0".into(),
                signature: eth_sign_message(&wallet, &reveal),
                message: reveal.clone(),
            },
        )
        .expect("the reveal touch answers with the wallet's key");
        assert_eq!(revealed.len(), 33);
        assert!(KeyScheme::Secp256k1.pubkey_wellformed(&revealed));

        let registered = passkey_pubkey(&passkey(4));
        assert_eq!(registered.len(), 33);
        assert!(KeyScheme::Secp256r1.pubkey_wellformed(&registered));

        // and the hand-typed path: the uncompressed spelling of that very
        // wallet key is not something `key add --pubkey` will mint a ticket for.
        let uncompressed = wallet
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        assert_eq!(uncompressed.len(), 65);
        assert!(!KeyScheme::Secp256k1.pubkey_wellformed(&uncompressed));
    }

    /// a login's frame: the page's assertion (faked exactly as an
    /// authenticator signs it) becomes the `AddKey` this device submits AS
    /// ITS OWN ORIGIN — `key join`'s shape with a passkey's consent inside.
    #[test]
    fn login_submits_the_consented_add_key_under_the_device_key() {
        use keyscheme::testkit::{passkey, passkey_assertion_parts, passkey_pubkey};
        let device = signer(6);
        let device_key = device.public_key().as_ref().to_vec();
        let mine = passkey(3);
        let account = AccountView {
            control: identity::Control::Keys,
            number: 11,
            name: "alice".into(),
            keys: vec![identity::KeyView {
                scheme: KeyScheme::Secp256r1,
                pubkey: passkey_pubkey(&mine),
                label: Some("phone".into()),
                added_at: 0,
            }],
            avatar: None,
            bio: None,
            updated_at: 0,
        };
        let preimage = identity::add_key_preimage(
            "chain-a",
            KeyScheme::Ed25519,
            &device_key,
            4,
            TEST_ACCOUNT,
            TEST_EXPIRES,
        );
        let (authenticator_data, client_data_json, signature) = passkey_assertion_parts(
            &mine,
            "auth.ducktape.industries",
            identity::IDENTITY_ADD_KEY_NS,
            &preimage,
        );
        let outcome = Outcome::Get {
            authenticator_data,
            client_data_json,
            signature,
            user_handle: Some(authpage::UserHandle::new("chain-a", 11)),
        };
        let (number, proof) = authpage::login_consent("chain-a", &outcome).unwrap();
        assert_eq!(number, 11);
        let msg = identity::decode_msg(
            &authpage::login_add_key(
                "chain-a",
                &device_key,
                4,
                &sdk::wire::encode(&account),
                None,
                proof,
                TEST_EXPIRES,
            )
            .unwrap(),
        )
        .unwrap();
        let frame = user_frame(&device, "identity", identity::encode_msg(&msg));
        let (origin, submitted) = node::decode_frame(&frame).unwrap();
        assert_eq!(origin, sdk::Origin::External(device_key));
        assert_eq!(identity::decode_msg(&submitted.payload).unwrap(), msg);
    }

    #[test]
    fn create_decodes_to_create_for_an_ed25519_origin() {
        let msg = create_msg("alice".into());
        assert_eq!(
            identity::decode_msg(&identity::encode_msg(&msg)).unwrap(),
            IdentityMsg::Create {
                name: "alice".into(),
                scheme: KeyScheme::Ed25519,
            }
        );
    }

    #[test]
    fn key_add_ticket_decodes_to_add_key_and_its_consent_verifies() {
        let member = signer(5);
        let new_key = signer(6).public_key().as_ref().to_vec();
        let ticket = add_key_ticket(
            &member,
            "chain-a",
            KeyScheme::Ed25519,
            &new_key,
            Some("phone".into()),
            2,
        );
        assert_eq!(ticket.lines().count(), 1, "one json line, pasteable");
        let IdentityMsg::AddKey {
            scheme,
            label,
            authorizer,
        } = identity::decode_msg(ticket.as_bytes()).unwrap()
        else {
            panic!("a ticket is an AddKey");
        };
        assert_eq!(scheme, KeyScheme::Ed25519);
        assert_eq!(label.as_deref(), Some("phone"));
        assert_eq!(authorizer.key, member.public_key().as_ref());
        // the module's own check, verbatim: at the generation the ticket was
        // minted for — and at no other.
        assert_eq!(authorizer.account, TEST_ACCOUNT);
        assert_eq!(authorizer.expires_at, TEST_EXPIRES);
        let preimage = |generation, account, expires_at| {
            identity::add_key_preimage(
                "chain-a",
                KeyScheme::Ed25519,
                &new_key,
                generation,
                account,
                expires_at,
            )
        };
        let verifies = |generation, account, expires_at| {
            KeyScheme::Ed25519.verify(
                &authorizer.key,
                identity::IDENTITY_ADD_KEY_NS,
                &preimage(generation, account, expires_at),
                &authorizer.proof,
            )
        };
        assert!(verifies(2, TEST_ACCOUNT, TEST_EXPIRES));
        assert!(!verifies(3, TEST_ACCOUNT, TEST_EXPIRES), "single-use");
        assert!(
            !verifies(2, TEST_ACCOUNT + 1, TEST_EXPIRES),
            "the ticket admits into ONE account"
        );
        assert!(
            !verifies(2, TEST_ACCOUNT, TEST_EXPIRES + 1),
            "and dies at ONE time"
        );
    }

    #[test]
    fn key_join_wraps_the_ticket_verbatim_under_the_joining_key() {
        let member = signer(5);
        let joiner = signer(6);
        let ticket = add_key_ticket(
            &member,
            "chain-a",
            KeyScheme::Ed25519,
            joiner.public_key().as_ref(),
            None,
            0,
        );
        let frame = join_frame(&joiner, &ticket).unwrap();
        let (origin, msg) = node::decode_frame(&frame).expect("frame verifies");
        assert_eq!(
            origin,
            sdk::Origin::External(joiner.public_key().as_ref().to_vec()),
            "the origin is the key being admitted"
        );
        assert_eq!(msg.target, "identity");
        assert_eq!(
            msg.payload,
            ticket.as_bytes(),
            "the member's proof is over these exact bytes"
        );

        // anything but an AddKey is refused before a node is dialed.
        let not_a_ticket =
            String::from_utf8(identity::encode_msg(&create_msg("x".into()))).unwrap();
        assert!(join_frame(&joiner, &not_a_ticket).is_err());
        assert!(join_frame(&joiner, "not json").is_err());
    }

    /// a decimal never dials: the base here is a dead url on purpose.
    #[test]
    fn resolve_account_takes_a_number_offline_and_refuses_zero() {
        assert_eq!(resolve_account("http://127.0.0.1:9", "12").unwrap(), 12);
        assert!(resolve_account("http://127.0.0.1:9", "0").is_err());
    }

    /// the authority resolver takes the same numbers as the display resolver
    /// — never dials for the number case, so this stays offline exactly like
    /// the display resolver's test above.
    #[test]
    fn resolve_account_authority_takes_a_number_offline_and_refuses_zero() {
        assert_eq!(
            resolve_account_authority("http://127.0.0.1:9", "12").unwrap(),
            12
        );
        assert!(resolve_account_authority("http://127.0.0.1:9", "0").is_err());
    }

    /// issue #1764: an authority decision (`cred grant`, `node work admit`)
    /// must never resolve a NAME to an account — `SetName` is free, unbound
    /// and gated by nothing but membership, so a name is attacker-chosen and
    /// non-unique. the refusal names the numbers (if any) currently holding
    /// it, so the operator can pick the right one instead of the resolver
    /// guessing (and instead of racing a squatter's rename).
    #[test]
    fn authority_name_refusal_lists_current_holders_and_never_picks_one() {
        let account = |number: u64, name: &str| AccountView {
            control: identity::Control::Keys,
            number,
            name: name.into(),
            keys: Vec::new(),
            avatar: None,
            bio: None,
            updated_at: 0,
        };
        let accounts = vec![account(3, "alice"), account(5, "alice"), account(7, "bob")];

        let refusal = authority_name_refusal("alice", &accounts);
        assert!(refusal.contains("NUMBER"), "{refusal}");
        assert!(refusal.contains('3'), "{refusal}");
        assert!(refusal.contains('5'), "{refusal}");
        assert!(!refusal.contains('7'), "{refusal}");

        // a squatter who hasn't taken the name yet still gets refused, never
        // silently treated as "no match, carry on" — the caller's `?` turns
        // this into a hard error regardless of who currently holds it.
        let no_holder = authority_name_refusal("mallory", &accounts);
        assert!(no_holder.contains("no account currently carries it"));
    }

    /// issue #1763: on the validator/replica lanes `consensus_time` IS the
    /// block height, so a consent mints ~a day of blocks past it — the
    /// pre-fix behavior, byte for byte.
    #[test]
    fn expiry_from_clock_under_height_is_time() {
        assert_eq!(
            expiry_from_clock(1_000, noded::ConsensusTimeUnit::Height),
            1_000 + CONSENT_TTL_HEIGHT_UNITS
        );
    }

    /// on the sim lane `consensus_time` is a millisecond epoch clock miles
    /// past any block height (`SIM_EPOCH_MS` alone is ~1.75e12) — mixing the
    /// two units is exactly bug #1763 (every consent minted "already
    /// expired"). the fix mints from the clock reading itself, plus a TTL
    /// pinned to the identity module's own hard ceiling in this unit (see
    /// `CONSENT_TTL_MILLIS_UNITS`'s doc) rather than the height-lane's day.
    #[test]
    fn expiry_from_clock_under_millisecond_epoch() {
        let sim_epoch_ms = 1_750_000_000_000_u64;
        let expiry = expiry_from_clock(sim_epoch_ms, noded::ConsensusTimeUnit::Millis);
        assert_eq!(expiry, sim_epoch_ms + identity::MAX_CONSENT_TTL);
        // never the height-lane TTL: at this scale it would still be almost
        // instantly expired against the module's much smaller ms ceiling.
        assert_ne!(expiry, sim_epoch_ms + CONSENT_TTL_HEIGHT_UNITS);
        // and always within the module's ceiling, or `identity` refuses the
        // consent as outliving `MAX_CONSENT_TTL` even though it isn't stale.
        assert!(expiry - sim_epoch_ms <= identity::MAX_CONSENT_TTL);
    }
}
