use super::*;
use ::forge;
use std::net::IpAddr;

/// The local half of the client-computed merge.
pub enum MergeBuild {
    Clean { merge_oid: String, pack: Vec<u8> },
    Conflicts(Vec<String>),
}

/// Build the merge commit for `theirs` (source head) into `ours` (target
/// head) without touching the mirror: a throwaway bare repo whose odb reads
/// the mirror's objects through a disk alternate. Returns the new oid plus
/// the MINIMAL pack — only objects reachable from the merge but from
/// NEITHER parent.
///
/// This is the `git.merge` kernel door's whole body, and it is a host
/// capability rather than a module's reading: a git implementation plus a
/// second transport, neither of which a wasm view has. `module` names the
/// git-backed module whose smart-HTTP route the mirror is fetched from.
pub fn build_git_merge(
    endpoint: &str,
    module: &str,
    repo: &str,
    ours: &str,
    theirs: &str,
    message: &str,
) -> Result<MergeBuild, String> {
    let mirror = sync_git_mirror(endpoint, module, repo)?;
    let ours_oid = git2::Oid::from_str(ours).map_err(git_err)?;
    let theirs_oid = git2::Oid::from_str(theirs).map_err(git_err)?;
    merge_against_mirror(&mirror, ours_oid, theirs_oid, message)
}

/// The mirror-independent half: merge two commits readable from `mirror`'s
/// odb and pack what neither parent already carries.
pub(crate) fn merge_against_mirror(
    mirror: &git2::Repository,
    ours_oid: git2::Oid,
    theirs_oid: git2::Oid,
    message: &str,
) -> Result<MergeBuild, String> {
    let scratch = ScratchDir::create()?;
    let temp = git2::Repository::init_bare(scratch.path()).map_err(git_err)?;
    let objects = mirror.path().join("objects");
    let objects = objects
        .to_str()
        .ok_or_else(|| format!("non-utf8 objects path {}", objects.display()))?;
    temp.odb()
        .map_err(git_err)?
        .add_disk_alternate(objects)
        .map_err(git_err)?;

    let ours_commit = temp.find_commit(ours_oid).map_err(|_| {
        "the target head is not in the local mirror; the branch may have moved — reload the item"
            .to_string()
    })?;
    let theirs_commit = temp.find_commit(theirs_oid).map_err(|_| {
        "the source head is not in the local mirror; the branch may have moved — reload the item"
            .to_string()
    })?;
    let mut index = temp
        .merge_commits(&ours_commit, &theirs_commit, None)
        .map_err(git_err)?;
    if index.has_conflicts() {
        let mut conflicts = Vec::new();
        for conflict in index.conflicts().map_err(git_err)? {
            let conflict = conflict.map_err(git_err)?;
            let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) else {
                continue;
            };
            conflicts.push(String::from_utf8_lossy(&entry.path).into_owned());
        }
        conflicts.sort();
        conflicts.dedup();
        return Ok(MergeBuild::Conflicts(conflicts));
    }

    let tree_oid = index.write_tree_to(&temp).map_err(git_err)?;
    let tree = temp.find_tree(tree_oid).map_err(git_err)?;
    let signature = git2::Signature::now("ducktape", "ducktape@localhost").map_err(git_err)?;
    let merge_oid = temp
        .commit(
            None,
            &signature,
            &signature,
            message,
            &tree,
            &[&ours_commit, &theirs_commit],
        )
        .map_err(git_err)?;

    let mut builder = temp.packbuilder().map_err(git_err)?;
    let mut walk = temp.revwalk().map_err(git_err)?;
    walk.push(merge_oid).map_err(git_err)?;
    walk.hide(ours_oid).map_err(git_err)?;
    walk.hide(theirs_oid).map_err(git_err)?;
    builder.insert_walk(&mut walk).map_err(git_err)?;
    let mut buf = git2::Buf::new();
    builder.write_buf(&mut buf).map_err(git_err)?;

    Ok(MergeBuild::Clean {
        merge_oid: merge_oid.to_string(),
        pack: buf.to_vec(),
    })
}

/// Open (creating on first use) and refresh the bare mirror of one repo's
/// smart-HTTP remote. The mirror is a persistent per-endpoint cache under the
/// same root the user key lives in, so two networks' repos never shadow each
/// other.
static FORGE_MIRROR_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn forge_mirror_lock(dir: &Path) -> Result<Arc<Mutex<()>>, String> {
    let locks = FORGE_MIRROR_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| "forge mirror lock registry is poisoned".to_string())?;
    Ok(locks
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn sync_git_mirror(endpoint: &str, module: &str, repo: &str) -> Result<git2::Repository, String> {
    let dir = git_mirror_dir(endpoint, module, repo)?;
    let lock = forge_mirror_lock(&dir)?;
    let _guard = lock
        .lock()
        .map_err(|_| format!("git mirror lock is poisoned for {repo:?}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("create git mirror dir {}: {error}", dir.display()))?;
    let mirror = match git2::Repository::open_bare(&dir) {
        Ok(existing) => existing,
        Err(_) => git2::Repository::init_bare(&dir).map_err(git_err)?,
    };
    {
        let mut remote = mirror
            .remote_anonymous(&format!(
                "{}/{module}/{repo}",
                endpoint.trim_end_matches('/')
            ))
            .map_err(git_err)?;
        remote
            .fetch(&["+refs/heads/*:refs/heads/*"], None, None)
            .map_err(|error| format!("fetch git remote for {repo:?}: {error}"))?;
    }
    Ok(mirror)
}

/// `<app cache>/git-remote/<endpoint-slug>/<module>/<repo>` — a rebuildable
/// mirror, so it lives in the app's cache directory
/// ([`super::app_dirs::cache_dir`]), never under the ducktape home.
fn git_mirror_dir(endpoint: &str, module: &str, repo: &str) -> Result<PathBuf, String> {
    let named = |segment: &str| {
        !segment.is_empty()
            && !segment.contains('/')
            && !segment.contains('\\')
            && !segment.starts_with('.')
    };
    if !named(repo) || !named(module) {
        return Err(format!("invalid git repo name {module:?}/{repo:?}"));
    }
    let root = super::app_dirs::cache_dir()?;
    let slug: String = endpoint
        .chars()
        .map(|character| match character.is_ascii_alphanumeric() {
            true => character,
            false => '-',
        })
        .collect();
    Ok(root.join("git-remote").join(slug).join(module).join(repo))
}

fn git_err(error: git2::Error) -> String {
    error.message().to_string()
}

/// Process-unique throwaway directory under the OS temp dir, removed
/// (best-effort) on drop — the merge scratch is one bare repo per click.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ducktape-forge-merge-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("create merge scratch dir {}: {error}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Page one blob's bytes in through `blob_bytes` (1 MiB pages to eof).
/// Page 1 asks by the caller's rev (a branch name or an oid); every later
/// page asks by the exact oid page 1 answered, so a branch that moves
/// mid-read cannot hand back pages of two different commits — and that exact
/// oid is returned. `None` bytes: the object is past the byte cap — by the
/// size the node announces (it refuses an object past its own paged cap with
/// an empty final page, and the two caps are one number) or by what arrived.
/// An empty blob is `Some(empty)`, not a refusal.
async fn forge_blob_bytes(
    client: &RpcClient,
    repo: &str,
    rev: &str,
    path: &str,
) -> Result<(String, Option<Vec<u8>>), String> {
    use super::picture::MAX_PICTURE_BYTES;
    let mut bytes = Vec::new();
    let mut rev = rev.to_owned();
    loop {
        let query = serde_json::json!({ "blob_bytes": {
            "repo": repo,
            "rev": &rev,
            "path": path,
            "offset": bytes.len() as u64,
            "len": forge::MAX_BLOB_PAGE_BYTES as u64,
        }});
        let reply: serde_json::Value = client.query("forge", &query).await?;
        let page = reply
            .get("blob_bytes")
            .cloned()
            .ok_or_else(|| "the requested file was not found".to_string())?;
        let page: forge::BlobBytesReply =
            serde_json::from_value(page).map_err(|error| error.to_string())?;
        rev = page.rev;
        let chunk = super::storage::base64_decode(&page.b64)
            .ok_or_else(|| "the node's blob page is not valid base64".to_string())?;
        bytes.extend_from_slice(&chunk);
        let announced_past_cap = page.size > MAX_PICTURE_BYTES as i64;
        let past_cap = announced_past_cap || bytes.len() > MAX_PICTURE_BYTES;
        if past_cap {
            return Ok((rev, None));
        }
        let done = page.eof || chunk.is_empty();
        if done {
            return Ok((rev, Some(bytes)));
        }
    }
}

/// Fetch the pictures a Markdown blob embeds and park them under the
/// document, keyed by the image URL as written, for `forge_markdown`'s
/// viewer. Best effort, in document order, the first `MAX_INLINE_PICTURES`:
/// an image that does not resolve, fetch or decode simply keeps its alt text.
/// ponytail: fetched before the text lands, so a README with eight large
/// pictures shows late; split into its own lane if that is ever felt.
pub async fn load_inline_pictures(
    client: &RpcClient,
    doc: String,
    source: &str,
    base: String,
    net: String,
) {
    use super::picture::{MAX_INLINE_PICTURES, decode_off_thread, park_inline_pictures};
    let anchor = super::duck_uri::resolve_duck_link(base, net.clone());
    let mut wanted: Vec<String> = Vec::new();
    for item in pulldown_cmark::Parser::new_ext(source, pulldown_cmark::Options::all()) {
        let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) = item else {
            continue;
        };
        let url = dest_url.into_string();
        let seen = wanted.contains(&url);
        if !seen {
            wanted.push(url);
        }
    }
    wanted.truncate(MAX_INLINE_PICTURES);
    // Side by side, not one after another: a web picture answers on a remote
    // host's clock, and eight of them in a row would stack eight timeouts in
    // front of the README.
    let fetches = wanted.into_iter().map(|url| {
        let anchor = &anchor;
        let net = net.clone();
        async move {
            let bytes = inline_picture_bytes(client, anchor, &url, &net).await?;
            let picture = decode_off_thread(bytes).await.ok()?;
            Some((url, picture))
        }
    });
    let pictures = futures::future::join_all(fetches)
        .await
        .into_iter()
        .flatten()
        .collect();
    park_inline_pictures(doc, pictures);
}

/// Where an image URL's bytes live, by the duck:// module table: a
/// `duck://forge/<repo>/blob/<path>[@rev]` is that repo's committed file, a
/// `duck://files/...` is the attachment in duckfs, a bare relative path is
/// this repo's file beside the document at the document's own commit, and a
/// web URL is one capped GET. Every other kind — a page or channel ref, a
/// malformed duck URI — has no bytes to fetch.
///
/// Scoped by `net`, the connected chain id, exactly as the open plane is: a
/// citation whose `?net=` names another network addresses another store, so
/// it draws nothing rather than this network's object of the same name.
async fn inline_picture_bytes(
    client: &RpcClient,
    anchor: &super::duck_uri::DuckLink,
    url: &str,
    net: &str,
) -> Option<Vec<u8>> {
    use super::duck_uri::{DuckKind, resolve_duck_link};
    use super::picture::resolve_repo_path;
    let link = resolve_duck_link(url.to_owned(), net.to_owned());
    match link.kind {
        DuckKind::ForgeBlob => forge_blob_bytes(client, &link.repo, &link.rev, &link.path)
            .await
            .ok()
            .and_then(|(_, bytes)| bytes),
        DuckKind::Files => super::storage::files_read_all(client, &link.path)
            .await
            .ok()
            .flatten(),
        DuckKind::Unknown => {
            let path = resolve_repo_path(&anchor.path, url)?;
            forge_blob_bytes(client, &anchor.repo, &anchor.rev, &path)
                .await
                .ok()
                .and_then(|(_, bytes)| bytes)
        }
        DuckKind::Web => web_picture_bytes(url).await,
        // A citation from another network, and the kinds that name no bytes.
        DuckKind::ForeignNetwork
        | DuckKind::Page
        | DuckKind::ForgeRepo
        | DuckKind::ForgeItem
        | DuckKind::Channel
        | DuckKind::ChannelMessage
        | DuckKind::Run
        | DuckKind::Account => None,
    }
}

/// How long a web picture may take, end to end. Shorter than the RPC
/// client's 30 s on purpose: the README's text waits on this, and a remote
/// host is nobody's to trust.
const WEB_PICTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// One GET for a web picture, bounded by `MAX_PICTURE_BYTES` before the body
/// is read (the announced length) and while it streams (an unannounced or
/// lying one), and by `WEB_PICTURE_TIMEOUT`. `None` for any refusal — the
/// image keeps its alt text, never an error. A README names the URL and the
/// reader's machine makes the request, so the host is gated first
/// ([`blocked_picture_host`]): the reader's own machine, its link, and
/// nowhere/everywhere are not a picture's address, on the first hop or any
/// redirect.
pub async fn web_picture_bytes(url: &str) -> Option<Vec<u8>> {
    let url = reqwest::Url::parse(url).ok()?;
    let allowed = picture_host_allowed(&url).await;
    if !allowed {
        return None;
    }
    fetch_picture_bytes(url).await
}

/// The GET itself, after the host gate. The app's one HTTP client for the
/// open web, built once; the node's RPC client stays the node's.
/// ponytail: no cache across documents and no per-host limit — a README
/// re-fetches its pictures on every open; add a byte-keyed cache when felt.
pub(crate) async fn fetch_picture_bytes(url: reqwest::Url) -> Option<Vec<u8>> {
    use super::picture::MAX_PICTURE_BYTES;
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            // A redirect is re-gated by what its URL spells: an IP literal
            // or `localhost`. ponytail: a hop to a NAME that resolves to a
            // blocked address is not re-resolved here (the policy is sync);
            // resolve hops too if that is ever the concern.
            let policy = reqwest::redirect::Policy::custom(|attempt| {
                let literal_blocked = host_literal(attempt.url()).is_some_and(blocked_picture_host);
                let name_is_localhost = attempt
                    .url()
                    .domain()
                    .is_some_and(|name| name.eq_ignore_ascii_case("localhost"));
                let hop_blocked = literal_blocked || name_is_localhost;
                match hop_blocked {
                    true => attempt.stop(),
                    false => attempt.follow(),
                }
            });
            reqwest::Client::builder()
                .timeout(WEB_PICTURE_TIMEOUT)
                .redirect(policy)
                .build()
                .ok()
        })
        .as_ref()?;
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let announced_past_cap = response
        .content_length()
        .is_some_and(|length| length > MAX_PICTURE_BYTES as u64);
    if announced_past_cap {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        let streamed_past_cap = bytes.len() + chunk.len() > MAX_PICTURE_BYTES;
        if streamed_past_cap {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

/// Whether a web picture may be asked of `url`'s host: an IP literal is
/// judged as written, a name by every address it resolves to — one blocked
/// address refuses the name (`localhost` and a metadata alias both land
/// here). A name that does not resolve is refused too: there is nothing to
/// fetch from.
async fn picture_host_allowed(url: &reqwest::Url) -> bool {
    if let Some(ip) = host_literal(url) {
        return !blocked_picture_host(ip);
    }
    let Some(name) = url.domain() else {
        return false;
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let Ok(addresses) = tokio::net::lookup_host((name, port)).await else {
        return false;
    };
    let mut resolved = false;
    for address in addresses {
        resolved = true;
        if blocked_picture_host(address.ip()) {
            return false;
        }
    }
    resolved
}

/// The host as an IP literal, if that is how the URL spells it (`[::1]`
/// keeps its brackets in `host_str`).
fn host_literal(url: &reqwest::Url) -> Option<IpAddr> {
    url.host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// An address a README may not point the reader's machine at: the machine
/// itself, its link (cloud metadata answers on 169.254.169.254), nothing,
/// or everyone. Private ranges stay ALLOWED on purpose — a team's forge and
/// the pictures beside its READMEs live on a LAN, and refusing them would
/// refuse the product's own shape.
pub(crate) fn blocked_picture_host(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| blocked_picture_host(IpAddr::V4(v4)))
        }
    }
}

/// The highlighter's language token: the path's final extension, else the
/// file name itself lowercased (Makefile, Dockerfile). syntect matches both
/// and falls back to plain text on an unknown token — an unknown file renders
/// exactly as the single-ink viewer used to.
pub fn code_token(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => name.to_ascii_lowercase(),
    }
}

/// A retained native code editor in read-only mode, including selection and copy.
pub struct CodeView {
    state: gpui_kit::Entity<gpui_kit::component::input::EditorState>,
    source: String,
    path: String,
    dark: bool,
}
impl CodeView {
    pub fn new(
        source: String,
        path: String,
        dark: bool,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Self {
        use gpui_kit::AppContext as _;
        let language = language_name(&path);
        let state = cx.new(|cx| {
            let mut state = gpui_kit::component::input::EditorState::new(window, cx)
                .language(language)
                .line_number(true);
            state.set_value(expand_tabs(&source), window, cx);
            state
        });
        Self {
            state,
            source,
            path,
            dark,
        }
    }
    pub fn replace(
        &mut self,
        source: String,
        path: String,
        dark: bool,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        if self.source != source || self.path != path {
            *self = Self::new(source, path, dark, window, cx);
        } else {
            self.dark = dark;
        }
        cx.notify();
    }
}
/// Tabs as spaces to the next 4-column stop. Neither the editor nor the text
/// shaper expands a tab: it draws as the font's tab glyph, one space wide,
/// so a tab-indented file loses its indentation.
pub(crate) fn expand_tabs(source: &str) -> String {
    const STOP: usize = 4;
    let mut out = String::with_capacity(source.len());
    let mut column = 0;
    for ch in source.chars() {
        match ch {
            '\t' => {
                let width = STOP - column % STOP;
                out.extend(std::iter::repeat_n(' ', width));
                column += width;
            }
            '\n' => {
                out.push(ch);
                column = 0;
            }
            _ => {
                out.push(ch);
                column += 1;
            }
        }
    }
    out
}

fn language_name(path: &str) -> String {
    match code_token(path).as_str() {
        "rs" => "rust",
        "js" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "py" => "python",
        "sh" | "bash" => "bash",
        "md" => "markdown",
        "yml" => "yaml",
        extension => return extension.to_owned(),
    }
    .to_owned()
}
impl gpui_kit::Render for CodeView {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::*;
        if self.source.is_empty() {
            return div().p_3().child("This file is empty.").into_any_element();
        }
        // the reader fills whatever height its pane gives it and scrolls
        // inside; the pane, not the file's line count, sizes it
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                gpui_kit::component::input::Editor::new(&self.state)
                    .readonly(true)
                    .bordered(false)
                    .h(relative(1.))
                    .aria_label(format!("Code: {}", self.path)),
            )
            .into_any_element()
    }
}
