//! A run's log and outputs into files as one commit: each file as 1 MiB chunk blobs (files' chunk
//! size), every chunk put in one bundle, then one Commit that names them, made again on the new
//! head when another commit to the same paths landed first.
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use abi::{Outcome, role};
use node::Frame;
use noded::Upload;
use serde_json::{Value, json};

use super::backend::{blob_json, blocking};
use super::{Landing, Talk};
use crate::Sentence as _;

const CHUNK: &str = "chunk";
const CHUNK_SIZE: u64 = 1 << 20;

struct Chunked {
    entries: Vec<blob::contract::Entry>,
    uploads: Vec<Upload>,
    changes: Vec<Value>,
}

pub async fn written(
    talk: &Talk,
    dir: &Path,
    folder: &str,
    message: String,
) -> Result<Value, String> {
    let root = dir.to_path_buf();
    let into = folder.to_owned();
    let chunked = blocking(move || gathered(&root, &into)).await?;
    if !chunked.entries.is_empty() {
        put(talk, chunked.entries, chunked.uploads).await?;
    }
    let pieces = dir.join("chunks");
    let _ = blocking(move || std::fs::remove_dir_all(pieces).sentence()).await;
    let mut stale_attempts: u64 = 0;
    loop {
        let head = talk.files_ask(json!("Head")).await?;
        let base = head["Head"]["id"].clone();
        let commit = json!({ "Commit": {
            "base": base,
            "message": message,
            "changes": chunked.changes,
        }});
        match talk.files(commit, Landing::Land).await {
            Ok(landed) => {
                let snapshot = landed
                    .get("returned")
                    .cloned()
                    .ok_or("files answered the commit with no snapshot")?;
                return Ok(json!({ "path": folder, "snapshot": snapshot }));
            }
            Err(error) if stale(&error) => {
                stale_attempts += 1;
                if stale_attempts.is_power_of_two() {
                    tracing::warn!(
                        target: "ducktape::machine",
                        event = "machine_commit_stale",
                        attempts = stale_attempts,
                        reason = "base_moved",
                        "another commit landed first; the machine commits the run's outputs again on the new head"
                    );
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn gathered(dir: &Path, folder: &str) -> Result<Chunked, String> {
    let out = dir.join("out");
    let linked = std::fs::symlink_metadata(&out)
        .sentence()?
        .file_type()
        .is_symlink();
    if linked {
        return Err(format!(
            "{} is a symlink, which the machine never follows",
            out.display()
        ));
    }
    let mut files = vec![(dir.join("log"), "log".to_owned())];
    listed(&out, "", &mut files)?;
    chunked(&files, &dir.join("chunks"), folder)
}

fn stale(error: &str) -> bool {
    error.contains(&format!("rejected {}:", abi::reason::STALE))
}

fn chunked(files: &[(PathBuf, String)], pieces: &Path, folder: &str) -> Result<Chunked, String> {
    let mut chunked = Chunked {
        entries: Vec::new(),
        uploads: Vec::new(),
        changes: Vec::new(),
    };
    for (path, name) in files {
        let size = std::fs::metadata(path).sentence()?.len();
        let parts = match size {
            0 => Vec::new(),
            1..=CHUNK_SIZE => vec![path.clone()],
            _ => split(path, size, pieces, chunked.uploads.len())?,
        };
        let mut chunks = Vec::new();
        for part in parts {
            let entry = blob::file_entry(CHUNK, &part).map_err(|refusal| refusal.sentence)?;
            chunks.push(blob_json(&entry.id));
            let fresh = !chunked.entries.iter().any(|held| held.id == entry.id);
            if fresh {
                chunked.entries.push(entry);
                chunked.uploads.push(Upload {
                    kind: CHUNK.to_owned(),
                    path: part,
                });
            }
        }
        let path = format!("{}/{name}", folder.trim_end_matches('/'));
        let content = json!({ "Chunks": { "size": size, "chunks": chunks } });
        chunked.changes.push(json!({
            "Put": { "path": path, "exec": false, "meta": [], "content": content }
        }));
    }
    Ok(chunked)
}

fn split(path: &Path, size: u64, pieces: &Path, first: usize) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(pieces).sentence()?;
    let mut file = File::open(path).sentence()?;
    let mut parts = Vec::new();
    for index in 0..size.div_ceil(CHUNK_SIZE) {
        let mut bytes = Vec::new();
        (&mut file)
            .take(CHUNK_SIZE)
            .read_to_end(&mut bytes)
            .sentence()?;
        let piece = pieces.join(format!("{first}-{index}"));
        std::fs::write(&piece, &bytes).sentence()?;
        parts.push(piece);
    }
    Ok(parts)
}

fn listed(dir: &Path, under: &str, files: &mut Vec<(PathBuf, String)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .sentence()?
        .collect::<Result<_, _>>()
        .sentence()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = format!("{under}{}", entry.file_name().to_string_lossy());
        let kind = entry.file_type().sentence()?;
        if kind.is_dir() {
            listed(&entry.path(), &format!("{name}/"), files)?;
            continue;
        }
        if kind.is_file() {
            files.push((entry.path(), format!("out/{name}")));
        }
    }
    Ok(())
}

async fn put(
    talk: &Talk,
    entries: Vec<blob::contract::Entry>,
    uploads: Vec<Upload>,
) -> Result<(), String> {
    let status = talk.client.status().await.sentence()?;
    let program = status
        .roles
        .bound(role::BLOBS)
        .map_err(|refusal| refusal.sentence)?;
    let put = blob::contract::Op::Put { blobs: entries };
    let frame = Frame::sign_as(
        crate::CLIENT,
        &talk.key,
        status.network.as_bytes(),
        status.anchor(),
        Some(crate::salt()),
        program,
        abi::encode(&put),
    );
    let (_, receipt) = talk.client.land_blobs(&frame, uploads).await.sentence()?;
    match receipt.outcome {
        Outcome::Applied { .. } => Ok(()),
        Outcome::Rejected(refusal) => Err(format!(
            "the chunks were refused: {}: {}",
            refusal.reason, refusal.sentence
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_are_listed_under_out_in_path_order_beside_the_log() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("out");
        std::fs::create_dir_all(out.join("b")).unwrap();
        std::fs::write(out.join("z"), "z").unwrap();
        std::fs::write(out.join("b/a"), "a").unwrap();
        let mut files = Vec::new();
        listed(&out, "", &mut files).unwrap();
        let names: Vec<&str> = files.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(names, ["out/b/a", "out/z"]);
    }

    #[test]
    fn a_small_file_is_its_own_chunk_a_large_one_is_cut_and_an_empty_one_has_none() {
        let root = tempfile::tempdir().unwrap();
        let small = root.path().join("small");
        let large = root.path().join("large");
        let empty = root.path().join("empty");
        std::fs::write(&small, "hi").unwrap();
        std::fs::write(&large, vec![7u8; (CHUNK_SIZE + 1) as usize]).unwrap();
        std::fs::write(&empty, "").unwrap();
        let files = [
            (small.clone(), "log".to_owned()),
            (large, "out/large".to_owned()),
            (empty, "out/empty".to_owned()),
        ];
        let pieces = root.path().join("chunks");
        let made = chunked(&files, &pieces, "/shared/fleet/1/").unwrap();
        assert_eq!(made.uploads.len(), 3);
        assert_eq!(made.uploads[0].path, small);
        let counts: Vec<usize> = made
            .changes
            .iter()
            .map(|change| {
                change["Put"]["content"]["Chunks"]["chunks"]
                    .as_array()
                    .unwrap()
                    .len()
            })
            .collect();
        assert_eq!(counts, [1, 2, 0]);
        assert_eq!(
            made.changes[1]["Put"]["path"],
            json!("/shared/fleet/1/out/large")
        );
        assert!(stale(
            "landed 9 rejected stale: /shared/fleet/1/log changed"
        ));
        assert!(!stale("landed 9 rejected unauthorized: no"));
    }

    #[cfg(unix)]
    #[test]
    fn an_out_that_is_a_symlink_is_refused_and_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("log"), "").unwrap();
        std::os::unix::fs::symlink(root.path(), run.join("out")).unwrap();
        let refused = gathered(&run, "/shared/fleet/1/").err().unwrap();
        assert!(refused.contains("symlink"), "{refused}");
    }
}
