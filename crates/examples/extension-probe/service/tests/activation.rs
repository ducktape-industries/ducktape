//! The actual executable consumes a held listener and keeps authentication
//! across replacement without exposing a port-rebinding window.
#![cfg(unix)]
use std::net::TcpListener;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};
use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest as _};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(listener: &TcpListener, credentials: &std::path::Path) -> Process {
    let notify_path = credentials.join("notify.sock");
    let _ = std::fs::remove_file(&notify_path);
    let notify = std::os::unix::net::UnixDatagram::bind(&notify_path).unwrap();
    notify
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let fd = listener.as_raw_fd();
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "export LISTEN_PID=$$; exec \"$@\"",
            "extension-activation",
            env!("CARGO_BIN_EXE_extension-service"),
        ])
        .env("NOTIFY_SOCKET", &notify_path)
        .env("LISTEN_FDS", "1")
        .env("CREDENTIALS_DIRECTORY", credentials);
    // Only async-signal-safe descriptor syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if fd != 3 && libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let process = Process(command.spawn().expect("start extension executable"));
    let mut message = [0; 64];
    let size = notify
        .recv(&mut message)
        .expect("service declares initialized readiness");
    assert_eq!(&message[..size], b"READY=1");
    process
}

async fn refusal(address: std::net::SocketAddr, token: &str, caller: bool) -> u16 {
    let mut request = format!("ws://{address}/stream")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-duck-upstream-token", token.parse().unwrap());
    if caller {
        for (name, value) in [
            ("x-duck-caller-account", "1".to_owned()),
            ("x-duck-caller-node", "b".repeat(64)),
            ("x-duck-route-account", "7".into()),
            ("x-duck-route-label", "probe".into()),
            ("x-duck-route-revision", "1".into()),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
    }
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .expect("process answers queued socket");
    match response {
        Err(Error::Http(response)) => response.status().as_u16(),
        _ => panic!("upgrade must refuse"),
    }
}

#[tokio::test]
async fn held_listener_survives_process_replacement_and_authentication_fails_closed() {
    let credentials = tempfile::tempdir().unwrap();
    std::fs::write(credentials.path().join("upstream-token"), "a".repeat(64)).unwrap();
    std::fs::write(
        credentials.path().join("application.json"),
        r#"{"node":"http://127.0.0.1:0","module":"probe","account":7,"route":"probe"}"#,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let first = start(&listener, credentials.path());
    assert_eq!(refusal(address, &"b".repeat(64), true).await, 401);
    assert_eq!(refusal(address, &"a".repeat(64), false).await, 401);
    drop(first);
    assert!(
        TcpListener::bind(address).is_err(),
        "supervisor retains the port while service is down"
    );
    let _replacement = start(&listener, credentials.path());
    assert_eq!(refusal(address, &"b".repeat(64), true).await, 401);
    assert_eq!(refusal(address, &"a".repeat(64), false).await, 401);
}
