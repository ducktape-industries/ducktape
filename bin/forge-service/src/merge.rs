//! Merge calculation reads the installed tenant store and writes only temporary objects.
use super::{ServiceState, error_response};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    repo: String,
    ours: String,
    theirs: String,
    message: String,
}

pub async fn merge(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<MergeRequest>,
) -> Response {
    let seated = headers
        .get("x-duck-caller-account")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|account| account > 0);
    if !seated {
        return error_response(StatusCode::UNAUTHORIZED, "signed user required for merge");
    }
    use axum::response::IntoResponse as _;
    match calculate(&state, request).await {
        Ok(reply) => Json(reply).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, &error),
    }
}

async fn calculate(
    state: &ServiceState,
    request: MergeRequest,
) -> Result<serde_json::Value, String> {
    let repo = forge::norm_repo(&request.repo).map_err(|error| error.to_string())?;
    let valid_message = !request.message.is_empty() && request.message.len() <= 4096;
    if !valid_message {
        return Err("invalid merge request".into());
    }
    let ours = git2::Oid::from_str(&request.ours).map_err(git_err)?;
    let theirs = git2::Oid::from_str(&request.theirs).map_err(git_err)?;
    let directory = state.forge_repo.join(repo);
    let build = tokio::task::spawn_blocking(move || {
        let mirror = git2::Repository::open(directory).map_err(git_err)?;
        merge_against_mirror(&mirror, ours, theirs, &request.message)
    })
    .await
    .map_err(|error| error.to_string())??;
    match build {
        MergeBuild::Conflicts(paths) => Ok(serde_json::json!({"conflicts":paths})),
        MergeBuild::Clean { merge_oid, pack } => {
            if pack.len() > blobstore::MAX_TRANSFER_BYTES {
                return Err("merge pack exceeds transfer limit".into());
            }
            let pack_digest = state
                .client
                .put_blob(pack)
                .await
                .map_err(|error| error.to_string())?;
            Ok(serde_json::json!({"merge_oid":merge_oid,"pack_digest":pack_digest}))
        }
    }
}

pub enum MergeBuild {
    Clean { merge_oid: String, pack: Vec<u8> },
    Conflicts(Vec<String>),
}

fn git_err(error: git2::Error) -> String {
    error.message().to_owned()
}

pub(crate) fn merge_against_mirror(
    mirror: &git2::Repository,
    ours_oid: git2::Oid,
    theirs_oid: git2::Oid,
    message: &str,
) -> Result<MergeBuild, String> {
    let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn merge_builder_produces_the_cas_commit_and_its_minimal_pack() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = git2::Repository::init_bare(dir.path()).unwrap();
        let base = mirror_commit(&mirror, None, &[("a.txt", "base\n"), ("b.txt", "keep\n")]);
        let ours = mirror_commit(
            &mirror,
            Some(base),
            &[("a.txt", "ours\n"), ("b.txt", "keep\n")],
        );
        let theirs = mirror_commit(
            &mirror,
            Some(base),
            &[("a.txt", "base\n"), ("b.txt", "theirs\n")],
        );

        let build = merge_against_mirror(&mirror, ours, theirs, "Merge pull request #1").unwrap();
        let MergeBuild::Clean { merge_oid, pack } = build else {
            panic!("disjoint edits must merge cleanly");
        };

        // land the pack in the mirror and read the merge commit back out —
        // exactly what a validator does after the blob fan-out.
        let odb = mirror.odb().unwrap();
        let mut writepack = odb.packwriter().unwrap();
        std::io::Write::write_all(&mut writepack, &pack).unwrap();
        writepack.commit().unwrap();
        let merged = mirror
            .find_commit(git2::Oid::from_str(&merge_oid).unwrap())
            .unwrap();
        let parents: Vec<git2::Oid> = merged.parent_ids().collect();
        assert_eq!(parents, vec![ours, theirs], "target first, source second");
        let tree = merged.tree().unwrap();
        let read = |path: &str| {
            let entry = tree.get_path(Path::new(path)).unwrap();
            String::from_utf8(mirror.find_blob(entry.id()).unwrap().content().to_vec()).unwrap()
        };
        assert_eq!(read("a.txt"), "ours\n");
        assert_eq!(read("b.txt"), "theirs\n");
    }

    #[test]
    fn merge_builder_reports_conflicts_and_builds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = git2::Repository::init_bare(dir.path()).unwrap();
        let base = mirror_commit(&mirror, None, &[("a.txt", "base\n")]);
        let ours = mirror_commit(&mirror, Some(base), &[("a.txt", "ours\n")]);
        let theirs = mirror_commit(&mirror, Some(base), &[("a.txt", "theirs\n")]);

        let build = merge_against_mirror(&mirror, ours, theirs, "Merge pull request #2").unwrap();
        let MergeBuild::Conflicts(paths) = build else {
            panic!("competing edits must conflict");
        };
        assert_eq!(paths, vec!["a.txt".to_string()]);
    }

    fn mirror_commit(
        repo: &git2::Repository,
        parent: Option<git2::Oid>,
        files: &[(&str, &str)],
    ) -> git2::Oid {
        let mut tree = repo.treebuilder(None).unwrap();
        for (path, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            tree.insert(path, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("mule", "mule@localhost").unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|oid| vec![repo.find_commit(oid).unwrap()])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &signature, &signature, "mule", &tree, &parent_refs)
            .unwrap()
    }

    #[tokio::test]
    async fn deployed_guest_merge_calls_the_service_and_submits_its_actual_commit() {
        use axum::{
            body::{Body, Bytes},
            http::Request,
        };
        use base64::Engine as _;
        use ducktape_view_guest::testing::answer;
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;
        let directory = tempfile::tempdir().unwrap();
        let mirror = git2::Repository::init(directory.path().join("lab")).unwrap();
        let base = mirror_commit(&mirror, None, &[("a", "base"), ("b", "base")]);
        let ours = mirror_commit(&mirror, Some(base), &[("a", "ours"), ("b", "base")]);
        let theirs = mirror_commit(&mirror, Some(base), &[("a", "base"), ("b", "theirs")]);
        let (packs, mut received) = tokio::sync::mpsc::channel(1);
        let cas = axum::Router::new().route(
            "/v1/files/blob",
            axum::routing::post(move |bytes: Bytes| {
                let packs = packs.clone();
                async move {
                    packs.send(bytes.to_vec()).await.unwrap();
                    Json(serde_json::json!({"digest":"aa".repeat(32)}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, cas).await.unwrap();
        });
        let app = crate::router(
            crate::Config {
                node_url: origin,
                node_key: "01".repeat(32),
                signing_seed: "09".repeat(32),
                chain_id: "test".into(),
                account: 42,
                label: "git".into(),
                module: "forge".into(),
                git_store: directory.path().into(),
            },
            [b'a'; 64],
        )
        .unwrap();
        forge_view::boot_native();
        let _ = forge_view::tick_native(Vec::new());
        forge_view::host::merge(
            "lab".into(),
            7,
            "feature".into(),
            theirs.to_string(),
            ours.to_string(),
        );
        let frame = forge_view::tick_native(Vec::new());
        let asset = frame
            .requests
            .iter()
            .find(|request| request.kind == "asset.read")
            .unwrap();
        let frame =
            forge_view::tick_native(vec![answer(asset.id, br#"{"account":42,"route":"git"}"#)]);
        let request = frame
            .requests
            .iter()
            .find(|request| request.kind == "net.request")
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        assert_eq!(envelope["account"], 42);
        assert_eq!(envelope["route"], "git");
        let body: Vec<u8> = serde_json::from_value(envelope["body"].clone()).unwrap();
        let http = Request::builder()
            .method("POST")
            .uri(envelope["path"].as_str().unwrap())
            .header("content-type", "application/json")
            .header("x-duck-upstream-token", "a".repeat(64))
            .header("x-duck-route-account", "42")
            .header("x-duck-route-label", "git")
            .header("x-duck-route-revision", "1")
            .header("x-duck-caller-account", "7")
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(http).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let reply = serde_json::json!({"head":{"status":200,"headers":[]},"body_b64":base64::engine::general_purpose::STANDARD.encode(bytes)});
        let frame = forge_view::tick_native(vec![answer(
            request.id,
            &serde_json::to_vec(&reply).unwrap(),
        )]);
        let submit = frame
            .requests
            .iter()
            .find(|request| request.kind == "op.submit")
            .unwrap();
        let operation: serde_json::Value = serde_json::from_slice(&submit.payload).unwrap();
        assert_eq!(
            operation["payload"]["merge_pr"]["prev_target_oid"],
            ours.to_string()
        );
        assert_eq!(
            operation["payload"]["merge_pr"]["expected_source_oid"],
            theirs.to_string()
        );
        assert_eq!(
            operation["payload"]["merge_pr"]["pack_digest"],
            "aa".repeat(32)
        );
        let merged = git2::Oid::from_str(
            operation["payload"]["merge_pr"]["merge_oid"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(
            mirror.find_commit(merged).is_err(),
            "service never writes the mounted store"
        );
        let odb = mirror.odb().unwrap();
        let mut writer = odb.packwriter().unwrap();
        std::io::Write::write_all(&mut writer, &received.recv().await.unwrap()).unwrap();
        writer.commit().unwrap();
        let commit = mirror.find_commit(merged).unwrap();
        assert_eq!(commit.parent_id(0).unwrap(), ours);
        assert_eq!(commit.parent_id(1).unwrap(), theirs);
        server.abort();
    }
}
