//! Boot failures must not publish readiness for an unusable terminal service.
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
        "capabilities":absent, "kernel":absent, "rootfs":absent,
        "executors":absent
    });
    std::fs::write(
        directory.path().join("application.json"),
        config.to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = listener.as_raw_fd();
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "export LISTEN_PID=$$; exec \"$@\"",
            "terminal-activation",
            env!("CARGO_BIN_EXE_ducktape-terminal"),
        ])
        .env("CREDENTIALS_DIRECTORY", directory.path())
        .env("STATE_DIRECTORY", directory.path())
        .env("NOTIFY_SOCKET", &notify_path)
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
