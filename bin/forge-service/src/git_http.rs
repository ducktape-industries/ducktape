//! git smart-HTTP: forge as a full push+fetch remote over `/{repo}/…`.

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::{ServiceState, error_response};

// Git wire negotiation and ref policy live in this independently installed
// process. Queries and submissions use the generic module API; pack bytes use
// the generic content-addressed store. Fetch reads only the explicit read-only
// tenant binding, so it advertises only object closures it can actually serve.

/// the capabilities forge's receive-pack advertises. deliberately NO
/// `side-band-64k`, so the client sends the report-status back as plain
/// pkt-lines (not muxed onto a side channel) — the minimal wire this bridge
/// needs to read.
const GIT_RECEIVE_PACK_CAPS: &str =
    "report-status report-status-v2 delete-refs ofs-delta agent=ducktape-forge/0.1";
/// the capabilities forge's upload-pack (fetch/clone) advertises. `side-band-64k`
/// muxes the packfile onto band 1 of the reply — git clients request it by
/// default; `multi_ack_detailed` is the modern negotiation, `thin-pack`/
/// `ofs-delta` are standard pack encodings. no fetch-side extras (shallow /
/// filter): the answer is either the full closure or a have-bounded delta.
const GIT_UPLOAD_PACK_CAPS: &str =
    "multi_ack_detailed side-band-64k thin-pack ofs-delta agent=ducktape-forge/0.1";
/// what a git request that is NOT a push may carry: a fetch's want/have
/// negotiation and a merge request are lists of oids, not content. A push has
/// no limit at all — see the receive-pack route.
pub const GIT_NEGOTIATION_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// how much of a spooled push this bridge reads to find the end of the
/// pkt-line command section. [`MAX_GIT_PKT_LINES`] lines of `<old> <new>
/// <ref>` fit inside it several times over; a command section that somehow
/// does not is read in further doublings rather than refused.
const GIT_COMMAND_HEAD_BYTES: usize = 1024 * 1024;

/// the most a gzip-encoded body may INFLATE to, as a multiple of what arrived.
/// Not a size limit — a bomb guard: gzip's ratio tops out around 1030:1, and
/// this door writes what it inflates to disk before anything has proved
/// itself. A real git request compresses nowhere near this.
const GIT_MAX_INFLATE_RATIO: u64 = 64;
/// max PACK bytes per side-band-64k data pkt-line: prefixed with the 1-byte band
/// id, plus the 4-byte pkt length header, this yields a 65520-byte line — git's
/// `LARGE_PACKET_MAX`, the ceiling a side-band-64k client accepts.
const GIT_SIDE_BAND_CHUNK: usize = 65515;
/// the ref namespace pushes may touch: any branch. a command outside
/// `refs/heads/*` (tags, notes) is refused with a per-ref `ng`.
const GIT_HEADS_PREFIX: &str = "refs/heads/";
/// 40 ascii zeros: git's "null" oid — the old value of a ref being created, and
/// the head advertised for an unborn repo.
const GIT_ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// raw sha1 oid length in bytes. git's wire oids are 40 hex chars == 20 bytes;
/// forge's `Push` op wants exactly these raw bytes (it re-length-checks too).
const GIT_OID_RAW_LEN: usize = 20;
/// the flush-pkt: a zero-length pkt that ends a pkt-line stream or section.
const GIT_FLUSH_PKT: &[u8] = b"0000";
/// max pkt-lines parsed out of one request — the command/want section
/// ([`parse_pkt_lines`]) and the upload-pack negotiation tail
/// ([`parse_upload_pack_request`]'s haves loop) each stop here. a real push
/// updates at most a few thousand refs (a monorepo touching every branch);
/// a real fetch negotiation trades at most a few thousand haves before a
/// client gives up and sends the full closure instead. 65536 gives generous
/// headroom over that while still bounding what a body of minimal pkt-lines
/// can force: at the cap, the line list costs on the order of 1.5 MB of `Vec`
/// headers, not the ~1 GB an unbounded 95 MiB body of 5-byte lines allocates.
const MAX_GIT_PKT_LINES: usize = 65_536;

/// encode one git pkt-line: a 4-hex length (INCLUDING the 4 length bytes)
/// followed by the payload. every line this bridge emits is tiny, well under
/// the 65516-byte payload cap, so no splitting is needed.
fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 4;
    let mut out = format!("{len:04x}").into_bytes();
    out.extend_from_slice(payload);
    out
}

/// split a leading pkt-line section off `buf`: parse length-framed lines until a
/// flush-pkt (`0000`), returning each payload (WITHOUT its 4-byte length header)
/// and the bytes AFTER the flush (for receive-pack, the raw packfile). a
/// truncated or malformed length is a clean error, never a panic — a corrupt
/// body becomes a 400.
fn parse_pkt_lines(buf: &[u8]) -> Result<(Vec<Vec<u8>>, &[u8]), String> {
    let mut lines = Vec::new();
    let mut rest = buf;
    loop {
        if rest.len() < 4 {
            return Err("truncated pkt-line length header".into());
        }
        let hdr =
            std::str::from_utf8(&rest[..4]).map_err(|_| "non-ascii pkt-line length".to_string())?;
        let len = usize::from_str_radix(hdr, 16)
            .map_err(|_| "invalid pkt-line length hex".to_string())?;
        if len == 0 {
            // flush-pkt terminates the command section; the rest is the pack.
            return Ok((lines, &rest[4..]));
        }
        if len < 4 || len > rest.len() {
            return Err("pkt-line length out of range".into());
        }
        if lines.len() >= MAX_GIT_PKT_LINES {
            return Err("too many pkt-lines in request".into());
        }
        lines.push(rest[4..len].to_vec());
        rest = &rest[len..];
    }
}

/// the parts of a v0 upload-pack request this server needs. haves bound the
/// pack: every have the repo knows hides its closure from the walk, so a
/// remote client refreshing its mirror downloads only what moved — the
/// remote-view lane syncs per head movement, and a full-closure answer there
/// would re-ship the whole repo every time.
struct UploadPackRequest {
    wants: Vec<String>,
    haves: Vec<String>,
    side_band: bool,
    done: bool,
}

/// parse the complete v0 upload-pack request, including the negotiation tail.
/// A stateless smart-HTTP client may end a round with a flush instead of `done`;
/// that round must receive only NAK so it can send another batch of haves.
fn parse_upload_pack_request(body: &[u8]) -> Result<UploadPackRequest, String> {
    let (lines, mut rest) = parse_pkt_lines(body)?;
    let mut wants = Vec::new();
    let mut side_band = false;
    let mut first_want = true;
    for line in &lines {
        let text = std::str::from_utf8(line)
            .map_err(|_| "non-utf8 want line".to_string())?
            .trim_end();
        let Some(want) = text.strip_prefix("want ") else {
            return Err("unexpected line in want section".into());
        };
        let mut toks = want.split(' ');
        let oid = toks
            .next()
            .filter(|oid| !oid.is_empty())
            .ok_or_else(|| "want line carried no oid".to_string())?;
        if git2::Oid::from_str(oid).is_err() {
            return Err("want line carried an invalid oid".into());
        }
        wants.push(oid.to_string());
        if first_want {
            side_band = toks.any(|cap| cap == "side-band-64k");
            first_want = false;
        }
    }
    if wants.is_empty() {
        return Err("request carried no want lines".into());
    }

    let mut haves = Vec::new();
    let mut done = false;
    let mut negotiation_lines = 0usize;
    while !rest.is_empty() {
        if done {
            return Err("upload-pack negotiation continued after done".into());
        }
        if rest.len() < 4 {
            return Err("truncated negotiation pkt-line length header".into());
        }
        if negotiation_lines >= MAX_GIT_PKT_LINES {
            return Err("too many negotiation pkt-lines in request".into());
        }
        negotiation_lines += 1;
        let hdr = std::str::from_utf8(&rest[..4])
            .map_err(|_| "non-ascii negotiation pkt-line length".to_string())?;
        let len = usize::from_str_radix(hdr, 16)
            .map_err(|_| "invalid negotiation pkt-line length hex".to_string())?;
        if len == 0 {
            rest = &rest[4..];
            continue;
        }
        if len < 4 || len > rest.len() {
            return Err("negotiation pkt-line length out of range".into());
        }
        let text = std::str::from_utf8(&rest[4..len])
            .map_err(|_| "non-utf8 negotiation line".to_string())?
            .trim_end();
        if text == "done" {
            done = true;
        } else if let Some(oid) = text.strip_prefix("have ") {
            if git2::Oid::from_str(oid).is_err() {
                return Err("have line carried an invalid oid".into());
            }
            haves.push(oid.to_string());
        } else {
            return Err("unexpected upload-pack negotiation line".into());
        }
        rest = &rest[len..];
    }

    Ok(UploadPackRequest {
        wants,
        haves,
        side_band,
        done,
    })
}

/// decode an even-length hex string to raw bytes; `None` on an odd length or any
/// non-hex nibble. turns a git pkt-line oid (40 hex) into raw sha1 bytes.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// the heads an advertisement offers — NOT the same set for the two services.
///
/// PUSH advertises forge's COMMITTED heads: the client builds its ref commands
/// against what it is shown and consensus gates each as a CAS against the
/// committed head, so advertising anything else mints a doomed push.
///
/// FETCH advertises what this node can actually SERVE — its ON-DISK refs. the
/// two diverge on a node whose objects have not caught up yet (a resident, or
/// a validator that was down for the push: see `RepoState::materialize`), and
/// there, offering the committed head is worse than offering the older one.
/// `git clone` wants every ref it was shown, the pack builder cannot walk an
/// oid whose objects are missing, and the error takes the WHOLE clone down —
/// not just the branch that lagged. an older head is what any mirror serves,
/// and the node's pack sweep catches it up within a tick.
async fn advertised_refs(
    handle: &ServiceState,
    repo: &str,
    service: GitService,
) -> Result<Vec<forge::RefHead>, Response> {
    match service {
        GitService::Receive => forge_refs(handle, repo).await,
        GitService::Upload => servable_refs(handle, repo)
            .map_err(|why| error_response(StatusCode::INTERNAL_SERVER_ERROR, &why)),
    }
}

/// the fetch half of [`advertised_refs`], reading the same on-disk repo
/// [`build_upload_pack`] packs from.
fn servable_refs(handle: &ServiceState, repo: &str) -> Result<Vec<forge::RefHead>, String> {
    on_disk_refs(&handle.forge_repo, repo).map_err(|e| format!("read forge refs: {e}"))
}

/// this node's on-disk branches for `repo`. a repo dir nothing has
/// materialized here yet is an empty listing, which advertises as an empty
/// repository — the same answer an unborn repo gives.
fn on_disk_refs(base: &std::path::Path, repo: &str) -> Result<Vec<forge::RefHead>, git2::Error> {
    let dir = base.join(repo);
    if !dir.join(".git").exists() {
        return Ok(Vec::new());
    }
    let repo = git2::Repository::open(&dir)?;
    Ok(forge::list_branches(&repo)?
        .into_iter()
        .map(|(name, head)| forge::RefHead {
            name,
            head: head.to_string(),
        })
        .collect())
}

/// query the forge module for a repo's committed branches (`[]` == unborn).
/// errors surface as an http `Response` so callers can early-return them.
async fn forge_refs(handle: &ServiceState, repo: &str) -> Result<Vec<forge::RefHead>, Response> {
    let result = handle
        .client
        .query(
            &handle.module,
            &forge::ForgeQuery::ListRefs {
                repo: repo.to_string(),
            },
        )
        .await;
    match result {
        Ok(forge::ForgeReply::Refs(refs)) => Ok(refs),
        Ok(_) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected ListRefs reply",
        )),
        Err(error) => Err(error_response(StatusCode::BAD_GATEWAY, &error.to_string())),
    }
}

/// a receive-pack command list, decoded.
#[derive(Debug)]
struct PushCommands {
    /// `(old hex, new hex, full refname)` per update — the report-status
    /// keys, in the order the pusher named them.
    cmds: Vec<(String, String, String)>,
    /// `git push --signed`'s certificate, when one rode along.
    cert: Option<forge::PushCert>,
}

const PUSH_CERT_LINE: &str = "push-cert";
const PUSH_CERT_END: &str = "push-cert-end";
const SSHSIG_ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";

/// Only a Git push certificate can authorize a service-submitted ref update.
fn push_may_carry_proof(commands: &[Vec<u8>]) -> bool {
    commands
        .first()
        .is_some_and(|first| command_text(first) == PUSH_CERT_LINE)
}

/// decode the command list. a stock push sends `<old> <new> <refname>` lines
/// (capabilities after a NUL on the first). a signed push (send-pack.c
/// `generate_push_cert`) sends `push-cert\0<caps>` instead, then every line
/// of the certificate — its text, then the armored signature — one pkt-line
/// each WITH its newline, then `push-cert-end`; the ref updates are inside
/// the certificate, and the plain lines are not sent. `expected_nonce` is
/// what this node advertised: a certificate must echo it EXACTLY (both the
/// chain half and the repo half). this check is only a front door — a
/// certificate never has to pass through it to reach consensus (any account
/// with `/v1/submit/frame` standing can carry one straight past this
/// function), so it does not by itself bound what a validator will accept.
/// consensus re-checks the full nonce itself, unaided (`pushcert::signer`,
/// #1773) — forge learns its own chain id through the same genesis-config
/// seam identity/gateway/runs use, so a certificate minted for a different
/// ducktape network is refused regardless of whether it slips past this
/// front door.
fn parse_push_commands(
    commands: &[Vec<u8>],
    expected_nonce: Option<&str>,
) -> Result<PushCommands, String> {
    let Some((first, rest)) = commands.split_first() else {
        return Err("empty command list".into());
    };
    let signed = command_text(first) == PUSH_CERT_LINE;
    if !signed {
        let cmds = commands
            .iter()
            .map(|raw| command_triple(command_text(raw)))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(PushCommands { cmds, cert: None });
    }
    let Some(expected_nonce) = expected_nonce else {
        return Err("this node offered no push-cert (its chain is not named yet)".into());
    };
    let (text, armor) = certificate_lines(rest)?;
    let sshsig = keyscheme::sshsig::dearmor(&armor)?;
    let certificate = forge::pushcert::parse(text.as_bytes())?;
    if certificate.nonce != expected_nonce {
        return Err(format!(
            "push certificate nonce {:?} is not this node's {expected_nonce:?}",
            certificate.nonce
        ));
    }
    let cmds = certificate
        .updates
        .iter()
        .map(|u| {
            (
                oid_hex(u.prev_oid.as_deref()),
                oid_hex(u.new_oid.as_deref()),
                format!("{GIT_HEADS_PREFIX}{}", u.ref_name),
            )
        })
        .collect();
    Ok(PushCommands {
        cmds,
        cert: Some(forge::PushCert {
            cert: text.into_bytes(),
            sshsig,
        }),
    })
}

/// the certificate's text and its armored signature, reassembled from the
/// pkt-lines between `push-cert` and `push-cert-end` — verbatim, newline for
/// newline, because the signature is over exactly those bytes.
fn certificate_lines(lines: &[Vec<u8>]) -> Result<(String, String), String> {
    let mut text = String::new();
    let mut armor = String::new();
    for raw in lines {
        let line = std::str::from_utf8(raw).map_err(|_| "push certificate is not utf-8")?;
        if line.trim_end() == PUSH_CERT_END {
            return Ok((text, armor));
        }
        let in_armor = !armor.is_empty() || line.starts_with(SSHSIG_ARMOR_BEGIN);
        if in_armor {
            armor.push_str(line);
        } else {
            text.push_str(line);
        }
    }
    Err("push certificate is not terminated by push-cert-end".into())
}

/// a command pkt-line's text: up to the NUL that starts the capability list
/// (first line only), trailing newline dropped.
fn command_text(raw: &[u8]) -> &str {
    let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    std::str::from_utf8(&raw[..nul])
        .map(str::trim_end)
        .unwrap_or("")
}

fn command_triple(line: &str) -> Result<(String, String, String), String> {
    let mut parts = line.split(' ');
    let (Some(old), Some(new), Some(refname)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("malformed ref-update command".into());
    };
    Ok((old.to_string(), new.to_string(), refname.to_string()))
}

fn oid_hex(oid: Option<&[u8]>) -> String {
    match oid {
        Some(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        None => GIT_ZERO_OID.to_string(),
    }
}

/// build a receive-pack `report-status` body: `unpack ok`, one status line per
/// ref, then a flush. each entry is `(full refname, None == ok | Some(reason)
/// == ng)`. forge's PushRefs is ATOMIC, so callers report one shared fate for
/// every ref of a push. the pack is always received by the time we answer, so
/// `unpack ok` is unconditional (we don't verify closure here).
fn git_report_status(results: &[(String, Option<String>)]) -> Response {
    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line(b"unpack ok\n"));
    for (refname, err) in results {
        let status_line = match err {
            None => format!("ok {refname}\n"),
            Some(reason) => format!("ng {refname} {reason}\n"),
        };
        body.extend_from_slice(&pkt_line(status_line.as_bytes()));
    }
    body.extend_from_slice(GIT_FLUSH_PKT);
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-receive-pack-result",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// query params for the ref advertisement; git always sends `service=`.
#[derive(Debug, Deserialize)]
pub struct InfoRefsParams {
    pub service: Option<String>,
}

/// which smart-HTTP service an info/refs advertisement is for — push
/// (receive-pack) or fetch (upload-pack). the two differ only in the banner,
/// the capability set, the content-type, and whether a `HEAD` line rides along.
#[derive(Clone, Copy)]
enum GitService {
    Receive,
    Upload,
}

impl GitService {
    fn name(self) -> &'static str {
        match self {
            Self::Receive => "git-receive-pack",
            Self::Upload => "git-upload-pack",
        }
    }
    fn caps(self) -> &'static str {
        match self {
            Self::Receive => GIT_RECEIVE_PACK_CAPS,
            Self::Upload => GIT_UPLOAD_PACK_CAPS,
        }
    }
    fn advertisement_content_type(self) -> &'static str {
        match self {
            Self::Receive => "application/x-git-receive-pack-advertisement",
            Self::Upload => "application/x-git-upload-pack-advertisement",
        }
    }
}

/// the capability line for `service` on `repo`. receive-pack additionally
/// offers `push-cert=<nonce>` — the invitation `git push --signed` needs
/// (git refuses to sign a push the server did not offer a nonce for) — once
/// this node knows its chain. The nonce is whatever `forge::pushcert::nonce`
/// makes of that chain and repo, and it is git's to accept: it validates the
/// value it is handed before signing, so the shape lives in that one function
/// and is checked against git's rules by that module's own test.
fn advertised_caps(handle: &ServiceState, repo: &str, service: GitService) -> String {
    let base = service.caps();
    let nonce = match service {
        GitService::Receive => push_cert_nonce(handle, repo),
        GitService::Upload => None,
    };
    match nonce {
        Some(nonce) => format!("{base} push-cert={nonce}"),
        None => base.to_string(),
    }
}

/// the push-cert nonce this node offers for `repo`; `None` until the status
/// cell names the chain (a node still booting offers no signed pushes).
fn push_cert_nonce(handle: &ServiceState, repo: &str) -> Option<String> {
    let chain_id = &handle.chain_id;
    let named = !chain_id.is_empty();
    named.then(|| forge::pushcert::nonce(chain_id, repo))
}

/// GET /{repo}/info/refs?service=… — the smart-HTTP ref advertisement a
/// `git push`/`git clone` fetches FIRST to learn the remote's current head.
/// which heads those are differs per service (see [`advertised_refs`]). both
/// receive-pack (push) and upload-pack (fetch) are served — the v0 banner we
/// send makes git speak the classic protocol for the follow-up POST even when it
/// probed with `Git-Protocol: version=2`.
///
/// AUTH: a READ, for BOTH services, and it has to be. The receive-pack
/// advertisement is what a client fetches to learn the head it is fast-forwarding
/// from and the `push-cert` nonce it must sign over — so requiring a credential
/// HERE would make `git push --signed` impossible for exactly the person whose
/// certificate is the credential [`git_receive_pack`] wants. It gives nothing
/// away either: the nonce is a pure function of chain id and repo name
/// ([`push_cert_nonce`]) and mints no state, and the refs it lists are the same
/// refs the open upload-pack advertisement hands any clone. The proof is
/// demanded where the mutation is — on the receive-pack POST.
pub(crate) async fn git_info_refs(
    State(handle): State<ServiceState>,
    Path(repo): Path<String>,
    Query(params): Query<InfoRefsParams>,
) -> Response {
    let Ok(repo) = forge::norm_repo(&repo) else {
        return error_response(StatusCode::NOT_FOUND, "no such repo");
    };
    let service = match params.service.as_deref() {
        Some("git-receive-pack") => GitService::Receive,
        Some("git-upload-pack") => GitService::Upload,
        _ => {
            return error_response(
                StatusCode::FORBIDDEN,
                "only git-receive-pack and git-upload-pack are served",
            );
        }
    };
    git_advertise_refs(&handle, &repo, service).await
}

/// the branch a clone checks out: the repo's integration branch, else `main`.
///
/// Same order the forge module itself resolves a repo's head in
/// (`forge::query`'s `revision`, and what `RepoHead::head` documents), because
/// it is the same question. A repo seeded on `dev` has no `main` at all, and a
/// client told nothing falls back to a `refs/heads/main` that does not exist —
/// it clones every ref and lands on an UNBORN HEAD.
fn default_branch(refs: &[forge::RefHead]) -> Option<&forge::RefHead> {
    let named = |branch: &'static str| refs.iter().find(move |r| r.name == branch);
    named(forge::refs::INTEGRATION_BRANCH).or_else(|| named(forge::refs::MAIN_BRANCH))
}

/// build the smart-HTTP ref advertisement for `service`: the service banner, a
/// flush, the ref line(s), then a flush. an unborn repo advertises the null oid
/// against the magic `capabilities^{}` ref (so caps ride along with no real ref)
/// — a clone then reports an empty repository. a born repo advertises EVERY
/// committed branch; a fetch advertisement leads with a `HEAD` line at the
/// [`default_branch`]'s oid so `git clone` resolves the branch to check out.
/// capabilities ride the first emitted line after a NUL, per the v0 protocol.
async fn git_advertise_refs(handle: &ServiceState, repo: &str, service: GitService) -> Response {
    let refs = match advertised_refs(handle, repo, service).await {
        Ok(refs) => refs,
        Err(resp) => return resp,
    };
    let default = match service {
        GitService::Upload => default_branch(&refs),
        GitService::Receive => None,
    };
    let caps = advertised_caps(handle, repo, service);
    // `symref` NAMES the branch. Without it a client has to guess HEAD by
    // matching its oid against the advertised refs, and a feature branch cut
    // from the default sits on that same oid until its first commit — so the
    // guess is wrong exactly when a run has just branched.
    let caps = match default {
        Some(r) => format!("{caps} symref=HEAD:{GIT_HEADS_PREFIX}{}", r.name),
        None => caps,
    };

    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line(
        format!("# service={}\n", service.name()).as_bytes(),
    ));
    body.extend_from_slice(GIT_FLUSH_PKT);
    if refs.is_empty() {
        body.extend_from_slice(&pkt_line(
            format!("{GIT_ZERO_OID} capabilities^{{}}\0{caps}\n").as_bytes(),
        ));
    } else {
        let mut lines: Vec<String> = Vec::new();
        if let Some(r) = default {
            lines.push(format!("{} HEAD", r.head));
        }
        for r in &refs {
            lines.push(format!("{} {GIT_HEADS_PREFIX}{}", r.head, r.name));
        }
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                body.extend_from_slice(&pkt_line(format!("{line}\0{caps}\n").as_bytes()));
            } else {
                body.extend_from_slice(&pkt_line(format!("{line}\n").as_bytes()));
            }
        }
    }
    body.extend_from_slice(GIT_FLUSH_PKT);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, service.advertisement_content_type()),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// the two ways [`decode_git_body`] can fail: a malformed gzip stream, or one
/// that inflates past its cap — a would-be zip bomb.
#[derive(Debug)]
enum GitBodyError {
    BadEncoding(String),
    OverCap,
}

/// a push's body on disk: what the client sent (gzip already inflated), and
/// where its packfile starts once the command section has been read. The file
/// goes away with this value, whether the push landed or died.
pub(crate) struct SpooledPush {
    path: std::path::PathBuf,
    len: u64,
}

impl Drop for SpooledPush {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Stream a request body onto disk under `<git_store>/.incoming/`, inflating
/// `Content-Encoding: gzip` as it goes. THE SIZE IS NOT BOUNDED — a push
/// carries whatever history it carries, and holding it in this process to
/// measure it is the thing this avoids. What is bounded is gzip's expansion
/// ([`GIT_MAX_INFLATE_RATIO`]), because a bomb is not a push.
async fn spool_request_body(
    git_store: &std::path::Path,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<SpooledPush, GitBodyError> {
    use futures::StreamExt as _;
    use std::io::Write;

    let dir = git_store.join(".incoming");
    std::fs::create_dir_all(&dir)
        .map_err(|e| GitBodyError::BadEncoding(format!("cannot spool the push: {e}")))?;
    let path = dir.join(format!(
        "push-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
    ));
    let file = std::fs::File::create(&path)
        .map_err(|e| GitBodyError::BadEncoding(format!("cannot spool the push: {e}")))?;
    // the file is live from here: this guard deletes it on every exit below,
    // including the `?`s, and hands it to the caller on success.
    let mut spooled = SpooledPush { path, len: 0 };

    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    // `write::GzDecoder` inflates what is WRITTEN into it, which is what lets
    // a compressed body stream through this door instead of being
    // materialized to be decoded.
    let mut sink: Box<dyn Write + Send> = match gzip {
        true => Box::new(flate2::write::GzDecoder::new(file)),
        false => Box::new(file),
    };
    let mut received = 0u64;
    let mut frames = body.into_data_stream();
    while let Some(frame) = frames.next().await {
        let frame = frame.map_err(|e| GitBodyError::BadEncoding(format!("body: {e}")))?;
        received += frame.len() as u64;
        sink.write_all(&frame)
            .map_err(|e| GitBodyError::BadEncoding(format!("cannot spool the push: {e}")))?;
        let inflated = std::fs::metadata(&spooled.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let bomb = gzip && inflated > received.saturating_mul(GIT_MAX_INFLATE_RATIO);
        if bomb {
            return Err(GitBodyError::OverCap);
        }
    }
    sink.flush()
        .map_err(|e| GitBodyError::BadEncoding(format!("cannot spool the push: {e}")))?;
    drop(sink);
    spooled.len = std::fs::metadata(&spooled.path)
        .map_err(|e| GitBodyError::BadEncoding(format!("cannot spool the push: {e}")))?
        .len();
    Ok(spooled)
}

impl SpooledPush {
    /// the pkt-line command section at the head of the body, plus the offset
    /// its packfile starts at. Read in doublings so a push updating thousands
    /// of refs is read further rather than refused.
    fn commands(&self) -> Result<(Vec<Vec<u8>>, u64), String> {
        use std::io::Read as _;
        let mut want = GIT_COMMAND_HEAD_BYTES;
        loop {
            let mut head = vec![0u8; want.min(self.len as usize)];
            let mut file = std::fs::File::open(&self.path).map_err(|e| e.to_string())?;
            file.read_exact(&mut head).map_err(|e| e.to_string())?;
            let read_everything = head.len() as u64 == self.len;
            match parse_pkt_lines(&head) {
                Ok((lines, rest)) => {
                    let pack_offset = (head.len() - rest.len()) as u64;
                    return Ok((lines, pack_offset));
                }
                // the command section did not fit the window — unless the
                // window WAS the whole body, in which case it is malformed.
                Err(detail) if !read_everything => {
                    let truncated = detail.contains("truncated") || detail.contains("out of range");
                    if !truncated {
                        return Err(detail);
                    }
                    want *= 2;
                }
                Err(detail) => return Err(detail),
            }
        }
    }
}

/// return the request body, gzip-inflated if `Content-Encoding: gzip`. git
/// compresses a fetch's negotiation list; any other encoding is passed through.
///
/// A PUSH does not come through here — it streams to disk
/// ([`spool_request_body`]) because it has no size limit to be measured
/// against. What remains is the negotiation lane, and there the inflate is
/// read through `cap` because gzip's max compression ratio is ~1030:1: an
/// uncapped `read_to_end` on a body that already fits the compressed limit
/// could still allocate tens of gigabytes.
fn decode_git_body(headers: &HeaderMap, body: &[u8], cap: usize) -> Result<Vec<u8>, GitBodyError> {
    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    if !gzip {
        return Ok(body.to_vec());
    }
    use std::io::Read as _;
    let mut out = Vec::new();
    // read one byte past `cap`: a body that inflates to EXACTLY `cap` bytes
    // still decodes below, while anything larger trips the length check.
    flate2::read::GzDecoder::new(body)
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| GitBodyError::BadEncoding(format!("gzip inflate failed: {e}")))?;
    if out.len() > cap {
        return Err(GitBodyError::OverCap);
    }
    Ok(out)
}

/// one refused push, on the forge plane. the http funnel already records the
/// status at `debug`; this is the plane's own line, with a `reason` an
/// operator greps and counts. never the pack, never a path.
fn push_refused(_repo: &str, reason: &'static str, _detail: &str) {
    tracing::warn!(
        target: "ducktape::forge",
        event = "forge_push_refused",
        reason,
        "push refused"
    );
}

/// the `reason` behind a push refusal that arrived as prose: this bridge's
/// own parse errors and forge's consensus rejections both come back as the
/// sentence git prints per ref. the nonce messages also say "certificate",
/// so they sit ahead of that catch-all.
fn push_refusal_reason(message: &str) -> &'static str {
    const KNOWN: &[(&str, &str)] = &[
        ("non-fast-forward", "non_fast_forward"),
        ("requires an authenticated external origin", "unsigned"),
        ("offered no push-cert", "push_cert_unoffered"),
        ("signature does not verify", "bad_cert_signature"),
        ("nonce", "bad_cert_nonce"),
        ("certificate", "bad_cert"),
        ("too many ref updates", "too_many_refs"),
    ];
    KNOWN
        .iter()
        .find_map(|(text, reason)| message.contains(text).then_some(*reason))
        .unwrap_or("rejected")
}

/// POST /{repo}/git-receive-pack — receive a push: parse the ref-update
/// command list + packfile, stash the whole pack in the node-local blob store,
/// and CAS every branch through ONE atomic forge `PushRefs` op (one submit ==
/// one block). branch deletions (`:feature`) ride the same op pack-free. the
/// response is a git `report-status` reflecting the push's shared fate.
pub(crate) async fn git_receive_pack(
    State(handle): State<ServiceState>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let Ok(repo) = forge::norm_repo(&repo) else {
        return error_response(StatusCode::NOT_FOUND, "no such repo");
    };
    // the push lands on DISK, however big it is: this bridge holds one frame
    // of it at a time, and the packfile goes on to the node's store from the
    // file rather than through this process's memory.
    let spooled = match spool_request_body(&handle.forge_repo, &headers, body).await {
        Ok(spooled) => spooled,
        Err(GitBodyError::OverCap) => {
            const REASON: &str = "gzip-encoded body inflates beyond any plausible ratio";
            push_refused(&repo, "gzip_bomb", REASON);
            return error_response(StatusCode::BAD_REQUEST, REASON);
        }
        Err(GitBodyError::BadEncoding(msg)) => {
            push_refused(&repo, "bad_encoding", &msg);
            return error_response(StatusCode::BAD_REQUEST, &msg);
        }
    };

    // the body is a pkt-line command list, a flush-pkt, then the raw packfile.
    let (commands, pack_offset) = match spooled.commands() {
        Ok(parsed) => parsed,
        Err(msg) => {
            push_refused(&repo, "malformed_commands", &msg);
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("malformed git command stream: {msg}"),
            );
        }
    };
    if commands.is_empty() {
        // a push whose pack exceeds git's `http.postBuffer` (1 MiB default) is
        // preceded by a flush-only PROBE POST (Content-Length: 4, body `0000`,
        // zero commands) before git streams the real chunked request. an empty
        // command list is a valid no-op: answer 200 with an empty result so the
        // probe succeeds and git proceeds with the actual push. 400 here aborts
        // every push larger than the post buffer.
        return (
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    "application/x-git-receive-pack-result",
                ),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            GIT_FLUSH_PKT.to_vec(),
        )
            .into_response();
    }

    // A PUSH MUST PROVE ITSELF, exactly like every other mutating route — and
    // it proves itself one of the two ways git can carry:
    //
    // The installed service has its own transport key. User ref authority
    // always comes from a Git certificate checked again by the guest.
    if !push_may_carry_proof(&commands) {
        const REFUSAL: &str = "this push carries no proof: use git push --signed";
        push_refused(&repo, "push_unauthenticated", REFUSAL);
        // CONSUME-AND-REFUSE, LIKE EVERY OTHER PUSH REFUSAL BELOW. The pack is
        // fully received by now, and an HTTP 401 at this point is what git
        // prints as "the remote end hung up unexpectedly" — the one sentence
        // that says nothing about signing. A `report-status` with `ng` per ref
        // is what git renders as
        // `! [remote rejected] main -> main (<reason>)`, so the reason reaches
        // the person who has to act on it. Naming the refs is the PLAIN line
        // split: the gate above has just established this body claims no
        // certificate, so nothing is dearmored to answer it. A list that is
        // not plain lines either has no refs to name — that is the malformed
        // stream the parse below reports.
        let Ok(PushCommands { cmds, .. }) = parse_push_commands(&commands, None) else {
            return error_response(StatusCode::BAD_REQUEST, "malformed git command stream");
        };
        let results: Vec<(String, Option<String>)> = cmds
            .into_iter()
            .map(|(_, _, refname)| (refname, Some(REFUSAL.to_string())))
            .collect();
        return git_report_status(&results);
    }

    // the command list: plain `<old> <new> <refname>` lines, or — a signed
    // push — the certificate they live in. one push may update several
    // branches, and forge applies them ATOMICALLY.
    let nonce = push_cert_nonce(&handle, &repo);
    let PushCommands { cmds, cert } = match parse_push_commands(&commands, nonce.as_deref()) {
        Ok(parsed) => parsed,
        Err(msg) => {
            push_refused(&repo, push_refusal_reason(&msg), &msg);
            return error_response(StatusCode::BAD_REQUEST, &msg);
        }
    };

    // only branches are pushable (no tags/notes). consume-and-refuse: the pack
    // was fully received; reporting `ng` (not an http error) lets git print a
    // clean per-ref reason.
    if cmds
        .iter()
        .any(|(_, _, r)| !r.starts_with(GIT_HEADS_PREFIX))
    {
        push_refused(&repo, "ref_outside_heads", "only refs/heads/* is supported");
        let results: Vec<(String, Option<String>)> = cmds
            .into_iter()
            .map(|(_, _, r)| (r, Some(format!("only {GIT_HEADS_PREFIX}* is supported"))))
            .collect();
        return git_report_status(&results);
    }

    // old/new == the null oid mean "create" (prev_oid None) / "delete" (new_oid
    // None); otherwise 40-hex oids the forge per-branch CAS must match.
    let mut updates = Vec::new();
    for (old, new, refname) in &cmds {
        let prev_oid = if old == GIT_ZERO_OID {
            None
        } else {
            match hex_to_bytes(old).filter(|b| b.len() == GIT_OID_RAW_LEN) {
                Some(bytes) => Some(bytes),
                None => {
                    push_refused(&repo, "malformed_oid", "malformed old oid");
                    return error_response(StatusCode::BAD_REQUEST, "malformed old oid");
                }
            }
        };
        let new_oid = if new == GIT_ZERO_OID {
            None
        } else {
            match hex_to_bytes(new).filter(|b| b.len() == GIT_OID_RAW_LEN) {
                Some(bytes) => Some(bytes),
                None => {
                    push_refused(&repo, "malformed_oid", "malformed new oid");
                    return error_response(StatusCode::BAD_REQUEST, "malformed new oid");
                }
            }
        };
        updates.push(forge::RefUpdate {
            ref_name: refname[GIT_HEADS_PREFIX.len()..].to_string(),
            prev_oid,
            new_oid,
        });
    }

    // a signed push is refused HERE with the reason consensus would give — a
    // clean per-ref `ng` instead of a rejected block. every validator
    // re-verifies; this node is not trusted for it.
    if let Some(cert) = &cert
        && let Err(reason) = forge::pushcert::signer(cert, &handle.chain_id, &repo, &updates)
    {
        push_refused(&repo, push_refusal_reason(&reason), &reason);
        let results: Vec<(String, Option<String>)> = cmds
            .into_iter()
            .map(|(_, _, r)| (r, Some(reason.clone())))
            .collect();
        return git_report_status(&results);
    }

    // stash the WHOLE packfile as one node-local blob, keyed by its sha256;
    // forge materializes it by this digest (the bytes never cross consensus).
    // a delete-only push carries no objects, so nothing is stashed. The bytes
    // stream from the spool file straight into the node's store — neither end
    // ever holds the pack.
    let pack_bytes = spooled.len.saturating_sub(pack_offset);
    let pack_digest = if updates.iter().any(|u| u.new_oid.is_some()) {
        match handle
            .client
            .put_blob_file(&spooled.path, pack_offset)
            .await
        {
            Ok(digest) => match hex::decode(digest)
                .ok()
                .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            {
                Some(digest) => Some(digest),
                None => return error_response(StatusCode::BAD_GATEWAY, "invalid blob digest"),
            },
            Err(error) => return error_response(StatusCode::BAD_GATEWAY, &error.to_string()),
        }
    } else {
        None
    };

    // CAS every branch through ONE atomic PushRefs op and await the block.
    let payload = forge::encode_msg(&forge::ForgeMsg::PushRefs {
        repo: repo.clone(),
        updates,
        pack_digest: pack_digest.map(|digest| digest.to_vec()),
        cert,
    });
    let submitted = handle.submit(payload, pack_digest).await;
    let refnames: Vec<String> = cmds.into_iter().map(|(_, _, r)| r).collect();
    match submitted {
        Ok(height) => {
            tracing::info!(
                target: "ducktape::forge",
                event = "forge_push_landed",
                repo = %repo,
                refs = refnames.len(),
                pack_bytes,
                height,
                "push landed"
            );
            let results: Vec<(String, Option<String>)> =
                refnames.into_iter().map(|r| (r, None)).collect();
            git_report_status(&results)
        }
        Err(reason) => {
            push_refused(&repo, push_refusal_reason(&reason), &reason);
            // a CAS mismatch's rejection carries "non-fast-forward" — surface
            // exactly that token so git prints its standard "fetch first" hint.
            // any other rejection passes through as a single-line reason. the
            // op is atomic, so every ref shares the fate.
            let reason = if reason.contains("non-fast-forward") {
                "non-fast-forward".to_string()
            } else {
                reason.replace('\n', " ")
            };
            let results: Vec<(String, Option<String>)> = refnames
                .into_iter()
                .map(|r| (r, Some(reason.clone())))
                .collect();
            git_report_status(&results)
        }
    }
}

/// POST /{repo}/git-upload-pack — serve a fetch/clone. parse the pkt-line
/// negotiation (`want <oid>` lines, capabilities on the FIRST; flush-ended
/// `have` rounds receive plain NAK so the client keeps batching), open
/// `<forge_repo>/{repo}` READ-ONLY, and after `done` answer with the pack on
/// side-band-64k band 1: a have-bounded delta behind `ACK <common>` when the
/// repo knows any of the client's haves, or the full closure behind NAK when
/// it knows none (see [`build_upload_pack`]).
pub(crate) async fn git_upload_pack(
    State(handle): State<ServiceState>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Ok(repo) = forge::norm_repo(&repo) else {
        return error_response(StatusCode::NOT_FOUND, "no such repo");
    };
    let forge_repo = handle.forge_repo.clone();
    let body = match body {
        Ok(bytes) => bytes,
        // the DefaultBodyLimit layer rejects an oversized request with 413.
        Err(rejection) => return error_response(rejection.status(), &rejection.body_text()),
    };
    let body = match decode_git_body(&headers, &body, GIT_NEGOTIATION_BODY_LIMIT) {
        Ok(bytes) => bytes,
        Err(GitBodyError::OverCap) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "gzip-inflated body exceeds the pack body limit",
            );
        }
        Err(GitBodyError::BadEncoding(msg)) => {
            return error_response(StatusCode::BAD_REQUEST, &msg);
        }
    };

    let request = match parse_upload_pack_request(&body) {
        Ok(request) => request,
        Err(msg) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("malformed git-upload-pack request: {msg}"),
            );
        }
    };

    // A flush-ended have batch is an intermediate negotiation round. Returning
    // only NAK (and no side-band/PACK bytes) lets stock git send its next batch.
    if !request.done {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            pkt_line(b"NAK\n"),
        )
            .into_response();
    }

    // the pack build is blocking git2 IO over a non-Send `Repository`; run it off
    // the async worker, moving only Send data (the dir + hex oids) across.
    let repo_dir = forge_repo.join(&repo);
    let UploadPackRequest {
        wants,
        haves,
        side_band,
        ..
    } = request;
    let (pack, common) =
        match tokio::task::spawn_blocking(move || build_upload_pack(&repo_dir, &wants, &haves))
            .await
        {
            Ok(Ok(built)) => built,
            Ok(Err(UploadPackError::RepoUnavailable(e))) => {
                // the git2 detail (which carries the node's absolute forge
                // path) never reaches the client or the warn-level ring; an
                // absent/unopenable repo dir is just a 404 to the outside.
                tracing::debug!(
                    target: "ducktape::forge",
                    repo = %repo,
                    error = %e,
                    "forge repo unavailable for upload-pack"
                );
                return error_response(StatusCode::NOT_FOUND, "no such repo");
            }
            Ok(Err(UploadPackError::WantNotAdvertised(hex))) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("want {hex} is not one of this repo's advertised refs"),
                );
            }
            Ok(Err(UploadPackError::Other(msg))) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, &msg);
            }
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "git pack builder task panicked",
                );
            }
        };

    tracing::debug!(
        target: "ducktape::forge",
        repo = %repo,
        pack_bytes = pack.len(),
        delta = common.is_some(),
        "upload-pack served"
    );

    let mut out = Vec::new();
    // the terminal negotiation line, valid in every v0 multi_ack mode: a bare
    // `ACK <oid>` names the common base the pack builds on (the delta answer),
    // NAK means no usable have was found (the pack is then the full closure).
    // either way a PLAIN pkt-line, BEFORE any side-band framing begins.
    match &common {
        Some(oid) => out.extend_from_slice(&pkt_line(format!("ACK {oid}\n").as_bytes())),
        None => out.extend_from_slice(&pkt_line(b"NAK\n")),
    }
    if side_band {
        // band 1 = pack data, chunked to the side-band-64k ceiling.
        for chunk in pack.chunks(GIT_SIDE_BAND_CHUNK) {
            let mut framed = Vec::with_capacity(chunk.len() + 1);
            framed.push(0x01);
            framed.extend_from_slice(chunk);
            out.extend_from_slice(&pkt_line(&framed));
        }
        out.extend_from_slice(GIT_FLUSH_PKT);
    } else {
        // the client didn't request side-band: the raw pack follows NAK directly
        // (no band framing, no trailing flush — the pack trailer ends the stream).
        out.extend_from_slice(&pack);
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        out,
    )
        .into_response()
}

/// [`build_upload_pack`]'s failure modes. `RepoUnavailable` carries the raw
/// git2 error — libgit2 puts the repo's absolute path verbatim in that
/// message (`repository.c`'s "could not find repository at '%s'"), so it is
/// NEVER surfaced to the client or put in the log ring's warn line; the
/// handler answers a fixed 404 and logs this variant's detail at `debug`
/// only. `WantNotAdvertised` is a refusal, not a server error: the client
/// asked for an oid this node does not currently advertise as a branch tip.
/// `Other` covers everything past those two (a bad want oid, a pack-write
/// failure) and is not path-bearing.
#[derive(Debug)]
enum UploadPackError {
    RepoUnavailable(git2::Error),
    WantNotAdvertised(String),
    Other(String),
}

/// build the packfile answering `want_hexes`, bounded by the client's haves:
/// every have this repo knows as a commit hides its closure from the walk
/// (forge's `pack_delta`), so a mirror refresh downloads only what moved. a
/// client with NO usable common base still gets the FULL self-contained
/// closure (forge's `pack_closure_many` — ONE packing implementation for the
/// module's snapshot pack and this fetch lane). returns the pack plus the
/// first usable common base, which the handler ACKs.
///
/// every want must equal one of this repo's current branch tips — the same
/// anti-amplifier `forge::build_objects` enforces on the peer lane ("that
/// guard is the whole anti-amplifier"): a caller may only ask for history
/// this node still advertises, never an arbitrary walk of its object
/// database by oid.
fn build_upload_pack(
    repo_dir: &std::path::Path,
    want_hexes: &[String],
    have_hexes: &[String],
) -> Result<(Vec<u8>, Option<String>), UploadPackError> {
    let repo = git2::Repository::open(repo_dir).map_err(UploadPackError::RepoUnavailable)?;
    let tips: Vec<git2::Oid> = forge::list_branches(&repo)
        .map_err(|e| UploadPackError::Other(format!("read refs: {e}")))?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    let mut oids = Vec::with_capacity(want_hexes.len());
    for hex in want_hexes {
        let oid = git2::Oid::from_str(hex)
            .map_err(|e| UploadPackError::Other(format!("bad want oid {hex}: {e}")))?;
        if !tips.contains(&oid) {
            return Err(UploadPackError::WantNotAdvertised(hex.clone()));
        }
        oids.push(oid);
    }
    // only haves this repo KNOWS as commits can bound the walk — a have from
    // history this node never saw simply doesn't help (and never errors).
    let mut common = Vec::new();
    for hex in have_hexes {
        let Ok(oid) = git2::Oid::from_str(hex) else {
            continue; // parser already validated; belt and braces
        };
        if repo.find_commit(oid).is_ok() {
            common.push(oid);
        }
    }
    if common.is_empty() {
        return forge::pack_closure_many(&repo, &oids)
            .map(|pack| (pack, None))
            .map_err(|e| UploadPackError::Other(format!("build pack: {e}")));
    }
    let ack = common[0].to_string();
    forge::pack_delta(&repo, &oids, &common)
        .map(|pack| (pack, Some(ack)))
        .map_err(|e| UploadPackError::Other(format!("build delta pack: {e}")))
}

#[cfg(test)]
mod decode_git_body_tests {
    use super::*;
    use std::io::Write as _;

    fn gzip_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        headers
    }

    fn gzip_of_zeros(n: usize) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&vec![0u8; n]).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn a_gzip_body_that_inflates_past_the_cap_is_refused_one_under_it_decodes() {
        let cap = 4096usize;
        let headers = gzip_headers();

        let at_cap = gzip_of_zeros(cap);
        let decoded = decode_git_body(&headers, &at_cap, cap).expect("exactly at the cap decodes");
        assert_eq!(decoded.len(), cap);

        let over_cap = gzip_of_zeros(cap + 1);
        let err = decode_git_body(&headers, &over_cap, cap)
            .expect_err("past the cap must be refused, not fully inflated");
        assert!(matches!(err, GitBodyError::OverCap));
    }
}

#[cfg(test)]
mod receive_pack_tests {
    use super::*;

    /// the real `ssh-keygen -Y sign -n git` fixture keyscheme and forge pin,
    /// framed exactly as `git push --signed` frames it.
    const CERT: &str = "certificate version 0.1\npusher key::ssh-ed25519 AAAA 1756332000 +0000\npushee http://127.0.0.1:8844/forge/lab\nnonce chain-a/lab\n\n0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/heads/main\n";
    const ARMORED: &str = "-----BEGIN SSH SIGNATURE-----\n\
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgJjhQt02r3vG8+pxaBdryKnexRC\n\
cULQqMrrcadzt/2iEAAAADZ2l0AAAAAAAAAAZzaGE1MTIAAABTAAAAC3NzaC1lZDI1NTE5\n\
AAAAQAkqyuC4rshUkBgUVsgAqGxBltLKRLcwdq5LAQn+2lCUmiUJWTsYTykmuaNO+cntB2\n\
ZYBzkWoVNWmNV5YTCuZwE=\n\
-----END SSH SIGNATURE-----\n";

    fn signed_commands() -> Vec<Vec<u8>> {
        let mut lines = vec![b"push-cert\0report-status agent=git/2.43.0\n".to_vec()];
        for line in CERT.split_inclusive('\n') {
            lines.push(line.as_bytes().to_vec());
        }
        for line in ARMORED.split_inclusive('\n') {
            lines.push(line.as_bytes().to_vec());
        }
        lines.push(b"push-cert-end\n".to_vec());
        lines
    }

    #[test]
    fn a_signed_push_yields_its_certificate_and_the_moves_inside_it() {
        let parsed = parse_push_commands(&signed_commands(), Some("chain-a/lab")).unwrap();
        assert_eq!(
            parsed.cmds,
            vec![(
                GIT_ZERO_OID.to_string(),
                "ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2".to_string(),
                "refs/heads/main".to_string()
            )]
        );
        let cert = parsed.cert.expect("a certificate");
        assert_eq!(cert.cert, CERT.as_bytes(), "the signed bytes, verbatim");
        assert_eq!(cert.sshsig, keyscheme::sshsig::dearmor(ARMORED).unwrap());
        // and it is what consensus will accept.
        let updates = vec![forge::RefUpdate {
            ref_name: "main".into(),
            prev_oid: None,
            new_oid: Some(hex_to_bytes("ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2").unwrap()),
        }];
        forge::pushcert::signer(&cert, "chain-a", "lab", &updates).expect("verifies");
    }

    #[test]
    fn a_certificate_must_echo_this_nodes_nonce_and_be_terminated() {
        let wrong = parse_push_commands(&signed_commands(), Some("chain-b/lab")).unwrap_err();
        assert!(wrong.contains("nonce"), "{wrong}");
        let unoffered = parse_push_commands(&signed_commands(), None).unwrap_err();
        assert!(unoffered.contains("offered no push-cert"), "{unoffered}");
        let mut cut = signed_commands();
        cut.pop();
        let cut = parse_push_commands(&cut, Some("chain-a/lab")).unwrap_err();
        assert!(cut.contains("push-cert-end"), "{cut}");
    }

    #[test]
    fn a_stock_push_still_parses_line_by_line() {
        let commands = vec![
            b"0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/heads/main\0report-status\n".to_vec(),
            b"ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 0000000000000000000000000000000000000000 refs/heads/old\n".to_vec(),
        ];
        let parsed = parse_push_commands(&commands, None).unwrap();
        assert!(parsed.cert.is_none());
        assert_eq!(parsed.cmds.len(), 2);
        assert_eq!(parsed.cmds[1].2, "refs/heads/old");
        assert!(parse_push_commands(&[b"junk\n".to_vec()], None).is_err());
    }

    /// receive-pack refuses a credential-less push BEFORE it ever attempts
    /// the certificate parse — checked on a command list that would fail
    /// `parse_push_commands` outright (neither valid triples nor a
    /// certificate), so the gate cannot be reading anything the parse
    /// produced.
    #[test]
    fn a_credential_less_push_is_refused_before_the_certificate_parse() {
        let unparseable_junk = vec![b"junk\n".to_vec()];
        assert!(
            !push_may_carry_proof(&unparseable_junk),
            "no operator header and no push-cert claim: refused pre-parse"
        );
        assert!(
            parse_push_commands(&unparseable_junk, None).is_err(),
            "would ALSO fail to parse"
        );

        assert!(
            push_may_carry_proof(&signed_commands()),
            "a body claiming push-cert earns the (cheap) certificate parse"
        );
    }
}

#[cfg(test)]
mod upload_pack_tests {
    use super::*;

    const WANT: &str = "1111111111111111111111111111111111111111";
    const HAVE: &str = "2222222222222222222222222222222222222222";

    fn request_tail(tail: &[u8]) -> Vec<u8> {
        let mut body =
            pkt_line(format!("want {WANT} multi_ack_detailed side-band-64k\n").as_bytes());
        body.extend_from_slice(GIT_FLUSH_PKT);
        body.extend_from_slice(tail);
        body
    }

    #[test]
    fn flush_ended_have_round_is_not_done() {
        let mut tail = pkt_line(format!("have {HAVE}\n").as_bytes());
        tail.extend_from_slice(GIT_FLUSH_PKT);

        let parsed = parse_upload_pack_request(&request_tail(&tail)).expect("valid request");

        assert_eq!(parsed.wants, vec![WANT.to_string()]);
        assert_eq!(parsed.haves, vec![HAVE.to_string()]);
        assert!(parsed.side_band);
        assert!(!parsed.done, "a have flush must not authorize pack bytes");
    }

    #[test]
    fn explicit_done_completes_negotiation() {
        let mut tail = pkt_line(format!("have {HAVE}\n").as_bytes());
        tail.extend_from_slice(&pkt_line(b"done\n"));

        let parsed = parse_upload_pack_request(&request_tail(&tail)).expect("valid request");

        assert!(parsed.done);
        assert!(parsed.side_band);
        assert_eq!(parsed.haves, vec![HAVE.to_string()]);
    }

    /// a fetch advertisement offers exactly what this node can pack: nothing
    /// for a repo it has never materialized, and afterwards the ON-DISK heads
    /// — never a committed head whose objects have not arrived, which would
    /// take the whole clone down instead of just lagging one branch.
    #[test]
    fn on_disk_refs_offer_only_the_branches_this_node_can_pack() {
        let base = tempfile::tempdir().unwrap();
        assert!(
            on_disk_refs(base.path(), "demo").unwrap().is_empty(),
            "a repo nothing materialized here advertises as empty"
        );

        let repo = git2::Repository::init(base.path().join("demo")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let blob = repo.blob(b"one").unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("a.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let head = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "one", &tree, &[])
            .unwrap();
        repo.reference("refs/heads/feature/x", head, true, "test")
            .unwrap();

        let refs = on_disk_refs(base.path(), "demo").unwrap();

        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["feature/x", "main"], "every born branch, sorted");
        for r in &refs {
            assert_eq!(r.head, head.to_string(), "at its on-disk oid");
        }
    }

    /// two commits at the origin; a client that has the first must get a pack
    /// that (a) is smaller than the full closure and (b) still completes the
    /// second commit when installed next to the objects it already holds.
    #[test]
    fn haves_bound_the_pack_to_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let origin = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();

        let blob_a = origin.blob(b"one").unwrap();
        let mut tb = origin.treebuilder(None).unwrap();
        tb.insert("a.txt", blob_a, 0o100644).unwrap();
        let tree1 = origin.find_tree(tb.write().unwrap()).unwrap();
        let first = origin
            .commit(Some("refs/heads/dev"), &sig, &sig, "one", &tree1, &[])
            .unwrap();
        // keep `first` an advertised tip (a second branch) after `dev` moves
        // to `second` below — the want-guard only packs an advertised tip.
        origin
            .reference("refs/heads/base", first, true, "test")
            .unwrap();

        let blob_b = origin.blob(b"two").unwrap();
        let mut tb = origin.treebuilder(Some(&tree1)).unwrap();
        tb.insert("b.txt", blob_b, 0o100644).unwrap();
        let tree2 = origin.find_tree(tb.write().unwrap()).unwrap();
        let first_commit = origin.find_commit(first).unwrap();
        let second = origin
            .commit(
                Some("refs/heads/dev"),
                &sig,
                &sig,
                "two",
                &tree2,
                &[&first_commit],
            )
            .unwrap();

        let want = vec![second.to_string()];
        let (full, ack) = build_upload_pack(dir.path(), &want, &[]).unwrap();
        assert_eq!(ack, None, "no haves -> full closure after NAK");
        let (delta, ack) = build_upload_pack(dir.path(), &want, &[first.to_string()]).unwrap();
        assert_eq!(ack, Some(first.to_string()), "the common base is ACKed");
        assert!(
            delta.len() < full.len(),
            "delta pack ({}) must be smaller than the closure ({})",
            delta.len(),
            full.len()
        );

        // an unknown have cannot bound the walk — the answer stays full.
        let (fallback, ack) = build_upload_pack(dir.path(), &want, &[HAVE.to_string()]).unwrap();
        assert_eq!(ack, None);
        assert_eq!(fallback.len(), full.len());

        // install first's closure, then the delta, into a fresh repo: the
        // second commit and BOTH blobs must be readable — the delta carried
        // everything the client didn't already hold.
        let clone_dir = tempfile::tempdir().unwrap();
        let clone = git2::Repository::init_bare(clone_dir.path()).unwrap();
        let (base_pack, _) = build_upload_pack(dir.path(), &[first.to_string()], &[]).unwrap();
        for pack in [&base_pack, &delta] {
            let odb = clone.odb().unwrap();
            let mut pw = odb.packwriter().unwrap();
            std::io::Write::write_all(&mut pw, pack).unwrap();
            pw.commit().unwrap();
        }
        let landed = clone.find_commit(second).unwrap();
        assert_eq!(landed.tree().unwrap().len(), 2);
        assert!(clone.find_blob(blob_a).is_ok());
        assert!(clone.find_blob(blob_b).is_ok());
    }

    #[test]
    fn malformed_negotiation_tail_is_rejected() {
        let err = parse_upload_pack_request(&request_tail(b"0009wat!\n"))
            .err()
            .expect("unknown negotiation line must fail");

        assert!(err.contains("unexpected upload-pack negotiation line"));
    }

    /// a body entirely of minimal 5-byte pkt-lines (`0005A`, one byte of
    /// payload) is refused once it passes `MAX_GIT_PKT_LINES`, before it can
    /// force the ~1 GB of small allocations an unbounded parse would make.
    #[test]
    fn a_body_of_minimal_pkt_lines_past_the_cap_is_refused() {
        let one_line = b"0005A".to_vec();
        let under_cap: Vec<u8> = one_line.repeat(MAX_GIT_PKT_LINES);
        let over_cap: Vec<u8> = one_line.repeat(MAX_GIT_PKT_LINES + 1);

        // exactly at the cap: parse_pkt_lines runs out of buffer looking for
        // the terminating flush, which is its own (unrelated) truncation
        // error — the point here is it is NOT the "too many" error.
        let under = parse_pkt_lines(&under_cap).unwrap_err();
        assert!(!under.contains("too many"), "{under}");

        let over = parse_pkt_lines(&over_cap).unwrap_err();
        assert!(over.contains("too many pkt-lines in request"), "{over}");
    }

    /// a want naming an oid still in the ODB but no longer any branch's tip
    /// (the branch moved past it, or was force-pushed away) is refused, not
    /// packed — the anti-amplifier guard `build_upload_pack` shares with the
    /// peer lane's `forge::build_objects`. the current tip still packs fine.
    #[test]
    fn a_want_off_every_advertised_tip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();

        let blob = repo.blob(b"one").unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("a.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let orphaned = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "one", &tree, &[])
            .unwrap();
        let orphaned_commit = repo.find_commit(orphaned).unwrap();
        let tip = repo
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "two",
                &tree,
                &[&orphaned_commit],
            )
            .unwrap();

        let err = build_upload_pack(dir.path(), &[orphaned.to_string()], &[]).unwrap_err();
        assert!(
            matches!(err, UploadPackError::WantNotAdvertised(hex) if hex == orphaned.to_string())
        );

        build_upload_pack(dir.path(), &[tip.to_string()], &[])
            .expect("a want for the current tip still packs");
    }

    /// an absent repo dir maps to the path-bearing git2 error variant, not
    /// the generic one — the handler turns this into a fixed 404 and never
    /// surfaces the git2 message (which names the node's absolute path).
    #[test]
    fn an_absent_repo_dir_maps_to_the_path_bearing_error_variant() {
        let base = tempfile::tempdir().unwrap();
        let missing = base.path().join("no-such-repo");

        let err = build_upload_pack(&missing, &[WANT.to_string()], &[]).unwrap_err();

        assert!(matches!(err, UploadPackError::RepoUnavailable(_)));
    }
}
