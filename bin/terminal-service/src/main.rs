use std::path::PathBuf;

#[cfg(unix)]
fn inherited_listener() -> Result<std::net::TcpListener, Box<dyn std::error::Error>> {
    use std::os::fd::FromRawFd as _;

    let activated = std::env::var("LISTEN_FDS").as_deref() == Ok("1")
        && std::env::var("LISTEN_PID")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            == Some(std::process::id());
    if !activated {
        return Err("expected exactly one socket-activated listener".into());
    }
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // Check fd3 before taking ownership: activation environment variables
    // alone cannot make an arbitrary inherited file a TCP listener.
    let socket = unsafe {
        libc::getsockopt(
            3,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if socket != 0 || kind != libc::SOCK_STREAM {
        return Err("activation descriptor is not a stream socket".into());
    }
    require_listening(3)?;
    // systemd passes this valid owned descriptor exactly once, to this PID.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("activation listener must be TCP loopback".into());
    }
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(unix)]
fn require_listening(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let mut accepting: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&accepting) as libc::socklen_t;
    // Borrow the descriptor for inspection; ownership remains with the caller.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut accepting as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if accepting != 1 {
        return Err(std::io::Error::other("activation socket is not listening"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn inherited_listener() -> Result<std::net::TcpListener, Box<dyn std::error::Error>> {
    Err("socket activation requires Unix".into())
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    workspace: PathBuf,
    node_api: String,
    account: u64,
    label: String,
    // Public execution identity, used for provider labels; never a private key.
    identity: [u8; 32],
    capabilities: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
    executors: PathBuf,
}

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let directory = PathBuf::from(
        std::env::var_os("CREDENTIALS_DIRECTORY").ok_or("missing credentials directory")?,
    );
    let token: [u8; 64] = std::fs::read(directory.join("upstream-token"))?
        .try_into()
        .map_err(|_| "invalid upstream credential length")?;
    let canonical_token = token
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    if !canonical_token {
        return Err("invalid upstream credential".into());
    }
    let config: Config =
        serde_json::from_slice(&std::fs::read(directory.join("application.json"))?)?;
    let state =
        PathBuf::from(std::env::var_os("STATE_DIRECTORY").ok_or("missing state directory")?);
    let paths_absolute = [
        &state,
        &config.workspace,
        &config.capabilities,
        &config.kernel,
        &config.rootfs,
        &config.executors,
    ]
    .into_iter()
    .all(|path| path.is_absolute());
    if !paths_absolute {
        return Err("service paths must be absolute".into());
    }
    let listener = tokio::net::TcpListener::from_std(inherited_listener()?)?;
    let backend = provider_host::SandboxBackend::MicroVm {
        vmm: provider_host::Vmm::platform_default(),
        kernel: config.kernel,
        rootfs: config.rootfs,
        executors: config.executors,
    };
    backend
        .probe()
        .map_err(|error| format!("sandbox unavailable: {error}"))?;
    let identity = provider_host::execution_node_id(&config.identity);
    let owner = format!("terminal#{}:{}", config.account, config.label);
    let providers =
        agent_service::discover(&config.identity, &config.capabilities, backend, &owner)?;
    if providers.capabilities().is_empty() {
        return Err("no terminal providers available".into());
    }
    std::fs::create_dir_all(&state)?;
    let (runtime, driver) =
        ducktape_terminal::runtime::Runtime::start(providers, identity, state.join("sessions"));
    supervise(
        runtime.clone(),
        driver,
        serve(
            listener,
            ducktape_terminal::http::Route {
                node: config.identity,
                account: config.account,
                label: config.label,
                workspace: config.workspace,
                node_api: config.node_api,
            },
            token,
            runtime,
        ),
    )
    .await
}

async fn supervise(
    runtime: ducktape_terminal::runtime::Runtime,
    mut driver: tokio::task::JoinHandle<Result<(), String>>,
    service: impl std::future::Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    tokio::select! {
        result = service => {
            // All server exits drain executor teardown, including readiness and
            // listener failures. Dropping HTTP alone would leave PTYs running.
            let stopped = runtime.stop().await;
            let finished = driver.await?;
            result?;
            stopped?;
            finished?;
            Ok(())
        }
        result = &mut driver => {
            result??;
            Err("terminal runtime stopped unexpectedly".into())
        }
    }
}

#[cfg(unix)]
async fn serve(
    listener: tokio::net::TcpListener,
    route: ducktape_terminal::http::Route,
    token: [u8; 64],
    runtime: ducktape_terminal::runtime::Runtime,
) -> Result<(), Error> {
    use std::future::IntoFuture as _;
    use tokio::signal::unix::{SignalKind, signal};
    // Install both handlers before READY so the supervisor can stop us as soon
    // as it observes readiness without bypassing PTY cleanup.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let router = ducktape_terminal::http::router(route, token, runtime)?;
    let server = axum::serve(listener, router).into_future();
    tokio::pin!(server);
    notify_ready()?;
    tokio::select! {
        result = &mut server => { result?; }
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn serve(
    _: tokio::net::TcpListener,
    _: ducktape_terminal::http::Route,
    _: [u8; 64],
    _: ducktape_terminal::runtime::Runtime,
) -> Result<(), Error> {
    Err("socket activation requires Unix".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn activation_rejects_a_connected_socket_before_readiness() {
        use std::os::fd::AsRawFd as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let connected = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(require_listening(connected.as_raw_fd()).is_err());
        assert!(require_listening(listener.as_raw_fd()).is_ok());
    }

    #[tokio::test]
    async fn executor_exit_stops_an_otherwise_listening_service() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, driver) = ducktape_terminal::runtime::Runtime::start(
            provider_host::ProviderSet::empty(),
            "test".into(),
            directory.path().into(),
        );
        runtime.stop().await.unwrap();
        let error = supervise(runtime, driver, std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("runtime stopped unexpectedly"));
    }

    #[tokio::test]
    async fn failed_server_start_stops_the_executor() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, driver) = ducktape_terminal::runtime::Runtime::start(
            provider_host::ProviderSet::empty(),
            "test".into(),
            directory.path().into(),
        );
        let remaining = runtime.clone();
        let error = supervise(runtime, driver, async { Err("readiness failed".into()) })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "readiness failed");
        assert!(remaining.stop().await.is_err());
    }
}
