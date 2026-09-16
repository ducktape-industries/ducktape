//! What is left of duckfs on the app's side of the wire.
//!
//! The browser — the listing, the preview, the snapshot history, a diff and
//! every write the reader makes — belongs to the `files` VIEW, which reads and
//! writes the module itself through the kernel contract. The native picture
//! surface reads image bytes through [`files_read_all`].

use super::*;

/// Page one duckfs file in whole through the `read` lane (1 MiB pages to eof
/// — the checkout's `read_all` shape). `None`: past the picture byte cap,
/// not assembled.
pub(crate) async fn files_read_all(rpc: &RpcClient, path: &str) -> Result<Option<Vec<u8>>, String> {
    use super::picture::MAX_PICTURE_BYTES;
    // The `read` lane's own page cap (duckfs `MAX_READ_BYTES`); the node clamps
    // anything larger, so asking for exactly it is one round-trip per MiB.
    let page_len = 1024 * 1024;
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len();
        let reply = rpc
            .query::<_, serde_json::Value>(
                "files",
                &serde_json::json!({
                    "read": {"path":path, "offset":offset, "len":page_len}
                }),
            )
            .await?;
        let reply = &reply["read"];
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
