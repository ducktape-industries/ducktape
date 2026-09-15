//! Files commit policy shared by deployed views. Device grants remain host-owned.
use base64::Engine as _;
use ducktape_view_guest::host;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedFile {
    pub token: String,
    pub name: String,
    pub bytes: u64,
}
async fn submit_bytes(target: &str, bytes: Vec<u8>) -> Result<(), String> {
    let ask = serde_json::json!({ "target": target, "body_b64": base64::engine::general_purpose::STANDARD.encode(bytes) });
    host::request(
        "op.submit_bytes",
        &serde_json::to_vec(&ask).expect("binary submit envelope"),
    )
    .await
    .map(|_| ())
}

pub async fn upload(file: SelectedFile, path: String) -> Result<String, String> {
    let result = upload_inner(&file, path).await;
    if result.is_ok() {
        release(&file.token).await;
    }
    result
}

pub async fn release(token: &str) {
    let _ = host::request("fs.release", token.as_bytes()).await;
}

async fn upload_inner(file: &SelectedFile, path: String) -> Result<String, String> {
    const MAX_UPLOAD: u64 = 64 << 20;
    if file.bytes > MAX_UPLOAD {
        return Err("Files must be at most 64 MiB".into());
    }
    duckfs_core::paths::canonical(&path)?;
    let ask = serde_json::json!({"target":"files", "query":{"refs":{}}});
    let refs: serde_json::Value = serde_json::from_slice(
        &host::request("rpc.query", &serde_json::to_vec(&ask).expect("refs query")).await?,
    )
    .map_err(|error| error.to_string())?;
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut offset = 0u64;
    let inline = file.bytes <= duckfs_core::MAX_INLINE_COMMIT_BYTES as u64;
    while offset < file.bytes {
        let len = (file.bytes - offset).min(256 << 10) as usize;
        let ask = serde_json::json!({"token":file.token, "offset":offset, "len":len});
        let bytes = host::request(
            "fs.read",
            &serde_json::to_vec(&ask).expect("file read envelope"),
        )
        .await?;
        let invalid_read = bytes.is_empty() || bytes.len() > len;
        if invalid_read {
            return Err("The selected file changed during its upload".into());
        }
        offset += bytes.len() as u64;
        chunk.extend_from_slice(&bytes);
        let chunk_ready =
            !inline && (chunk.len() as u64 == duckfs_core::CHUNK_SIZE || offset == file.bytes);
        if chunk_ready {
            submit_bytes("files", duckfs_core::encode_putblob(&chunk)).await?;
            chunks.push(duckfs_core::to_hex(&duckfs_core::objects::object_id(
                duckfs_core::Kind::Chunk,
                &chunk,
            )));
            chunk.clear();
        }
    }
    let content = if inline {
        duckfs_core::Content::Inline {
            b64: base64::engine::general_purpose::STANDARD.encode(chunk),
        }
    } else {
        duckfs_core::Content::Chunks {
            size: file.bytes,
            chunks,
        }
    };
    let commit = duckfs_core::FilesMsg::Commit {
        base_snapshot: refs["refs"]["head"].as_str().map(str::to_owned),
        message: format!("upload {}", file.name),
        changes: vec![duckfs_core::Change::Put {
            path: path.clone(),
            exec: false,
            meta: Default::default(),
            content,
        }],
    };
    submit_bytes("files", duckfs_core::encode_msg(&commit)).await?;
    Ok(format!("duck://files{path}"))
}
