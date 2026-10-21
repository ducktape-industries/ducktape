//! A run's log, outputs and the packs of the commits it made into files as one commit: each file
//! as 1 MiB chunk blobs (files' chunk size), every chunk put in one bundle, then one Commit that
//! names them, made again on the new head when another commit to the same paths landed first.
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use abi::{Outcome, role};
use node::Frame;
use noded::Upload;
use serde_json::{Value, json};

use super::backend::{blob_json, blocking};
use super::{Landing, Talk};
use crate::Sentence as _;

const CHUNK: &str = "chunk";
pub const CHUNK_SIZE: u64 = 1 << 20;
const PACKS: &str = "/shared/fleet/packs";

struct Chunked {
    entries: Vec<blob::contract::Entry>,
    uploads: Vec<Upload>,
    changes: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed {
    pub git: PathBuf,
    pub repo: String,
    pub commit: String,
}

pub fn pack_path(repo: &str, commit: &str) -> String {
    format!("{PACKS}/{repo}/{commit}.pack")
}

pub fn put_change(path: String, size: u64, chunks: Vec<Value>) -> Value {
    let content = json!({ "Chunks": { "size": size, "chunks": chunks } });
    json!({ "Put": { "path": path, "exec": false, "meta": [], "content": content } })
}

pub async fn written(
    talk: &Talk,
    dir: &Path,
    folder: &str,
    message: String,
    packs: Vec<Packed>,
) -> Result<(Value, Vec<String>), String> {
    let root = dir.to_path_buf();
    let into = folder.to_owned();
    let (chunked, unpacked) = blocking(move || gathered(&root, &into, &packs)).await?;
    if !chunked.entries.is_empty() {
        put(talk, chunked.entries, chunked.uploads).await?;
    }
    for made in ["chunks", "packs"] {
        let made = dir.join(made);
        let _ = blocking(move || std::fs::remove_dir_all(made).sentence()).await;
    }
    let written = committed(talk, chunked.changes, folder, message).await?;
    Ok((written, unpacked))
}

pub async fn committed(
    talk: &Talk,
    changes: Vec<Value>,
    folder: &str,
    message: String,
) -> Result<Value, String> {
    let mut stale_attempts: u64 = 0;
    loop {
        let head = talk.files_ask(json!("Head")).await?;
        let base = head["Head"]["id"].clone();
        let commit = json!({ "Commit": {
            "base": base,
            "message": message,
            "changes": changes,
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

fn gathered(dir: &Path, folder: &str, packs: &[Packed]) -> Result<(Chunked, Vec<String>), String> {
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
    let under = |name: &str| format!("{}/{name}", folder.trim_end_matches('/'));
    let mut files = vec![(dir.join("log"), under("log"))];
    let mut names = Vec::new();
    listed(&out, "", &mut names)?;
    files.extend(names.into_iter().map(|(path, name)| (path, under(&name))));
    let mut unpacked = Vec::new();
    for (index, pack) in packs.iter().enumerate() {
        let into = dir.join("packs").join(format!("{index}.pack"));
        match packed(&pack.git, &pack.commit, &into) {
            Ok(()) => files.push((into, pack_path(&pack.repo, &pack.commit))),
            Err(why) => unpacked.push(format!("{} {}: {why}", pack.repo, pack.commit)),
        }
    }
    Ok((chunked(&files, &dir.join("chunks"))?, unpacked))
}

fn git(dir: &Path) -> Command {
    let mut git = Command::new("git");
    git.args(["-c", "safe.directory=*"])
        .arg(format!("--git-dir={}", dir.display()))
        .env("GIT_OBJECT_DIRECTORY", dir.join("objects"))
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    git
}

pub fn packed(dir: &Path, commit: &str, into: &Path) -> Result<(), String> {
    let refs = git(dir)
        .args(["for-each-ref", "--format=^%(objectname)", "refs/remotes"])
        .stdin(Stdio::null())
        .output()
        .sentence()?;
    if !refs.status.success() {
        let why = String::from_utf8_lossy(&refs.stderr);
        return Err(format!("its remote refs were not read: {}", why.trim()));
    }
    let mut revs = format!("{commit}\n").into_bytes();
    revs.extend(refs.stdout);
    if let Some(parent) = into.parent() {
        std::fs::create_dir_all(parent).sentence()?;
    }
    let file = File::create(into).sentence()?;
    let mut packing = git(dir)
        .args(["pack-objects", "--revs", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(file)
        .stderr(Stdio::piped())
        .spawn()
        .sentence()?;
    let mut stdin = packing
        .stdin
        .take()
        .ok_or("git pack-objects took no input")?;
    stdin.write_all(&revs).sentence()?;
    drop(stdin);
    let ran = packing.wait_with_output().sentence()?;
    if !ran.status.success() {
        let why = String::from_utf8_lossy(&ran.stderr);
        return Err(format!("git pack-objects failed: {}", why.trim()));
    }
    Ok(())
}

fn stale(error: &str) -> bool {
    error.contains(&format!("rejected {}:", abi::reason::STALE))
}

fn chunked(files: &[(PathBuf, String)], pieces: &Path) -> Result<Chunked, String> {
    let mut chunked = Chunked {
        entries: Vec::new(),
        uploads: Vec::new(),
        changes: Vec::new(),
    };
    for (path, to) in files {
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
        chunked.changes.push(put_change(to.clone(), size, chunks));
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

pub async fn put(
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
pub(super) mod tests {
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
            (small.clone(), "/shared/fleet/1/log".to_owned()),
            (large, "/shared/fleet/1/out/large".to_owned()),
            (empty, "/shared/fleet/1/out/empty".to_owned()),
        ];
        let pieces = root.path().join("chunks");
        let made = chunked(&files, &pieces).unwrap();
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
        let refused = gathered(&run, "/shared/fleet/1/", &[]).err().unwrap();
        assert!(refused.contains("symlink"), "{refused}");
    }

    pub fn git_in(dir: &Path, args: &[&str]) -> String {
        let ran = Command::new("git")
            .args([
                "-c",
                "user.name=machine",
                "-c",
                "user.email=machine@fleet",
                "-c",
                "init.defaultBranch=main",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        String::from_utf8(ran.stdout).unwrap().trim().to_owned()
    }

    pub fn made_commit(root: &Path) -> (PathBuf, PathBuf, String) {
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git_in(&origin, &["init", "--quiet"]);
        std::fs::write(origin.join("a"), "a").unwrap();
        git_in(&origin, &["add", "a"]);
        git_in(&origin, &["commit", "--quiet", "-m", "a"]);
        let made = root.join("made");
        git_in(root, &["clone", "--quiet", "origin", "made"]);
        std::fs::write(made.join("b"), "b").unwrap();
        git_in(&made, &["add", "b"]);
        git_in(&made, &["commit", "--quiet", "-m", "b"]);
        let commit = git_in(&made, &["rev-parse", "HEAD"]);
        (origin, made, commit)
    }

    #[test]
    fn a_runs_commit_packs_beside_its_outputs_at_its_pack_path_and_a_bad_one_is_named() {
        let root = tempfile::tempdir().unwrap();
        let (_, made, commit) = made_commit(root.path());
        let run = root.path().join("run");
        std::fs::create_dir_all(run.join("out")).unwrap();
        std::fs::write(run.join("log"), "ran").unwrap();
        let packs = [
            Packed {
                git: made.join(".git"),
                repo: "core".into(),
                commit: commit.clone(),
            },
            Packed {
                git: root.path().join("nowhere"),
                repo: "app".into(),
                commit: "ab".repeat(20),
            },
        ];
        let (chunked, unpacked) = gathered(&run, "/shared/fleet/3/", &packs).unwrap();
        let paths: Vec<&str> = chunked
            .changes
            .iter()
            .map(|change| change["Put"]["path"].as_str().unwrap())
            .collect();
        let pack = pack_path("core", &commit);
        assert_eq!(paths, ["/shared/fleet/3/log", pack.as_str()]);
        assert_eq!(unpacked.len(), 1);
        assert!(unpacked[0].starts_with("app "), "{unpacked:?}");
    }
}
