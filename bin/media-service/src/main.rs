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
    // systemd passes this valid owned descriptor exactly once, to this PID.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("activation listener must be TCP loopback".into());
    }
    listener.set_nonblocking(true)?;
    Ok(listener)
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        std::env::var_os("CREDENTIALS_DIRECTORY").ok_or("missing credentials directory")?,
    );
    let token: [u8; 64] = std::fs::read(directory.join("upstream-token"))?
        .try_into()
        .map_err(|_| "upstream credential must be 64 lowercase hex bytes")?;
    let canonical_token = token
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    if !canonical_token {
        return Err("invalid upstream credential".into());
    }
    let config = serde_json::from_slice(&std::fs::read(directory.join("application.json"))?)?;
    let router = ducktape_media::router(config, token)?;
    let listener = tokio::net::TcpListener::from_std(inherited_listener()?)?;
    notify_ready()?;
    axum::serve(listener, router).await?;
    Ok(())
}
