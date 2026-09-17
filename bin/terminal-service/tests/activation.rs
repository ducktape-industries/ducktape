//! Actual socket activation, readiness failure, and process replacement.
#![cfg(unix)]
use std::{
    net::TcpListener,
    os::{
        fd::AsRawFd as _,
        unix::{net::UnixDatagram, process::CommandExt as _},
    },
    process::Command,
};

#[test]
fn unusable_sandbox_exits_without_announcing_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let notify_path = directory.path().join("notify.sock");
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    notify.set_nonblocking(true).unwrap();
    std::fs::write(directory.path().join("upstream-token"), "a".repeat(64)).unwrap();
    let absent = directory.path().join("absent");
    let config = serde_json::json!({
        "account":7, "label":"terminal", "identity":vec![1;32],
        "workspace":directory.path(), "node_api":"http://127.0.0.1:1",
        "capabilities":absent, "kernel":absent, "rootfs":absent,
        "executors":absent
    });
    std::fs::write(
        directory.path().join("application.json"),
        config.to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut command = activated_command(&listener, directory.path(), &notify_path);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("sandbox"), "{error}");
    let mut bytes = [0; 64];
    assert_eq!(
        notify.recv(&mut bytes).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(TcpListener::bind(listener.local_addr().unwrap()).is_err());
}

fn activated_command(
    listener: &TcpListener,
    directory: &std::path::Path,
    notify_path: &std::path::Path,
) -> Command {
    let fd = listener.as_raw_fd();
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "export LISTEN_PID=$$; exec \"$@\"",
            "terminal-activation",
            env!("CARGO_BIN_EXE_ducktape-terminal"),
        ])
        .env("CREDENTIALS_DIRECTORY", directory)
        .env("STATE_DIRECTORY", directory)
        .env("NOTIFY_SOCKET", notify_path)
        .env("LISTEN_FDS", "1");
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
    command
}

/// Uses the production binary and real sandbox prerequisites. It verifies
/// process activation/replacement, not an interactive microVM session.
#[tokio::test]
#[ignore = "requires Firecracker/KVM, DUCK_TERMINAL_GUEST_DIR, and DUCK_TERMINAL_EXECUTORS"]
async fn production_process_readiness_signal_shutdown_and_replacement() {
    use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest as _};
    let guest = std::path::PathBuf::from(
        std::env::var_os("DUCK_TERMINAL_GUEST_DIR").expect("guest image directory"),
    );
    let executors = std::path::PathBuf::from(
        std::env::var_os("DUCK_TERMINAL_EXECUTORS").expect("executor directory"),
    );
    let directory = tempfile::tempdir().unwrap();
    let notify_path = directory.path().join("notify.sock");
    let notify = tokio::net::UnixDatagram::bind(&notify_path).unwrap();
    let config = serde_json::json!({
        "account":7, "label":"terminal", "identity":vec![1;32],
        "workspace":directory.path(), "node_api":"http://127.0.0.1:1",
        "capabilities":directory.path().join("capabilities"),
        "kernel":guest.join("vmlinux"), "rootfs":guest.join("rootfs.ext4"),
        "executors":executors
    });
    std::fs::write(
        directory.path().join("application.json"),
        config.to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    for token in ['a', 'b'] {
        std::fs::write(
            directory.path().join("upstream-token"),
            token.to_string().repeat(64),
        )
        .unwrap();
        let command = activated_command(&listener, directory.path(), &notify_path);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut message = [0; 64];
        let ready = tokio::select! {
            result = notify.recv(&mut message) => result.unwrap(),
            status = child.wait() => panic!("service exited before readiness: {status:?}"),
        };
        assert_eq!(&message[..ready], b"READY=1");
        for (presented, expected) in [
            ('a', if token == 'a' { 403 } else { 401 }),
            ('b', if token == 'b' { 403 } else { 401 }),
        ] {
            let mut request = format!("ws://{address}/sessions/0000000000000001")
                .into_client_request()
                .unwrap();
            for (name, value) in [
                ("x-duck-upstream-token", presented.to_string().repeat(64)),
                ("x-duck-route-account", "7".into()),
                ("x-duck-route-label", "terminal".into()),
                ("x-duck-route-revision", "1".into()),
                ("x-duck-caller-account", "7".into()),
                ("x-duck-caller-node", "01".repeat(32)),
            ] {
                request.headers_mut().insert(name, value.parse().unwrap());
            }
            let Error::Http(response) =
                tokio_tungstenite::connect_async(request).await.unwrap_err()
            else {
                panic!("HTTP rejection expected");
            };
            assert_eq!(response.status().as_u16(), expected);
        }
        // Signal the exact child PID; never match or kill unrelated processes.
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
            0
        );
        assert!(child.wait().await.unwrap().success());
        assert!(
            TcpListener::bind(address).is_err(),
            "supervisor retains listener"
        );
    }
}
