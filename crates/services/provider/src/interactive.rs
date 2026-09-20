//! interactive, pty-backed CLI sessions — the same executor a headless run
//! spawns, but attached to a pseudo-terminal so a human can drive its native
//! TUI (codex, claude) keystroke-by-keystroke instead of feeding it one prompt
//! on stdin.
//!
//! This shares the headless path's whole isolation seam — the broker holds the
//! credential and the child gets only an opaque bearer + loopback base URL, the
//! fresh config home stops any dotfile fallback, and the microVM fences
//! everything else — and differs in exactly two places: the argv is the spec's
//! `[interactive]` TUI argv (not `[invoke]`'s headless one), and the child's
//! stdio is a pty, not pipes.
//!
//! **The pty is allocated where the child runs.** A pty master and its slave
//! are two ends of ONE kernel object, so a session inside a guest cannot be
//! given a terminal from here: `duck-guest-init` opens the pair, and the
//! operator's keystrokes reach it as ordinary stdin frames. That is the whole
//! of [`Transport`]'s two variants — the operator's own vendor login runs on
//! this host and gets a host pty; every lent session runs in a guest and gets a
//! guest one.
//!
//! **Sandboxed only.** A bare spawn would have no fresh HOME and no filesystem
//! fence, so an interactive session outside the sandbox would expose the
//! operator's whole home to whoever is typing — the exact thing the sandbox
//! exists to prevent, and exactly why [`crate::SandboxBackend`] cannot express
//! one; the host-pty primitive underneath
//! ([`InteractiveSession::spawn_on_pty`]) is reachable on its own only for that
//! vendor login.
//!
//! There is deliberately NO idle-timeout kill here (a terminal is idle by
//! nature): a session ends on explicit [`InteractiveSession::close`], on the
//! child exiting (the master read returns EIO → EOF), or when dropped. Spend is
//! bounded by the broker's own request/byte caps, not by output silence.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex;

use crate::broker::RunBroker;
use crate::microvm;
use crate::sandbox::SandboxBackend;
use crate::{
    BrokerKind, CliProvider, LiveChild, RunAuth, RunContext, RunHome, broker_provider_overrides,
    canonical_mount_path, configure_process_group,
};

/// how a live interactive session carries the terminal.
///
/// Two shapes, because the terminal is allocated in two different kernels. The
/// operator's own vendor login runs on THIS host, so its pty is a pty and this
/// process holds the master. A lent session runs in a guest, where the master
/// is unreachable from here — so the terminal bytes ride the run's ordinary
/// stdio frames and the GUEST is what makes them a terminal.
///
/// A `match` everywhere, no wildcard: the next transport must fail the build
/// until every arm routes it.
enum Transport {
    /// a run inside a microVM, whose `duck-guest-init` allocated the pty.
    ///
    /// Both variants are boxed, and symmetrically: each carries a live child's
    /// worth of state, and inlining either would size every `Transport` — and
    /// so every `InteractiveSession` — at the larger of the two.
    MicroVm(Box<GuestTerminal>),
    Pty(Box<HostPty>),
}

/// a live session's half of a pty on THIS host.
struct HostPty {
    /// the master's read half (a dup of the pty master, non-blocking).
    reader: AsyncFd<OwnedFd>,
    /// the master's write half (a second dup of the same master).
    writer: AsyncFd<OwnedFd>,
    /// the child, behind a lock so `close` can tear it down through `&self`.
    live: Mutex<LiveChild>,
}

/// a live session's half of a booted microVM.
struct GuestTerminal {
    /// terminal output, behind its OWN lock: a read blocks for as long as the
    /// terminal is quiet, and a shared lock would make that block the next
    /// keystroke too.
    output: Mutex<tokio::io::DuplexStream>,
    /// keystrokes.
    input: Mutex<tokio::io::DuplexStream>,
    /// window size, under NO lock: [`InteractiveSession::resize`] is
    /// synchronous (the terminal plane's resize handler is), so it has no lock
    /// to await.
    resize: microvm::ResizeLane,
    /// held for the session, never read — see [`microvm::TerminalIo`].
    _idle_stderr: tokio::io::DuplexStream,
    _pump: tokio::task::JoinHandle<()>,
    /// resolves when the guest reports the child's exit; a watch rather than
    /// the guest's oneshot so [`InteractiveSession::wait_child_exit`] can be
    /// awaited more than once.
    exited: tokio::sync::watch::Receiver<bool>,
    /// the VM itself, taken on close so the workspace is read back exactly
    /// once.
    vm: Mutex<Option<microvm::MicroVm>>,
    /// where the session's workspace is read back TO.
    workdir: std::path::PathBuf,
}

/// a live interactive session. Every method takes `&self`, so the owner can wrap
/// it in an `Arc` and pump output in one task while feeding input from another.
pub struct InteractiveSession {
    transport: Transport,
    /// held for the session's lifetime: dropping the broker tears its endpoint
    /// down, and dropping the config home REMOVES it — declared last, so the
    /// child that reads it is already gone by then. a session's transcripts and
    /// credentials file do not outlive the session in a workdir a later run
    /// (another account's, since #843) will mount.
    _broker: Option<RunBroker>,
    _config_home: Option<RunHome>,
    /// this session's guest tunnels the crate terminates (its node lane, its
    /// egress proxy), which die with the session. `None` for the operator's
    /// local vendor-login pty run, which is not a guest and gets no tunnel.
    _lanes: Option<crate::GuestLanes>,
}

impl InteractiveSession {
    /// adopt a booted microVM whose guest allocated the pty.
    ///
    /// The guest's exit oneshot is converted to a watch here, once: a session's
    /// exit is asked about by more than one caller (the plane that renders it
    /// and the reaper that tears it down), and a oneshot answers once.
    fn from_microvm(
        vm: microvm::MicroVm,
        io: microvm::TerminalIo,
        workdir: std::path::PathBuf,
        broker: Option<RunBroker>,
        config_home: Option<RunHome>,
        lanes: crate::GuestLanes,
    ) -> Self {
        let microvm::TerminalIo {
            output,
            input,
            exit,
            resize,
            idle_stderr,
            pump,
        } = io;
        let (exited_tx, exited) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let _ = exit.await;
            let _ = exited_tx.send(true);
        });
        Self {
            transport: Transport::MicroVm(Box::new(GuestTerminal {
                output: Mutex::new(output),
                input: Mutex::new(input),
                resize,
                _idle_stderr: idle_stderr,
                _pump: pump,
                exited,
                vm: Mutex::new(Some(vm)),
                workdir,
            })),
            _broker: broker,
            _config_home: config_home,
            _lanes: Some(lanes),
        }
    }

    /// spawn `command` with its stdio wired to a fresh pty and keep the master.
    /// The pty transport backs the operator's local vendor-login run; a lent
    /// session uses [`Self::from_microvm`] instead.
    fn spawn_on_pty(
        mut command: tokio::process::Command,
        broker: Option<RunBroker>,
        config_home: Option<RunHome>,
    ) -> Result<Self, String> {
        let (master, slave) = open_pty()?;
        set_nonblocking(&master)?;
        let writer_fd = master
            .try_clone()
            .map_err(|e| format!("dup pty master: {e}"))?;
        let stdin = slave
            .try_clone()
            .map_err(|e| format!("dup pty slave: {e}"))?;
        let stdout = slave
            .try_clone()
            .map_err(|e| format!("dup pty slave: {e}"))?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave))
            .kill_on_drop(true);
        configure_process_group(&mut command);
        let child = command
            .spawn()
            .map_err(|e| format!("spawn interactive session: {e}"))?;
        let live = LiveChild::new(child);
        let reader = AsyncFd::new(master).map_err(|e| format!("register pty master: {e}"))?;
        let writer = AsyncFd::new(writer_fd).map_err(|e| format!("register pty master: {e}"))?;
        Ok(Self {
            transport: Transport::Pty(Box::new(HostPty {
                reader,
                writer,
                live: Mutex::new(live),
            })),
            _broker: broker,
            _config_home: config_home,
            _lanes: None,
        })
    }

    /// spawn `command` on a pty with NO sandbox, broker, or fresh config home —
    /// the host runs it directly on this box. The ONLY intended caller is the
    /// operator's own `ducktape user cred add` vendor-login wrap (there is
    /// nothing to isolate — the operator's own credential on the operator's own
    /// machine). Every lent agent session goes through
    /// [`CliProvider::spawn_interactive_session`], which keeps isolation.
    pub fn spawn_local(command: tokio::process::Command) -> Result<Self, String> {
        Self::spawn_on_pty(command, None, None)
    }

    /// read the next chunk of terminal output. `Ok(0)` means end of session.
    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &self.transport {
            Transport::Pty(pty) => Self::pty_read(&pty.reader, buf).await,
            // the guest's pump closes this half when the guest's own pty read
            // ends, so `Ok(0)` arrives without translating anything.
            Transport::MicroVm(guest) => guest.output.lock().await.read(buf).await,
        }
    }

    /// the pty read path: on Linux the master read returns EIO once the last
    /// slave (the child) is gone, which we map to EOF.
    async fn pty_read(reader: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = reader.readable().await?;
            let res = guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                // SAFETY: fd is our live master pty; buf is valid for `buf.len()`.
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EIO) {
                        return Ok(0);
                    }
                    Err(e)
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// write input (keystrokes) to the terminal, in full.
    pub async fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        match &self.transport {
            Transport::Pty(pty) => Self::pty_write(&pty.writer, data).await,
            Transport::MicroVm(guest) => guest.input.lock().await.write_all(data).await,
        }
    }

    async fn pty_write(writer: &AsyncFd<OwnedFd>, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = writer.writable().await?;
            let res = guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                // SAFETY: fd is our live master pty; data is valid for `data.len()`.
                let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// resize the terminal, SIGWINCHing whatever is drawing on it.
    ///
    /// Sync, because the terminal plane's resize handler is. A host pty is an
    /// ioctl on the master; a guest pty is a frame the guest applies to its
    /// own master, queued without blocking.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        match &self.transport {
            Transport::Pty(pty) => {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                let fd = pty.reader.get_ref().as_raw_fd();
                // SAFETY: fd is our live master pty; &ws is a valid winsize.
                let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
            Transport::MicroVm(guest) => {
                guest.resize.resize(cols, rows);
                Ok(())
            }
        }
    }

    /// Resolve once the run has exited, without tearing it down (the teardown
    /// stays with [`Self::close`]). For a pty, this observes the child leader's
    /// exit even while a lingering grandchild holds the slave (so it does not
    /// hang the way read-until-EOF would); for a socket, it waits the container.
    pub async fn wait_child_exit(&self) {
        match &self.transport {
            Transport::Pty(pty) => {
                let Some(pid) = pty.live.lock().await.leader_pid() else {
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                crate::wait_leader_exit_unreaped(pid, "interactive session").await;
            }
            // the guest reports its own child's exit; the VMM's status would
            // only say whether the hypervisor exited cleanly.
            Transport::MicroVm(guest) => {
                let mut exited = guest.exited.clone();
                while !*exited.borrow_and_update() {
                    // the sender lives as long as the watching task, so an
                    // error here means the guest is gone without reporting —
                    // which is an exit too.
                    if exited.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// end the session: terminate the child, or kill the VM and walk its
    /// workspace back to the host. Idempotent — safe to call on teardown after
    /// the session already ended on its own.
    pub async fn close(&self) {
        match &self.transport {
            Transport::Pty(pty) => pty.live.lock().await.terminate().await,
            Transport::MicroVm(guest) => {
                // taken, so a second close is a no-op rather than a second
                // read-back over a workspace the first one already wrote.
                let Some(mut vm) = guest.vm.lock().await.take() else {
                    return;
                };
                vm.terminate().await;
                if let Err(e) = vm.collect(&guest.workdir).await {
                    tracing::warn!(
                        target: "ducktape::compute",
                        event = "session_workspace_read_back_failed",
                        reason = "collect_failed",
                        error = %e,
                        "an interactive session's workspace did not come back"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    fn window_size(&self) -> (u16, u16) {
        // Reads the HOST pty's master directly, so it only answers for that
        // transport. A guest's master is on the other side of a hypervisor —
        // there is no ioctl to make here, and reporting some other number would
        // be worse than refusing.
        let Transport::Pty(pty) = &self.transport else {
            panic!("window_size is only meaningful for a host pty transport");
        };
        // SAFETY: zeroed winsize is valid; ioctl fills it from the master pty.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let fd = pty.reader.get_ref().as_raw_fd();
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        assert_eq!(
            rc,
            0,
            "TIOCGWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
        (ws.ws_col, ws.ws_row)
    }
}

impl CliProvider {
    /// spawn this capability's interactive TUI on a pty, inside the provider's
    /// sandbox backend, from a spec with an `[interactive]`
    /// argv. The isolation is
    /// the headless path's (a broker holding the credential, a fresh config
    /// home, the VM fence); only the argv and the stdio (a guest pty, not
    /// pipes) differ. The VM's lifetime is the session's: it is torn down by
    /// [`InteractiveSession::close`], which is also what reads the workspace
    /// back.
    pub(crate) async fn spawn_interactive_session(
        &self,
        ctx: &RunContext,
        restricted: bool,
    ) -> Result<InteractiveSession, String> {
        let Some(interactive) = self.spec.interactive.clone() else {
            return Err(format!(
                "{}: this capability declares no [interactive] argv",
                self.spec.tag
            ));
        };
        // a restricted (shared/command-lane) session runs the read-only,
        // non-prompting argv; a capability that declares none does not support
        // shared mode, and we refuse rather than run it unrestricted.
        let base = if restricted {
            match interactive.restricted_args.as_deref() {
                Some(a) => a,
                None => {
                    return Err(format!(
                        "{}: this capability declares no restricted [interactive] argv \
                         (shared/command-lane session unsupported)",
                        self.spec.tag
                    ));
                }
            }
        } else {
            &interactive.args
        };
        let workdir = self.ensure_writable_workdir(ctx)?;
        let workdir = canonical_mount_path(&workdir, "sandbox workdir")?;
        let home = self.prepare_config_home(&workdir)?;
        // the per-run credential source: a peer-attached session carries a
        // consensus-resolved self-host gateway on `ctx.airlock`, so the broker
        // resolves the upstream to it instead of the boundary env; a local
        // session leaves it `None` and the env/host-credential path is unchanged.
        let broker = self.start_broker(ctx.airlock.as_ref()).await?;
        let auth = RunAuth {
            config_home: home.as_ref().map(RunHome::config),
            broker: broker.as_ref().map(|b| &b.endpoint),
        };
        let args = interactive_argv(base, &auth, &workdir, self.spec.isolation.broker);

        match &self.backend {
            SandboxBackend::MicroVm { .. } => {
                let (vm, io, lanes) = self
                    .microvm_boot(&args, &workdir, ctx, &auth, crate::GuestStdio::Pty)
                    .await?;
                Ok(InteractiveSession::from_microvm(
                    vm,
                    io.into_terminal(),
                    workdir,
                    broker,
                    home,
                    lanes,
                ))
            }
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => {
                unreachable!("interactive sessions never run under the bare test harness")
            }
        }
    }
}

/// the interactive TUI argv. For CODEX, the broker's `-c` overrides are
/// PREPENDED (a TUI argv has no `exec` subcommand to splice them after, and
/// codex reads `-c` as global config). For the Anthropic broker (claude) the
/// aiming is entirely by ENV (see [`CliProvider::apply_auth_env`]), so the base
/// argv passes through verbatim — as it does when there is no broker at all.
fn interactive_argv(
    base: &[String],
    auth: &RunAuth<'_>,
    workdir: &Path,
    kind: Option<BrokerKind>,
) -> Vec<String> {
    let (Some(broker), Some(BrokerKind::CodexResponses)) = (auth.broker, kind) else {
        return base.to_vec();
    };
    let mut argv = broker_provider_overrides(broker, workdir);
    argv.extend(base.iter().cloned());
    argv
}

/// allocate a pseudo-terminal via the POSIX path (`posix_openpt` + `grantpt` +
/// `unlockpt` + a platform slave-name lookup + `open`), so this needs no
/// `libutil` link the way `openpty` would. Returns `(master, slave)`.
fn open_pty() -> Result<(OwnedFd, OwnedFd), String> {
    // SAFETY: posix_openpt allocates a master pty; O_NOCTTY keeps it from
    // becoming this process's controlling terminal.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `master` is a fresh fd we now own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let mfd = master.as_raw_fd();
    // SAFETY: mfd is a live master pty for the two setup ioctls.
    if unsafe { libc::grantpt(mfd) } != 0 {
        return Err(format!("grantpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::unlockpt(mfd) } != 0 {
        return Err(format!("unlockpt: {}", std::io::Error::last_os_error()));
    }
    let mut name = [0 as libc::c_char; 256];
    pty_slave_name(mfd, &mut name)?;
    // SAFETY: `name` is NUL-terminated by the platform slave-name lookup.
    let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(format!(
            "open pty slave: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `slave` is a fresh fd we now own.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    // Give the pty a sane INITIAL size (80x24). A pty is created at 0x0; a TUI
    // handed 0x0 renders blank until
    // the first resize — and the app's first resize can be dropped while its ws
    // is still connecting. The real client geometry replaces this via `resize`.
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: mfd is a live master pty; &ws is a valid winsize.
    unsafe { libc::ioctl(mfd, libc::TIOCSWINSZ, &ws) };
    Ok((master, slave))
}

#[cfg(target_os = "macos")]
fn pty_slave_name(mfd: libc::c_int, name: &mut [libc::c_char]) -> Result<(), String> {
    // macOS has no ptsname_r. TIOCPTYGNAME is its thread-safe kernel interface
    // for copying the slave path into a caller-owned MAXPATHLEN-sized buffer.
    // SAFETY: mfd is a live master pty and `name` provides 256 writable bytes,
    // the buffer size encoded by TIOCPTYGNAME.
    if unsafe { libc::ioctl(mfd, libc::TIOCPTYGNAME.into(), name.as_mut_ptr()) } != 0 {
        return Err(format!(
            "ioctl TIOCPTYGNAME: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn pty_slave_name(mfd: libc::c_int, name: &mut [libc::c_char]) -> Result<(), String> {
    // SAFETY: `name` is a writable buffer; ptsname_r writes the NUL-terminated
    // slave path into it.
    if unsafe { libc::ptsname_r(mfd, name.as_mut_ptr(), name.len()) } != 0 {
        return Err(format!("ptsname_r: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// mark `fd` non-blocking (`O_NONBLOCK` on the open file description, which its
/// dups share) so [`AsyncFd`] can drive it.
fn set_nonblocking(fd: &OwnedFd) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    // SAFETY: raw is a live fd we own.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "fcntl F_GETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: raw is a live fd we own; setting O_NONBLOCK on its status flags.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "fcntl F_SETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the pty primitive pumps bytes both ways: write to the master, the child
    /// (`cat`) echoes it back, we read it. Proves openpty + AsyncFd read/write
    /// without needing a VM or a logged-in CLI. (`cat` on a pty also sees the
    /// line-discipline echo, so the payload is guaranteed to come back.)
    #[tokio::test]
    async fn pty_round_trips_bytes_through_a_child() {
        let session =
            InteractiveSession::spawn_on_pty(tokio::process::Command::new("cat"), None, None)
                .expect("spawn cat on a pty");
        session.write_all(b"ping\n").await.expect("write to pty");

        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        // read until the payload appears or the child is gone.
        for _ in 0..20 {
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), session.read(&mut buf))
                .await
                .expect("read did not hang")
                .expect("read from pty");
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"ping") {
                break;
            }
        }
        session.close().await;
        assert!(
            seen.windows(4).any(|w| w == b"ping"),
            "expected the pty to echo 'ping', got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// `wait_child_exit` resolves when the child LEADER exits even though a
    /// lingering grandchild still holds the pty slave open — the exact shape that
    /// hangs a plain `read`-until-EOF wrap (a vendor login that forks a helper
    /// and exits). `sh` backgrounds a long `sleep` (which inherits the slave),
    /// prints, then exits: the master never EOFs, but `wait_child_exit` returns.
    #[tokio::test]
    async fn wait_child_exit_returns_while_a_grandchild_holds_the_pty() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo ready"]);
        let session = InteractiveSession::spawn_on_pty(cmd, None, None).expect("spawn sh on a pty");

        // The child leader (sh) exits right after `echo ready`; the backgrounded
        // sleep keeps the slave open, so this must still complete promptly.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            session.wait_child_exit(),
        )
        .await
        .expect("wait_child_exit hung while a grandchild held the pty");

        // Drain any buffered output; the pty must reach a BLOCK (no more data),
        // never EOF — the sleep still holds a slave, so a read-until-EOF wrap
        // would hang here even though wait_child_exit already returned.
        let mut buf = [0u8; 256];
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(300),
                session.read(&mut buf),
            )
            .await
            {
                Err(_) => break, // read blocked: pty open, no EOF — the point.
                Ok(Ok(0)) => panic!("pty EOF'd — the grandchild did not hold it open"),
                Ok(Ok(_)) => continue, // drained a chunk (e.g. "ready"), keep going.
                Ok(Err(e)) => panic!("read errored: {e}"),
            }
        }

        // close() terminates the process group, reaping the sleep and freeing it.
        session.close().await;
    }

    /// resize sets the master's window size; the kernel reflects it back through
    /// TIOCGWINSZ. Pure ioctl round-trip — no VM.
    #[tokio::test]
    async fn resize_sets_the_window_size() {
        let session =
            InteractiveSession::spawn_on_pty(tokio::process::Command::new("cat"), None, None)
                .expect("spawn cat on a pty");
        session.resize(120, 40).expect("resize");
        assert_eq!(session.window_size(), (120, 40));
        session.close().await;
    }

    /// the broker overrides are PREPENDED for a CODEX interactive (TUI) argv
    /// (no `exec` selector to splice after); every other case passes the base
    /// argv through verbatim.
    #[test]
    fn interactive_argv_prepends_only_for_codex() {
        // no broker → base argv verbatim.
        let bare = interactive_argv(
            &["--foo".to_string()],
            &RunAuth::default(),
            Path::new("/w"),
            None,
        );
        assert_eq!(bare, vec!["--foo".to_string()]);

        let endpoint = crate::broker::BrokerEndpoint {
            kind: crate::CredentialKind::Codex,
            base_url: "http://127.0.0.1:9/v1".into(),
            run_bearer: "b".into(),
            control_url: String::new(),
            control_token: String::new(),
        };
        let auth = RunAuth {
            config_home: None,
            broker: Some(&endpoint),
        };
        // codex → overrides prepended.
        let codex = interactive_argv(
            &["--foo".to_string()],
            &auth,
            Path::new("/w"),
            Some(BrokerKind::CodexResponses),
        );
        assert_eq!(codex.first().map(String::as_str), Some("-c"));
        assert_eq!(codex.last().map(String::as_str), Some("--foo"));

        // claude (Anthropic) → base argv verbatim, aiming is by ENV not argv.
        let claude = interactive_argv(
            &["--foo".to_string()],
            &auth,
            Path::new("/w"),
            Some(BrokerKind::AnthropicMessages),
        );
        assert_eq!(claude, vec!["--foo".to_string()]);
    }

    #[tokio::test]
    async fn pi_shared_session_is_refused_before_starting_any_process() {
        let spec = crate::spec::builtin_specs()
            .into_iter()
            .find(|spec| spec.tag == "pi")
            .unwrap();
        let provider = CliProvider::from_spec(spec, "/nonexistent/pi".into(), SandboxBackend::Bare);
        let result = provider
            .spawn_interactive_session(&RunContext::default(), true)
            .await;
        let Err(error) = result else {
            panic!("Pi must not advertise a shared read-only session");
        };
        assert!(
            error.contains("shared/command-lane session unsupported"),
            "{error}"
        );
    }

    // ---- where the live coverage lives --------------------------------------
    //
    // Not here. The sandboxed session is a two-kernel object — a guest
    // allocates the pty, the host holds neither end of it — so there is nothing
    // meaningful to assert without booting a real VM, and this crate's default
    // lane is 85 tests that need nothing at all.
    //
    // `bin/node/tests/remote_session.rs` drives the whole thing end to end
    // through a real microVM (a guest node directs, the host boots the VM, a
    // keystroke crosses the forwarded lane, and the child's echo fans back onto
    // the directing node's topic), in a suite that runs by default on a
    // provisioned box.
}
