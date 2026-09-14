//! What is left of duckfs on the app's side of the wire.
//!
//! The browser — the listing, the preview, the snapshot history, a diff and
//! every write the reader makes — belongs to the `files` VIEW, which reads and
//! writes the module itself through the kernel contract. Two things cannot:
//! a file DROPPED on the window (the bytes are this device's, and only this
//! process can read them) and the picture bytes a host surface decodes. Both
//! page duckfs through [`files_read_all`] / [`files_upload`] here.

use super::*;

/// Page one duckfs file in whole through the `read` lane (1 MiB pages to eof
/// — the checkout's `read_all` shape). `None`: past the picture byte cap,
/// not assembled.
pub(crate) async fn files_read_all(rpc: &RpcClient, path: &str) -> Result<Option<Vec<u8>>, String> {
    use super::picture::MAX_PICTURE_BYTES;
    // The `read` lane's own page cap (duckfs `MAX_READ_BYTES`); the node clamps
    // anything larger, so asking for exactly it is one round-trip per MiB.
    let page_len = (1024 * 1024).to_string();
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len().to_string();
        let reply = rpc
            .files_get(
                "read",
                &[
                    ("path", path),
                    ("offset", offset.as_str()),
                    ("len", page_len.as_str()),
                ],
            )
            .await?;
        let page = base64_decode(reply["b64"].as_str().unwrap_or_default())
            .ok_or("The node's read page is not valid base64")?;
        let eof = reply["eof"].as_bool().unwrap_or(true);
        bytes.extend_from_slice(&page);
        let past_cap = bytes.len() > MAX_PICTURE_BYTES;
        if past_cap {
            return Ok(None);
        }
        let done = eof || page.is_empty();
        if done {
            return Ok(Some(bytes));
        }
    }
}

/// The head snapshot id for commit CAS (empty when nothing is committed).
async fn files_head(rpc: &RpcClient) -> Result<Option<String>, String> {
    let refs = rpc.files_get("refs", &[]).await?;
    Ok(refs["head"].as_str().map(str::to_string))
}

/// One commit, SIGNED BY THE PERSON. The files module records a frame's
/// verified signer as the commit's author and charges `/home/<owner>/**`
/// authority to it, so a person's commit rides the same signed-frame lane as
/// every other op this device's key makes. The unsigned `/v1/files/commit`
/// lane writes as the NODE — a daemon's authority over a daemon's home,
/// never a person's over theirs.
async fn files_commit_one(
    rpc: &RpcClient,
    password: String,
    message: String,
    change: serde_json::Value,
) -> Result<(), String> {
    let head = files_head(rpc).await?;
    let payload = files_commit_payload(head, message, change)?;
    signed_write(rpc, "files", payload, password).await?;
    Ok(())
}

/// Upload a local file dropped onto the window into the current directory:
/// small files ride inline; larger ones stage 1 MiB chunks then commit a
/// chunk list. The dropped path never leaves this device — only bytes do.
pub async fn files_upload(
    rpc: String,
    password: String,
    dir: String,
    dropped: String,
) -> Result<bool, AppError> {
    let name = file_name(&dropped).map_err(app_error)?;
    files_put(rpc, password, dir, dropped, name)
        .await
        .map_err(app_error)?;
    Ok(true)
}

/// Where a message's files live: one directory per message under the
/// attachments root, the root `duck_uri::classify_files` opens.
pub const ATTACHMENTS_ROOT: &str = "/shared/attachments";

/// Upload each local file beside the message it goes with. Sequential on
/// purpose: every put is its own head-based commit.
pub async fn attach_files(
    rpc: &str,
    password: &str,
    message_id: &str,
    sources: &[String],
) -> Result<(), String> {
    let dir = format!("{ATTACHMENTS_ROOT}/{message_id}");
    for source in sources {
        let name = attachment_name(&file_name(source)?);
        files_put(
            rpc.to_owned(),
            password.to_owned(),
            dir.clone(),
            source.clone(),
            name,
        )
        .await?;
    }
    Ok(())
}

/// The body a send with files posts: the typed text, then one link line per
/// file at the path `attach_files` puts it — so the row, the optimistic row
/// and the runs injector all read the same address.
pub fn attachment_body(body: String, message_id: &str, names: &[String]) -> String {
    if names.is_empty() {
        return body;
    }
    let mut lines: Vec<String> = body.lines().map(str::to_owned).collect();
    for name in names {
        let name = attachment_name(name);
        lines.push(format!(
            "[{name}](duck://files{ATTACHMENTS_ROOT}/{message_id}/{name})"
        ));
    }
    lines.join("\n")
}

/// The name a file is stored and linked under: what a markdown link can
/// carry — no whitespace, no brackets, no parens — with the rest kept.
pub fn attachment_name(name: &str) -> String {
    let safe = name.chars().map(|c| {
        let breaks_link = c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '/');
        if breaks_link { '_' } else { c }
    });
    safe.collect()
}

fn file_name(path: &str) -> Result<String, String> {
    PathBuf::from(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| "dropped path has no file name".to_string())
}

async fn files_put(
    rpc: String,
    password: String,
    dir: String,
    dropped: String,
    name: String,
) -> Result<(), String> {
    // the node refuses to serve back any object larger than files_http's
    // MAX_OBJECT_BYTES, and every staged MiB is a consensus block — a cap
    // HERE turns "drop a video, drive 300 blocks, node RSS grows by 300 MB"
    // into one refusal toast.
    const MAX_DROP_BYTES: u64 = 64 * 1024 * 1024;
    async {
        let source = PathBuf::from(&dropped);
        let size = std::fs::metadata(&source)
            .map_err(|error| format!("cannot read {dropped}: {error}"))?
            .len();
        if size > MAX_DROP_BYTES {
            return Err(format!(
                "{name} is {} MiB — the node stores files up to {} MiB",
                size / (1024 * 1024),
                MAX_DROP_BYTES / (1024 * 1024)
            ));
        }
        // off the render runtime: a multi-MB read is a blocking call.
        let read = tokio::task::spawn_blocking(move || std::fs::read(&source));
        let bytes = read
            .await
            .map_err(|error| format!("file read task failed: {error}"))?
            .map_err(|error| format!("cannot read {dropped}: {error}"))?;
        let rpc = rpc_client(&rpc)?;
        let target = fs_child(dir, name.clone());
        // The module's own bounds: what a commit may carry inline, and the
        // size of one staged chunk.
        let rides_inline = bytes.len() <= duckfs_core::MAX_INLINE_COMMIT_BYTES;
        let chunk_size =
            usize::try_from(duckfs_core::CHUNK_SIZE).expect("a duckfs chunk fits in memory");
        let content = match rides_inline {
            true => serde_json::json!({ "inline": { "b64": base64_encode(&bytes) } }),
            false => {
                // A staged chunk is a raw-bytes write charged to the person,
                // so each one is signed the way the commit below is.
                let staging = rpc
                    .clone()
                    .with_write_auth(data_plane_signer(&rpc, password.clone()).await?);
                let mut chunks = Vec::new();
                for chunk in bytes.chunks(chunk_size) {
                    chunks.push(staging.files_stage(chunk.to_vec()).await?);
                }
                serde_json::json!({ "chunks": { "size": bytes.len() as u64, "chunks": chunks } })
            }
        };
        files_commit_one(
            &rpc,
            password,
            format!("upload {name}"),
            serde_json::json!({
                "put": { "path": target, "exec": false, "meta": {}, "content": content }
            }),
        )
        .await
    }
    .await
}

/// The files read lane's wire: standard alphabet, padded — the same engine
/// duckfs-core encodes with, so both ends share one reading of a byte.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a `b64` page. `None` is a MALFORMED page — bad padding, trailing
/// bits, a character off the alphabet — and a caller treats it as the read
/// failing, never as an empty file: a node that answered garbage did not
/// answer nothing.
pub(crate) fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(input).ok()
}

/// A child path under the current directory (`/` is the root, never "").
pub fn fs_child(path: String, name: String) -> String {
    let name = name.trim().trim_matches('/');
    let dir = path.trim_end_matches('/');
    format!("{dir}/{name}")
}

/// Preview the files module's authority rule using the cached account and
/// actual signing key. The module resolves identity again when the write lands.
pub fn files_write_gate(dir: String, me: String) -> String {
    if me.is_empty() {
        return String::new();
    }
    let key = match public_key(&me, "user key") {
        Ok(key) => key,
        Err(reason) => return reason,
    };
    let authority = duckfs_core::Authority::External {
        key,
        account: names().account_of(&me),
    };
    let entry = fs_child(dir, "entry".into());
    let checked = duckfs_core::paths::canonical(&entry)
        .and_then(|segments| duckfs_core::paths::check_authority(&authority, &segments));
    match checked {
        Ok(()) => String::new(),
        Err(reason) => reason.trim_start_matches("files: ").to_string(),
    }
}

#[cfg(test)]
#[path = "storage_edit_tests.rs"]
mod edit_tests;

fn files_commit_payload(
    head: Option<String>,
    message: String,
    change: serde_json::Value,
) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&serde_json::json!({
        "commit": {
            "base_snapshot": head,
            "message": message,
            "changes": [change],
        }
    }))
    .map_err(|error| format!("files commit does not serialize: {error}"))
}
