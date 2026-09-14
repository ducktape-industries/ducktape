//! The `duck://` URI protocol (v1) — one grammar, one module table, one
//! place to add a module.
//!
//! ```text
//! duck-uri  = "duck://" authority path [ "@" rev ] [ "?net=" digest ] [ "#" fragment ]
//! authority = module | gateway-host        ; a dot means gateway plane
//! digest    = 8 lowercase hex              ; the chain id's hash half
//! ```
//!
//! [`classify_duck_link`] is the module table: every surface that opens or
//! embeds a link (the Markdown reader or app navigation handler)
//! classifies through it and nowhere else. A malformed
//! or unknown ref is [`DuckKind::Unknown`] — never an error here; the caller
//! decides what "nothing to open" looks like.
//!
//! THE LINK NAMES ITS NETWORK IN THE QUERY. A chain id is `<name>#<8 hex>`
//! (`workspace_config::identity::mint_chain_id`) and that literal `#` cannot
//! ride a URI, so the link carries the hex half alone. The authority stays
//! the module label. Every link the app PRODUCES carries `?net=`; one that
//! does not (hand-typed) resolves against the connected network as written.
//! [`resolve_duck_link`] is the open plane's entry — it adds the scope check
//! `classify_duck_link` cannot make on its own.

pub(crate) use crate::DuckKind;
// THE `?net=` FORMAT IS SPELLED ONCE, in the crate that also tokenizes a
// `duck://` run out of prose — the app and the in-consensus producer
// (`runs::inject`) are both readers of that one definition.
use ::chat::client::{chain_digest, duck_net_query as net_query, is_chain_digest};

/// One classified link. Only the fields its `kind` names are meaningful;
/// the rest are empty / zero.
///
/// The module table: `page/<id>[#<block>]`, `files/<path>`, `forge/<repo>`,
/// `forge/<repo>/<n>[#<seq>]`, `forge/<repo>/blob/<path>[@<oid>]`,
/// `channel/<id>[#<seq>]`, `run/<dispatch_id>`, `account/<n>`.
#[derive(Clone, Debug, PartialEq)]
pub struct DuckLink {
    pub kind: DuckKind,
    /// `forge_*`: the repository name.
    pub repo: String,
    /// `forge_item`: the item number (≥ 1).
    pub number: i64,
    /// `channel_message`: the message seq (≥ 1); `forge_item`: the Discussion
    /// message seq, or 0.
    pub seq: i64,
    /// `page`: the page id.
    pub page: String,
    /// `page`: the block the link lands on, or "" for the page's top.
    pub block: String,
    /// `run`: the run's dispatch id.
    pub dispatch: String,
    /// `channel` / `channel_message`: the channel id.
    pub channel: String,
    /// `files`: the absolute duckfs path; `forge_blob`: the repo-relative path.
    pub path: String,
    /// `forge_blob`: the `@rev`, or "" for the head.
    pub rev: String,
    /// `account`: the account number, as the DM peer list keys it.
    pub account: String,
    /// The `?net=` digest — the hex half of the chain id this link belongs
    /// to — or "" when the link names no network. `foreign_network` carries
    /// the digest that did NOT match.
    pub net: String,
}

impl DuckLink {
    fn unknown() -> Self {
        Self {
            kind: DuckKind::Unknown,
            repo: String::new(),
            number: 0,
            seq: 0,
            page: String::new(),
            block: String::new(),
            dispatch: String::new(),
            channel: String::new(),
            path: String::new(),
            rev: String::new(),
            account: String::new(),
            net: String::new(),
        }
    }

    fn of(kind: DuckKind) -> Self {
        Self {
            kind,
            ..Self::unknown()
        }
    }
}

/// Classify one link. Web links (`http(s)://`) are [`DuckKind::Web`];
/// everything that is not a well-formed module-plane duck URI is
/// [`DuckKind::Unknown`].
pub fn classify_duck_link(url: String) -> DuckLink {
    let web = url.starts_with("http://") || url.starts_with("https://");
    if web {
        return DuckLink::of(DuckKind::Web);
    }
    let Some(rest) = url.strip_prefix("duck://") else {
        return DuckLink::unknown();
    };
    let (authority, tail) = match rest.split_once('/') {
        Some((authority, tail)) => (authority, format!("/{tail}")),
        None => (rest, String::new()),
    };
    let gateway_plane = authority.contains('.');
    if gateway_plane || authority.is_empty() {
        return DuckLink::unknown();
    }
    let (body, fragment) = tail.split_once('#').unwrap_or((tail.as_str(), ""));
    let (address, query) = body.split_once('?').unwrap_or((body, ""));
    let Some(net) = query_net(query) else {
        return DuckLink::unknown();
    };
    let (path, rev) = address.split_once('@').unwrap_or((address, ""));
    let Some(segments) = clean_segments(path) else {
        return DuckLink::unknown();
    };
    let link = match authority {
        "page" => classify_page(&segments, rev, fragment),
        "files" => classify_files(path, &segments, rev, fragment),
        "forge" => classify_forge(&segments, rev, fragment),
        "channel" => classify_channel(&segments, rev, fragment),
        "run" => classify_run(&segments, rev, fragment),
        "account" => classify_account(&segments, rev, fragment),
        _ => DuckLink::unknown(),
    };
    // An `Unknown` addresses nothing, so it belongs to no network either.
    let names_nothing = link.kind == DuckKind::Unknown;
    match names_nothing {
        true => link,
        false => DuckLink { net, ..link },
    }
}

/// The `?net=` digest a link's query names: `""` for no query at all, the
/// digest for exactly `net=<8 lowercase hex>`, and `None` for every other
/// query — an unreadable query is a malformed link, not one to guess at.
fn query_net(query: &str) -> Option<String> {
    if query.is_empty() {
        return Some(String::new());
    }
    let digest = query.strip_prefix("net=")?;
    is_chain_digest(digest).then(|| digest.to_owned())
}

/// The open plane's entry: the grammar, plus the one check the grammar cannot
/// make on its own. A link that names a network OTHER than the connected one
/// would resolve its repo name / page id / channel id against a store that is
/// not the link's own, so it opens nothing — the caller draws the refusal
/// [`foreign_network_error`] spells. A link naming no network is the
/// hand-typed case and resolves against the connected network as written.
pub fn resolve_duck_link(url: String, connected_chain_id: String) -> DuckLink {
    let link = classify_duck_link(url);
    let ours = link.net.is_empty() || link.net == chain_digest(&connected_chain_id);
    match ours {
        true => link,
        false => DuckLink {
            kind: DuckKind::ForeignNetwork,
            ..link
        },
    }
}

/// The refusal for a link that belongs to another network: both networks by
/// name, because "this link does not open" without them is unactionable.
pub fn foreign_network_error(link_net: String, connected_chain_id: String) -> String {
    let here = match connected_chain_id.is_empty() {
        true => "no network".to_owned(),
        false => connected_chain_id,
    };
    format!("this link belongs to network {link_net} — this app is on {here}")
}

/// `duck://page/<id>?net=…` — the only handle on a page, whose id is a uuid.
pub fn duck_page_link(page: String, chain_id: String) -> String {
    format!("duck://page/{page}{}", net_query(&chain_id))
}

/// `duck://run/<dispatch_id>?net=…` — one agent run, by the dispatch id that
/// addresses it everywhere outside the runs module.
pub fn duck_run_link(dispatch_id: String, chain_id: String) -> String {
    format!("duck://run/{dispatch_id}{}", net_query(&chain_id))
}

/// `duck://forge/<repo>/<number>?net=…` — one tracker item, issue or PR.
pub fn duck_forge_item_link(repo: String, number: i64, chain_id: String) -> String {
    format!("duck://forge/{repo}/{number}{}", net_query(&chain_id))
}

/// `duck://forge/<repo>?net=…` — one repository.
pub fn duck_forge_repo_link(repo: String, chain_id: String) -> String {
    format!("duck://forge/{repo}{}", net_query(&chain_id))
}

/// `duck://channel/<id>?net=…#<seq>` — one message. The query precedes the
/// fragment, as in every other URI.
pub fn duck_channel_message_link(channel: String, seq: i64, chain_id: String) -> String {
    format!("duck://channel/{channel}{}#{seq}", net_query(&chain_id))
}

/// The `duck://` URL the OS launched this process with, or "" for a plain
/// start. `xdg-open 'duck://forge/ducktape/1?net=…'` runs the `Exec=` line of
/// the desktop entry that claims `x-scheme-handler/duck`
/// (`app/packaging/dev.ducktape.app.desktop`), which passes the URL as `%u`.
///
/// Read once into state and PARKED, never opened here: the link addresses
/// objects in a network this process has not connected to yet, and the open
/// plane must know the connected chain id before it can tell an address of
/// its own from one of somebody else's.
pub fn startup_duck_url() -> String {
    std::env::args()
        .skip(1)
        .find(|argument| argument.starts_with("duck://"))
        .unwrap_or_default()
}

/// The path's segments, or `None` when any is empty (`//`), `.` or `..`.
/// A bare `/` or empty path is an empty list.
fn clean_segments(path: &str) -> Option<Vec<&str>> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    let clean = segments
        .iter()
        .all(|segment| !segment.is_empty() && *segment != "." && *segment != "..");
    clean.then_some(segments)
}

/// A 1-based decimal, or `None` (empty, zero, signs, anything else).
fn positive(digits: &str) -> Option<i64> {
    let decimal = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
    if !decimal {
        return None;
    }
    digits.parse::<i64>().ok().filter(|number| *number > 0)
}

/// `/page/<id>[#<block>]`: a page is not versioned, so `@rev` is refused; the
/// fragment, when present, is the block the link lands on.
fn classify_page(segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    let [id] = segments else {
        return DuckLink::unknown();
    };
    if !rev.is_empty() {
        return DuckLink::unknown();
    }
    DuckLink {
        page: (*id).to_owned(),
        block: fragment.to_owned(),
        ..DuckLink::of(DuckKind::Page)
    }
}

/// `/run/<dispatch_id>`: a dispatch id is the run id's hex sha256 — exactly 64
/// lowercase hex — and a run is neither versioned nor anchored.
fn classify_run(segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    let [dispatch] = segments else {
        return DuckLink::unknown();
    };
    let plain = rev.is_empty() && fragment.is_empty();
    let is_dispatch_id = dispatch.len() == 64
        && dispatch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let named = plain && is_dispatch_id;
    if !named {
        return DuckLink::unknown();
    }
    DuckLink {
        dispatch: (*dispatch).to_owned(),
        ..DuckLink::of(DuckKind::Run)
    }
}

/// `/account/<n>`: an identity account by number — what a chat mention
/// links to (`chat::client::duck_account_link`). Account numbers start at 1,
/// and an account is neither versioned nor anchored.
fn classify_account(segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    let [account] = segments else {
        return DuckLink::unknown();
    };
    let plain = rev.is_empty() && fragment.is_empty();
    let numbered = positive(account).is_some();
    let named = plain && numbered;
    if !named {
        return DuckLink::unknown();
    }
    DuckLink {
        account: (*account).to_owned(),
        ..DuckLink::of(DuckKind::Account)
    }
}

/// Confined to exactly `/shared/attachments/<dir>/<name>` — classify is the
/// only guard between a crafted ref and a client read at another path.
fn classify_files(path: &str, segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    let ["shared", "attachments", _dir, _name] = segments else {
        return DuckLink::unknown();
    };
    let plain = rev.is_empty() && fragment.is_empty();
    if !plain {
        return DuckLink::unknown();
    }
    DuckLink {
        path: path.to_owned(),
        ..DuckLink::of(DuckKind::Files)
    }
}

fn classify_forge(segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    match segments {
        [repo] => {
            let plain = rev.is_empty() && fragment.is_empty();
            if !plain {
                return DuckLink::unknown();
            }
            DuckLink {
                repo: (*repo).to_owned(),
                ..DuckLink::of(DuckKind::ForgeRepo)
            }
        }
        [repo, "blob", file @ ..] => {
            // The node browses a pinned revision by exact oid only (a branch
            // name is not an address — it moves), so the protocol says the
            // same: `@rev` is 40 lowercase hex or absent (the head).
            let oid = rev.is_empty()
                || (rev.len() == 40
                    && rev
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            let named = !file.is_empty() && fragment.is_empty() && oid;
            if !named {
                return DuckLink::unknown();
            }
            DuckLink {
                repo: (*repo).to_owned(),
                path: file.join("/"),
                rev: rev.to_owned(),
                ..DuckLink::of(DuckKind::ForgeBlob)
            }
        }
        [repo, number] => {
            let Some(number) = positive(number) else {
                return DuckLink::unknown();
            };
            let seq = match fragment.is_empty() {
                true => Some(0),
                false => positive(fragment),
            };
            let Some(seq) = seq.filter(|_| rev.is_empty()) else {
                return DuckLink::unknown();
            };
            DuckLink {
                repo: (*repo).to_owned(),
                number,
                seq,
                ..DuckLink::of(DuckKind::ForgeItem)
            }
        }
        _ => DuckLink::unknown(),
    }
}

fn classify_channel(segments: &[&str], rev: &str, fragment: &str) -> DuckLink {
    let [id] = segments else {
        return DuckLink::unknown();
    };
    if !rev.is_empty() {
        return DuckLink::unknown();
    }
    let channel = (*id).to_owned();
    if fragment.is_empty() {
        return DuckLink {
            channel,
            ..DuckLink::of(DuckKind::Channel)
        };
    }
    let Some(seq) = positive(fragment) else {
        return DuckLink::unknown();
    };
    DuckLink {
        channel,
        seq,
        ..DuckLink::of(DuckKind::ChannelMessage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(url: &str) -> DuckKind {
        classify_duck_link(url.into()).kind
    }

    /// The module table, row by row — the cases the protocol plan pinned for
    /// the TS `classifyDuckRef`, carried to the Rust table verbatim.
    #[test]
    fn the_module_table_classifies_every_row_and_refuses_the_rest() {
        let page = classify_duck_link("duck://page/pg-1".into());
        assert_eq!(
            (page.kind, page.page.as_str(), page.block.as_str()),
            (DuckKind::Page, "pg-1", "")
        );
        let block = classify_duck_link("duck://page/pg-1#blk-7".into());
        assert_eq!(
            (block.kind, block.page.as_str(), block.block.as_str()),
            (DuckKind::Page, "pg-1", "blk-7")
        );
        assert_eq!(kind("duck://page/pg-1@v2"), DuckKind::Unknown);
        assert_eq!(kind("duck://page/a/b"), DuckKind::Unknown);
        assert_eq!(kind("duck://page/"), DuckKind::Unknown);

        let dispatch = "ab".repeat(32);
        let run = classify_duck_link(format!("duck://run/{dispatch}"));
        assert_eq!(
            (run.kind, run.dispatch.as_str()),
            (DuckKind::Run, dispatch.as_str())
        );
        assert_eq!(kind("duck://run/agent/abc"), DuckKind::Unknown);
        assert_eq!(kind("duck://run/abc"), DuckKind::Unknown, "not a digest");
        assert_eq!(
            kind(&format!("duck://run/{}", "AB".repeat(32))),
            DuckKind::Unknown,
            "lowercase only"
        );
        assert_eq!(kind(&format!("duck://run/{dispatch}#1")), DuckKind::Unknown);
        assert_eq!(kind("duck://run/"), DuckKind::Unknown);

        let file = classify_duck_link("duck://files/shared/attachments/u1/doc.pdf".into());
        assert_eq!(
            (file.kind, file.path.as_str()),
            (DuckKind::Files, "/shared/attachments/u1/doc.pdf")
        );
        assert_eq!(kind("duck://files/shared/skills/x.md"), DuckKind::Unknown);
        assert_eq!(
            kind("duck://files/shared/attachments/a/b/c"),
            DuckKind::Unknown
        );
        assert_eq!(
            kind("duck://files/shared/attachments/../etc/pw"),
            DuckKind::Unknown
        );
        assert_eq!(
            kind("duck://files/shared/attachments/u1/a.png"),
            DuckKind::Files
        );

        let repo = classify_duck_link("duck://forge/ducktape".into());
        assert_eq!(
            (repo.kind, repo.repo.as_str()),
            (DuckKind::ForgeRepo, "ducktape")
        );
        let item = classify_duck_link("duck://forge/ducktape/58".into());
        assert_eq!(
            (item.kind, item.number, item.seq),
            (DuckKind::ForgeItem, 58, 0)
        );
        let anchored = classify_duck_link("duck://forge/ducktape/58#12".into());
        assert_eq!(
            (anchored.kind, anchored.number, anchored.seq),
            (DuckKind::ForgeItem, 58, 12)
        );
        assert_eq!(kind("duck://forge/ducktape#12"), DuckKind::Unknown);
        assert_eq!(kind("duck://forge/ducktape/0"), DuckKind::Unknown);
        assert_eq!(kind("duck://forge/ducktape/58#0"), DuckKind::Unknown);
        assert_eq!(kind("duck://forge/ducktape/-1"), DuckKind::Unknown);

        let channel = classify_duck_link("duck://channel/general".into());
        assert_eq!(
            (channel.kind, channel.channel.as_str()),
            (DuckKind::Channel, "general")
        );
        let hidden = classify_duck_link("duck://channel/forge:ducktape:58".into());
        assert_eq!(
            (hidden.kind, hidden.channel.as_str()),
            (DuckKind::Channel, "forge:ducktape:58")
        );
        let message = classify_duck_link("duck://channel/general#42".into());
        assert_eq!((message.kind, message.seq), (DuckKind::ChannelMessage, 42));
        assert_eq!(kind("duck://channel/general#0"), DuckKind::Unknown);
        assert_eq!(kind("duck://channel/"), DuckKind::Unknown);

        let account = classify_duck_link(::chat::client::duck_account_link(7));
        assert_eq!(
            (account.kind, account.account.as_str()),
            (DuckKind::Account, "7")
        );
        assert_eq!(kind("duck://account/0"), DuckKind::Unknown);
        assert_eq!(kind("duck://account/abc"), DuckKind::Unknown);
        assert_eq!(kind("duck://account/7/keys"), DuckKind::Unknown);
        assert_eq!(kind("duck://account/7#1"), DuckKind::Unknown);
        assert_eq!(kind("duck://account/7@v2"), DuckKind::Unknown);

        assert_eq!(
            kind("duck://memory/notes/a.md"),
            DuckKind::Unknown,
            "reserved"
        );
        assert_eq!(
            kind("duck://team.duck/index.html"),
            DuckKind::Unknown,
            "gateway plane"
        );
        assert_eq!(kind("duck://net.duck"), DuckKind::Unknown, "gateway plane");
        assert_eq!(kind("duck://"), DuckKind::Unknown);
        assert_eq!(kind("mailto:a@b"), DuckKind::Unknown);
        assert_eq!(
            kind("./img/a.png"),
            DuckKind::Unknown,
            "a relative path is the caller's to resolve"
        );
        assert_eq!(kind("https://example.com/a.png"), DuckKind::Web);
        assert_eq!(kind("http://example.com"), DuckKind::Web);
    }

    /// The forge blob row: `/<repo>/blob/<path>[@<rev>]`.
    #[test]
    fn a_forge_blob_names_a_committed_file_at_a_revision_or_the_head() {
        let head = classify_duck_link("duck://forge/ducktape/blob/docs/logo.png".into());
        assert_eq!(
            (
                head.kind,
                head.repo.as_str(),
                head.path.as_str(),
                head.rev.as_str()
            ),
            (DuckKind::ForgeBlob, "ducktape", "docs/logo.png", "")
        );
        assert_eq!(
            kind("duck://forge/ducktape/blob/README.md@main"),
            DuckKind::Unknown,
            "a branch name moves — a rev is an exact oid"
        );
        assert_eq!(
            kind("duck://forge/ducktape/blob/a.png@ABCDEF0000000000000000000000000000000000"),
            DuckKind::Unknown,
            "lowercase hex"
        );
        let oid = classify_duck_link(
            "duck://forge/ducktape/blob/a/b.png@1111111111111111111111111111111111111111".into(),
        );
        assert_eq!(oid.path, "a/b.png");
        assert_eq!(oid.rev.len(), 40);
        assert_eq!(
            kind("duck://forge/ducktape/blob"),
            DuckKind::Unknown,
            "no file"
        );
        assert_eq!(
            kind("duck://forge/ducktape/blob/"),
            DuckKind::Unknown,
            "no file"
        );
        assert_eq!(
            kind("duck://forge/ducktape/blob/../x"),
            DuckKind::Unknown,
            "no dot-segments"
        );
        assert_eq!(
            kind("duck://forge/ducktape/blob/a.png#L3"),
            DuckKind::Unknown,
            "no fragment yet"
        );
    }

    /// The `?net=` component: parsed off every row, refused when malformed,
    /// and carried by every link the app builds.
    #[test]
    fn a_link_names_its_network_in_the_query() {
        let item = classify_duck_link("duck://forge/ducktape/58?net=d0cdf950".into());
        assert_eq!(
            (item.kind, item.number, item.net.as_str()),
            (DuckKind::ForgeItem, 58, "d0cdf950")
        );
        let anchored = classify_duck_link("duck://channel/general?net=d0cdf950#42".into());
        assert_eq!(
            (anchored.kind, anchored.seq, anchored.net.as_str()),
            (DuckKind::ChannelMessage, 42, "d0cdf950"),
            "query precedes fragment"
        );
        let blob = classify_duck_link(
            "duck://forge/d/blob/a.png@1111111111111111111111111111111111111111?net=d0cdf950"
                .into(),
        );
        assert_eq!(
            (blob.kind, blob.net.as_str()),
            (DuckKind::ForgeBlob, "d0cdf950")
        );
        assert_eq!(
            kind("duck://page/p1?net=D0CDF950"),
            DuckKind::Unknown,
            "lowercase hex"
        );
        assert_eq!(
            kind("duck://page/p1?net=d0cdf9"),
            DuckKind::Unknown,
            "eight hex"
        );
        assert_eq!(kind("duck://page/p1?net="), DuckKind::Unknown);
        assert_eq!(
            kind("duck://page/p1?chain=d0cdf950"),
            DuckKind::Unknown,
            "one key"
        );
        assert!(classify_duck_link("duck://page/p1".into()).net.is_empty());
        assert!(
            classify_duck_link("duck://nope/x?net=d0cdf950".into())
                .net
                .is_empty()
        );

        assert_eq!(
            duck_page_link("p1".into(), "mynet#d0cdf950".into()),
            "duck://page/p1?net=d0cdf950"
        );
        assert_eq!(
            duck_page_link("p1".into(), "my#net#d0cdf950".into()),
            "duck://page/p1?net=d0cdf950",
            "a name may carry a #; the minted separator is the last one"
        );
        assert_eq!(
            duck_channel_message_link("general".into(), 42, "mynet#d0cdf950".into()),
            "duck://channel/general?net=d0cdf950#42"
        );
        assert_eq!(
            duck_page_link("p1".into(), String::new()),
            "duck://page/p1",
            "no chain id yet, no query — never a `?net=` naming nothing"
        );
        for built in [
            duck_page_link("p1".into(), "mynet#d0cdf950".into()),
            "duck://channel/c1?net=d0cdf950".into(),
            duck_channel_message_link("c1".into(), 3, "mynet#d0cdf950".into()),
        ] {
            let link = resolve_duck_link(built.clone(), "mynet#d0cdf950".into());
            assert_ne!(link.kind, DuckKind::Unknown, "{built} must round-trip");
            assert_eq!(link.net, "d0cdf950", "{built}");
        }
    }

    /// The refusal: a link that names another network opens nothing, and says
    /// which two networks it is talking about.
    #[test]
    fn a_link_from_another_network_is_refused_not_resolved() {
        let here = "mynet#d0cdf950";
        let mine = resolve_duck_link("duck://forge/ducktape/58?net=d0cdf950".into(), here.into());
        assert_eq!((mine.kind, mine.number), (DuckKind::ForgeItem, 58));
        let theirs = resolve_duck_link("duck://forge/ducktape/58?net=aaaaaaaa".into(), here.into());
        assert_eq!(
            (theirs.kind, theirs.net.as_str()),
            (DuckKind::ForeignNetwork, "aaaaaaaa"),
            "the same repo name on another network is not this repo"
        );
        let typed = resolve_duck_link("duck://forge/ducktape/58".into(), here.into());
        assert_eq!(
            typed.kind,
            DuckKind::ForgeItem,
            "a hand-typed link stays usable"
        );
        let unjoined = resolve_duck_link("duck://page/p1?net=d0cdf950".into(), String::new());
        assert_eq!(
            unjoined.kind,
            DuckKind::ForeignNetwork,
            "no connected chain id is no store to resolve against"
        );
        assert_eq!(
            resolve_duck_link("https://example.com".into(), here.into()).kind,
            DuckKind::Web,
            "a web link belongs to no network"
        );

        let refusal = foreign_network_error("aaaaaaaa".into(), here.into());
        assert!(
            refusal.contains("aaaaaaaa") && refusal.contains(here),
            "{refusal}"
        );
        assert!(
            foreign_network_error("aaaaaaaa".into(), String::new()).contains("no network"),
            "an unconnected app still names where it is"
        );
    }
}
