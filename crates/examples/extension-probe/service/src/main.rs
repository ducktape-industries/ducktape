//! Independently installed example protocol. No node library or module-specific
//! native router participates: authorization is a query to the deployed guest.
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc, time::Duration};
use subtle::ConstantTimeEq as _;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    node: String,
    module: String,
    account: u64,
    route: String,
}

struct Service {
    config: Config,
    token: [u8; 64],
    http: reqwest::Client,
    sockets: Arc<tokio::sync::Semaphore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ask {
    text: String,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn caller(service: &Service, headers: &HeaderMap) -> Option<u64> {
    let token = header(headers, "x-duck-upstream-token")?.as_bytes();
    let verified = token.len() == 64 && bool::from(token.ct_eq(&service.token));
    if !verified {
        return None;
    }
    let account = header(headers, "x-duck-route-account")?
        .parse::<u64>()
        .ok()?;
    let route = header(headers, "x-duck-route-label")?;
    let revision = header(headers, "x-duck-route-revision")?
        .parse::<u64>()
        .ok()?;
    let bound = account == service.config.account && route == service.config.route && revision > 0;
    if !bound {
        return None;
    }
    header(headers, "x-duck-caller-account")?
        .parse::<u64>()
        .ok()
        .filter(|account| *account > 0)
}

impl Service {
    async fn reply(&self, account: u64, ask: Ask) -> Result<String, StatusCode> {
        let bounded = !ask.text.is_empty() && ask.text.len() <= 128;
        if !bounded {
            return Err(StatusCode::BAD_REQUEST);
        }
        let mut response = self
            .http
            .post(format!("{}/v1/query", self.config.node))
            .json(&serde_json::json!({"target":self.config.module,
                "query":{"authorize":{"account":account,"text":ask.text}}}))
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        if !response.status().is_success() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?
        {
            let exceeds_reply = bytes.len().saturating_add(chunk.len()) > 16;
            if exceeds_reply {
                return Err(StatusCode::BAD_GATEWAY);
            }
            bytes.extend_from_slice(&chunk);
        }
        let allowed: bool = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_GATEWAY)?;
        if !allowed {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(match cfg!(feature = "replacement") {
            true => ask.text.to_lowercase(),
            false => ask.text.to_uppercase(),
        })
    }
}

async fn request(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Json(ask): Json<Ask>,
) -> Response {
    let Some(account) = caller(&service, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match service.reply(account, ask).await {
        Ok(reply) => Json(serde_json::json!({"reply":reply})).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn stream(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(account) = caller(&service, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(permit) = service.sockets.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    upgrade
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |mut socket| async move {
            let _permit = permit;
            while let Some(Ok(message)) = socket.recv().await {
                let ask = match message {
                    Message::Text(text) => serde_json::from_str::<Ask>(&text),
                    Message::Binary(bytes) => serde_json::from_slice::<Ask>(&bytes),
                    Message::Close(_) => return,
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                let Ok(ask) = ask else { break };
                // Query current committed policy for each message, including a
                // socket opened before a membership change or module replacement.
                let Ok(reply) = service.reply(account, ask).await else {
                    break;
                };
                let message = Message::Text(serde_json::json!({"reply":reply}).to_string().into());
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(5), socket.send(message)).await,
                    Ok(Ok(()))
                ) {
                    return;
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), socket.send(Message::Close(None)))
                .await;
        })
}

fn inherited_listener() -> Result<std::net::TcpListener, Box<dyn std::error::Error>> {
    use std::os::fd::FromRawFd as _;
    let activated = std::env::var("LISTEN_FDS").as_deref() == Ok("1")
        && std::env::var("LISTEN_PID")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            == Some(std::process::id());
    if !activated {
        return Err("one inherited listener is required".into());
    }
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // Validate the inherited descriptor before taking its unique ownership.
    let result = unsafe {
        libc::getsockopt(
            3,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result != 0 || kind != libc::SOCK_STREAM {
        return Err("fd3 is not a stream socket".into());
    }
    // The supervisor gives this PID sole ownership of the checked descriptor.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("listener must use loopback".into());
    }
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// systemd considers the initialized process ready before publishing its route.
#[cfg(unix)]
fn notify_ready() -> std::io::Result<()> {
    use std::os::unix::{
        ffi::OsStrExt as _,
        net::{SocketAddr, UnixDatagram},
    };
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let bytes = path.as_bytes();
    let address = match bytes.strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        Some(name) => {
            use std::os::linux::net::SocketAddrExt as _;
            SocketAddr::from_abstract_name(name)?
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => {
            return Err(std::io::Error::other(
                "abstract notify socket requires Linux",
            ));
        }
        None => SocketAddr::from_pathname(path)?,
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address)?;
    Ok(())
}

#[cfg(not(unix))]
fn notify_ready() -> std::io::Result<()> {
    Err(std::io::Error::other("socket activation requires Unix"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory =
        PathBuf::from(std::env::var_os("CREDENTIALS_DIRECTORY").ok_or("missing credentials")?);
    let config: Config =
        serde_json::from_slice(&std::fs::read(directory.join("application.json"))?)?;
    let token: [u8; 64] = std::fs::read(directory.join("upstream-token"))?
        .try_into()
        .map_err(|_| "invalid credential length")?;
    let service = Arc::new(Service {
        config,
        token,
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?,
        sockets: Arc::new(tokio::sync::Semaphore::new(16)),
    });
    let router = Router::new()
        .route("/reply", post(request))
        .route("/stream", get(stream))
        .layer(DefaultBodyLimit::max(1024))
        .with_state(service);
    let listener = tokio::net::TcpListener::from_std(inherited_listener()?)?;
    notify_ready()?;
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-duck-upstream-token", "a".repeat(64)),
            ("x-duck-route-account", "1".into()),
            ("x-duck-route-label", "probe".into()),
            ("x-duck-route-revision", "1".into()),
            ("x-duck-caller-account", "9".into()),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        headers
    }

    fn service(node: String) -> Arc<Service> {
        Arc::new(Service {
            config: Config {
                node,
                module: "unlisted-policy".into(),
                account: 1,
                route: "probe".into(),
            },
            token: [b'a'; 64],
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            sockets: Arc::new(tokio::sync::Semaphore::new(16)),
        })
    }

    #[test]
    fn caller_requires_unique_credentials_and_exact_binding() {
        let service = service("http://127.0.0.1:1".into());
        let valid = headers();
        assert_eq!(caller(&service, &valid), Some(9));
        for name in [
            "x-duck-upstream-token",
            "x-duck-route-account",
            "x-duck-route-label",
            "x-duck-route-revision",
            "x-duck-caller-account",
        ] {
            let mut duplicate = valid.clone();
            duplicate.append(name, valid[name].clone());
            assert_eq!(caller(&service, &duplicate), None, "{name}");
            let mut missing = valid.clone();
            missing.remove(name);
            assert_eq!(caller(&service, &missing), None, "{name}");
        }
        for (name, value) in [
            ("x-duck-upstream-token", "b".repeat(64)),
            ("x-duck-route-account", "2".into()),
            ("x-duck-route-label", "other".into()),
            ("x-duck-route-revision", "0".into()),
            ("x-duck-caller-account", "0".into()),
        ] {
            let mut wrong = valid.clone();
            wrong.insert(name, value.parse().unwrap());
            assert_eq!(caller(&service, &wrong), None, "{name}");
        }
    }

    async fn policy(
        State(allowed): State<Arc<AtomicBool>>,
        Json(ask): Json<serde_json::Value>,
    ) -> Json<bool> {
        assert_eq!(ask["target"], "unlisted-policy");
        let member = ask["query"]["authorize"]["account"] == 9;
        Json(member && allowed.load(Ordering::Acquire))
    }

    #[tokio::test]
    async fn real_http_and_socket_recheck_guest_policy_and_enforce_limits() {
        let allowed = Arc::new(AtomicBool::new(true));
        let policy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node = format!("http://{}", policy_listener.local_addr().unwrap());
        let policy_router = Router::new()
            .route("/v1/query", post(policy))
            .with_state(allowed.clone());
        let policy_task =
            tokio::spawn(async move { axum::serve(policy_listener, policy_router).await.unwrap() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/reply", post(request))
            .route("/stream", get(stream))
            .layer(DefaultBodyLimit::max(1024))
            .with_state(service(node));
        let application = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{address}/reply");
        let success = client
            .post(&url)
            .headers(headers())
            .json(&serde_json::json!({"text":"Hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(success.status(), StatusCode::OK);
        let reply: serde_json::Value = success.json().await.unwrap();
        assert_eq!(
            reply["reply"],
            if cfg!(feature = "replacement") {
                "hello"
            } else {
                "HELLO"
            }
        );
        let unauthorized = client
            .post(&url)
            .json(&serde_json::json!({"text":"Hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let malformed = client
            .post(&url)
            .headers(headers())
            .json(&serde_json::json!({"text":"Hello","caller":9}))
            .send()
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let oversized = client
            .post(&url)
            .headers(headers())
            .json(&serde_json::json!({"text":"x".repeat(2048)}))
            .send()
            .await
            .unwrap();
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let mut upgrade = format!("ws://{address}/stream")
            .into_client_request()
            .unwrap();
        upgrade.headers_mut().extend(headers());
        let (mut socket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
        socket
            .send(WsMessage::Text(r#"{"text":"Hello"}"#.into()))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap().into_text().unwrap();
        assert!(reply.contains(if cfg!(feature = "replacement") {
            "hello"
        } else {
            "HELLO"
        }));
        allowed.store(false, Ordering::Release);
        socket
            .send(WsMessage::Binary(br#"{"text":"again"}"#.to_vec().into()))
            .await
            .unwrap();
        assert!(matches!(socket.next().await, Some(Ok(WsMessage::Close(_)))));
        let denied = client
            .post(&url)
            .headers(headers())
            .json(&serde_json::json!({"text":"Hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        application.abort();
        policy_task.abort();
    }
}
