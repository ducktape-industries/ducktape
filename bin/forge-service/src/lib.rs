//! Installed Git smart-HTTP process; consensus policy remains in its module.
mod git_http;
mod merge;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use serde::Deserialize;
use subtle::ConstantTimeEq as _;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node_url: String,
    pub node_key: String,
    pub chain_id: String,
    pub account: u64,
    pub label: String,
    pub module: String,
    pub git_store: PathBuf,
    /// This process's own private seed, never the node's key.
    pub signing_seed: String,
}

#[derive(Clone)]
struct ServiceState {
    client: ducktape_rpc::Client,
    signer: ed25519::PrivateKey,
    sequence: Arc<AtomicU64>,
    chain_id: String,
    module: String,
    forge_repo: PathBuf,
    account: u64,
    label: String,
    token: [u8; 64],
    requests: Arc<tokio::sync::Semaphore>,
}

impl ServiceState {
    async fn submit(
        &self,
        payload: Vec<u8>,
        required_blob: Option<[u8; 32]>,
    ) -> Result<u64, String> {
        let frame = node::encode_frame_with_blob(
            &self.signer,
            self.sequence.fetch_add(1, Ordering::Relaxed),
            &sdk::Msg {
                target: self.module.clone(),
                payload,
            },
            required_blob,
        );
        self.client
            .submit_frame(frame)
            .await
            .map_err(|error| error.to_string())
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error":message}))).into_response()
}

async fn authenticate(State(state): State<ServiceState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let field = |name| {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        values.next().is_none().then_some(value)
    };
    let token_matches = field("x-duck-upstream-token")
        .is_some_and(|token| bool::from(token.as_bytes().ct_eq(&state.token)));
    let route_matches = field("x-duck-route-account").and_then(|value| value.parse::<u64>().ok())
        == Some(state.account)
        && field("x-duck-route-label") == Some(state.label.as_str())
        && field("x-duck-route-revision")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|revision| revision > 0);
    if !token_matches || !route_matches {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "authenticated Gateway route required",
        );
    }
    let Ok(_permit) = state.requests.try_acquire() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "request capacity exhausted",
        );
    };
    next.run(request).await
}

pub fn router(config: Config, token: [u8; 64]) -> Result<axum::Router, Box<dyn std::error::Error>> {
    let canonical_key = |text: &str| {
        text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let valid_config = canonical_key(&config.node_key)
        && canonical_key(&config.signing_seed)
        && !config.chain_id.is_empty()
        && config.account > 0
        && !config.label.is_empty()
        && !config.module.is_empty()
        && config.module.len() <= node::MAX_TARGET_BYTES
        && config.git_store.is_absolute()
        && config.git_store.is_dir();
    if !valid_config {
        return Err("invalid application config".into());
    }
    let signer = ed25519::PrivateKey::decode(hex::decode(&config.signing_seed)?.as_slice())?;
    let auth_signer = signer.clone();
    let node_key = hex::decode(&config.node_key)?;
    let client = ducktape_rpc::Client::new(&config.node_url)?.with_write_auth(Arc::new(
        move |method, path, body| {
            node::signed_req::request_headers(&auth_signer, method, path, &node_key, body)
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect()
        },
    ));
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let state = ServiceState {
        client,
        signer,
        sequence: Arc::new(AtomicU64::new(sequence)),
        chain_id: config.chain_id,
        module: config.module,
        forge_repo: config.git_store,
        account: config.account,
        label: config.label,
        token,
        requests: Arc::new(tokio::sync::Semaphore::new(2)),
    };
    Ok(axum::Router::new()
        .route(
            "/merge",
            axum::routing::post(merge::merge).layer(DefaultBodyLimit::max(8192)),
        )
        .route(
            "/{repo}/info/refs",
            axum::routing::get(git_http::git_info_refs),
        )
        .route(
            "/{repo}/git-receive-pack",
            axum::routing::post(git_http::git_receive_pack),
        )
        .route(
            "/{repo}/git-upload-pack",
            axum::routing::post(git_http::git_upload_pack),
        )
        .layer(DefaultBodyLimit::max(blobstore::MAX_TRANSFER_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .with_state(state))
}

#[cfg(test)]
mod tests;
