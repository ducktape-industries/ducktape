//! one run, one microVM: boot it, carry its stdio, read its workspace back.
//!
//! This is the seat a container runtime's create/start/attach/wait held, and it is
//! smaller because a VM needs no daemon: the VMM is a child of this process and
//! dies with the run. There is nothing to reap, no socket to keep alive between
//! runs, and no image store to garbage-collect.
//!
//! The whole lifecycle in order:
//!
//! 1. build the run's workspace into an ext4 image ([`crate::workspace_image`])
//! 2. listen on the vsock socket the guest will dial — BEFORE the VMM starts
//! 3. spawn `firecracker --no-api --config-file`
//! 4. accept the guest, then pump frames both ways
//! 5. on the guest's exit frame, wait for the VMM and read the image back
//!
//! Step 2 is ordered deliberately. Firecracker connects the guest's outbound
//! vsock to `<uds_path>_<port>` on the host; if nothing is listening there when
//! the guest dials, the guest's connect fails and the run produces no output at
//! all — a silence that looks exactly like a hung CLI.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::firecracker_api::{self, VmConfig};
use crate::guest_manifest::RunManifest;
use crate::guest_proto::{self, Frame};

/// how long to wait for the guest to dial back after the VMM starts.
///
/// This is a BOOT budget, not a run budget: the measured cold boot on the
/// development host is 452 ms tuned, and the guest dials immediately after
/// mounting. A guest that has not connected in this long is not slow, it is
/// broken — a missing init, an unmountable workspace, a kernel panic — and the
/// useful thing to do is fail with the VMM's console output rather than wait.
const GUEST_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// a run's outstanding guest tunnel connections, past which an accepted
/// socket is dropped rather than spliced.
///
/// Shared across every tunnel a run has, not per-port: the guest decides how
/// many connections to hold and nothing on the guest side costs it anything
/// to hold one open, so the ceiling has to be the host's. See #1873.
const MAX_TUNNEL_CONNECTIONS: usize = 64;

/// a spliced connection with no bytes moved in EITHER direction for this
/// long is closed. The deadline is for the connection as a whole, not each
/// direction on its own: a request that goes quiet once its body ends must
/// not cut off a response still streaming the other way. Without this at
/// all, a guest that stops talking (or a service that stops answering) holds
/// its two fds and its permit forever for free.
const TUNNEL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// a live run's stdio, shaped like the pipes an ordinary child would give, so
/// the caller's output loop does not care which backend produced them.
pub struct MicroVmIo {
    pub stdin: tokio::io::DuplexStream,
    pub stdout: tokio::io::DuplexStream,
    pub stderr: tokio::io::DuplexStream,
    /// the guest's own exit frame. Distinct from the VMM process's status,
    /// which only says whether the hypervisor exited cleanly.
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    pub pump: tokio::task::JoinHandle<()>,
    /// every host→guest frame goes out through here, which is why the resize
    /// lane exists at all: ONE task owns the socket's write half (it must, see
    /// [`pump_frames`]), so a second writer would have to be a second owner.
    input: tokio::sync::mpsc::Sender<Frame>,
}

impl MicroVmIo {
    /// take this io apart into the halves a TERMINAL session drives
    /// independently.
    ///
    /// Separate owners on purpose: the terminal plane pumps output in one task
    /// while feeding keystrokes from another, so a single lock over both would
    /// let a quiet terminal block the next keystroke indefinitely.
    pub fn into_terminal(self) -> TerminalIo {
        TerminalIo {
            output: self.stdout,
            input: self.stdin,
            exit: self.exit,
            resize: ResizeLane(self.input),
            idle_stderr: self.stderr,
            pump: self.pump,
        }
    }
}

/// one interactive session's io: what to read, what to write, how to resize,
/// and when the guest's child exited.
///
/// Every field is public and separately owned, because a terminal session wants
/// them in different places at once — output under one lock, input under
/// another, and resize under NO lock (the caller notices a window change on a
/// synchronous path and cannot await one).
pub struct TerminalIo {
    /// terminal output. A pty merges the child's stderr into it, so this is the
    /// whole of what the session renders.
    pub output: tokio::io::DuplexStream,
    /// keystrokes, as ordinary stdin bytes. The guest writes them into the pty
    /// master, which is what makes them terminal input.
    pub input: tokio::io::DuplexStream,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    pub resize: ResizeLane,
    /// KEEP THIS ALIVE, never read it. The pump writes any stderr frame here,
    /// and a dropped receiver turns that write into an error that ends the
    /// whole session. A pty run sends no stderr frames, so this is a trap being
    /// disarmed rather than a lane in use.
    pub idle_stderr: tokio::io::DuplexStream,
    /// the pump task, so it is owned for the session's lifetime rather than
    /// detached.
    pub pump: tokio::task::JoinHandle<()>,
}

/// the window-size lane, split out so it can be used without holding the input
/// lock: a resize arrives on a synchronous path that has no lock to await.
#[derive(Clone)]
pub struct ResizeLane(tokio::sync::mpsc::Sender<Frame>);

impl ResizeLane {
    /// tell the guest the operator's terminal changed size.
    ///
    /// Best-effort and non-blocking: a resize is a redraw hint, and a session
    /// whose input queue is momentarily full must not stall the caller that
    /// noticed the window move.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.0.try_send(Frame::Resize { cols, rows });
    }
}

/// a booted microVM. Dropping it kills the VMM — a run whose caller went away
/// must not keep a guest holding memory.
pub struct MicroVm {
    vmm: tokio::process::Child,
    run_dir: PathBuf,
    workspace_image: PathBuf,
    console: PathBuf,
    /// this run's vsock socket. Held for its PARENT directory, which is the
    /// run's alone and is removed with it.
    vsock_uds: PathBuf,
    /// the tunnel acceptors, aborted when the VM goes away.
    tunnels: Vec<tokio::task::JoinHandle<()>>,
    /// this run's connection cap and the splice tasks it is currently
    /// spending it on, torn down with the run in `Drop` — aborting `tunnels`
    /// above only stops NEW connections, this ends the ones already spliced.
    tunnel_run: Arc<TunnelRun>,
    /// when the VMM was spawned, which is where
    /// [`firecracker_api::MAX_VM_LIFETIME`] is counted from.
    booted_at: tokio::time::Instant,
}

impl Drop for MicroVm {
    /// Aborts the tunnels and DELETES the run's scratch — both the run
    /// directory and the socket directory.
    ///
    /// The run directory holds this run's workspace image, its read-only asset
    /// image and its manifest: gigabytes for a run whose PATH entries are a
    /// build tree, and nothing there outlives the VM. Leaving it was measured
    /// at 21 GB of `/tmp` across one afternoon of tests, with no owner to
    /// notice — a node serving runs continuously would fill its disk.
    ///
    /// In Drop rather than in [`Self::collect`] so that a run that FAILED
    /// cleans up too. Every diagnostic that outlives the VM has already been
    /// read into an error string by then (see [`Self::boot_failure`]), so
    /// there is nothing here left to read.
    fn drop(&mut self) {
        for tunnel in self.tunnels.drain(..) {
            tunnel.abort();
        }
        self.tunnel_run.shutdown();
        // THE 21 GB LINE. A silent failure here is the leak this Drop exists to
        // prevent, and it is invisible until a node's disk is full — so a
        // removal that did not happen says so, once, with the directory that
        // survived (a random slot id, no credential in the name).
        // A removal that failed because the directory was already gone left no
        // leak, so it is not news — the same test `RunHome`'s own teardown
        // makes.
        if let Err(error) = std::fs::remove_dir_all(&self.run_dir)
            && self.run_dir.exists()
        {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason = "run_dir_not_removed",
                run_dir = %self.run_dir.display(),
                %error,
                "the run's scratch outlived its microVM"
            );
        }
        if let Some(socket_dir) = self.vsock_uds.parent() {
            let _ = std::fs::remove_dir_all(socket_dir);
        }
    }
}

impl MicroVm {
    /// boot a VM for `manifest`, with its workspace built from `workdir`.
    ///
    /// `run_dir` must be SHORT. The vsock path lives under it and a unix socket
    /// path is capped near 108 bytes (`SUN_LEN`); a scratch directory under a
    /// long home blows straight through it with `path must be shorter than
    /// SUN_LEN`. Put it under `XDG_RUNTIME_DIR`.
    ///
    /// The CALLER owns `run_dir` and the socket directory until this returns
    /// `Ok`: create both as [`ScratchDir`]s and `disarm` them only once the
    /// returned [`MicroVm`] exists, because from here on its `Drop` is what
    /// removes them. Every error path below leaves both directories to the
    /// caller's guards.
    pub async fn boot(
        run_dir: &Path,
        workdir: &Path,
        assets: &[crate::workspace_image::GuestAsset],
        cfg: &VmConfig,
        manifest: &RunManifest,
    ) -> Result<(Self, MicroVmIo), String> {
        let started = std::time::Instant::now();

        // 1. the run's per-run block devices: what it is supposed to run, its
        //    read-only inputs, and the workspace that will be read back.
        let blob = crate::guest_manifest::encode(manifest)?;
        std::fs::write(&cfg.manifest, &blob)
            .map_err(|e| format!("write {}: {e}", cfg.manifest.display()))?;
        // 0600: the manifest's `env` carries this run's secrets (the run-action
        // and broker bearer tokens). `run_dir` is already 0700 (`ScratchDir`),
        // but this file is pinned directly too, the same defense in depth the
        // seeded `.credentials.json` already gets.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&cfg.manifest, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("restrict {} permissions: {e}", cfg.manifest.display()))?;
        }
        crate::workspace_image::build_assets(assets, &cfg.assets, &run_dir.join("assets"))?;
        let size = crate::workspace_image::sized_for(workdir)?;
        crate::workspace_image::build(workdir, &cfg.workspace, size)?;

        // 2. listen BEFORE the VMM starts (see the module docs)
        //
        // The BASE path is Firecracker's own: it binds `uds_path` for
        // host-initiated connections into the guest. A leftover file there
        // from an earlier run on this run directory makes the VMM exit
        // immediately with `Address in use`, before any guest code runs — so
        // it is cleared here rather than left to a teardown that a crash
        // skipped.
        let _ = std::fs::remove_file(&cfg.vsock_uds);
        let guest_socket = vsock_port_path(&cfg.vsock_uds, guest_proto::VSOCK_PORT);
        let _ = std::fs::remove_file(&guest_socket);
        let listener = UnixListener::bind(&guest_socket)
            .map_err(|e| format!("listen on {}: {e}", guest_socket.display()))?;

        // the tunnels' host ends, bound on the same schedule and for the same
        // reason: the guest may dial them as soon as it is up.
        //
        // One listener per service, in the manifest's order. The guest picks a
        // vsock port, never a destination, so this loop IS the allowlist —
        // and `listen_ports` is its transcript, handed to the vz shim, which
        // must declare each guest-outbound port to Virtualization.framework.
        let mut listen_ports = vec![guest_proto::VSOCK_PORT];
        let mut tunnels = Vec::with_capacity(manifest.tunnel_ports.len());
        let tunnel_run = Arc::new(TunnelRun::new(MAX_TUNNEL_CONNECTIONS));
        for (index, port) in manifest.tunnel_ports.iter().enumerate() {
            let vsock_port = guest_proto::TUNNEL_PORT_BASE + index as u32;
            let path = vsock_port_path(&cfg.vsock_uds, vsock_port);
            let _ = std::fs::remove_file(&path);
            let tunnel_listener = UnixListener::bind(&path)
                .map_err(|e| format!("listen on {}: {e}", path.display()))?;
            tunnels.push(tokio::spawn(serve_tunnel(
                tunnel_listener,
                *port,
                Arc::clone(&tunnel_run),
            )));
            listen_ports.push(vsock_port);
        }

        // 3. the VMM
        let config = firecracker_api::boot_config(cfg, &listen_ports);
        let config_path = firecracker_api::write_boot_config(run_dir, &config)?;
        let vmm_bin = cfg.vmm.host_bin();
        let vmm_path = crate::host_tools::find_executable(vmm_bin)
            .ok_or_else(|| format!("{vmm_bin} is not executable on PATH or a standard bin dir"))?;
        let console = run_dir.join("console.log");
        let console_file = std::fs::File::create(&console)
            .map_err(|e| format!("create {}: {e}", console.display()))?;
        let mut command = tokio::process::Command::new(&vmm_path);
        // `--no-api` is Firecracker's own flag (the config-file boot leaves its
        // HTTP API off); the shim's whole interface is the config file.
        if cfg.vmm == crate::sandbox::Vmm::Firecracker {
            command.arg("--no-api");
        }
        let vmm = command
            .arg("--config-file")
            .arg(&config_path)
            .stdin(Stdio::null())
            // the guest's serial console. The ONLY diagnostic for a guest that
            // never reaches userspace, so it is captured rather than inherited.
            .stdout(
                console_file
                    .try_clone()
                    .map_err(|e| format!("dup console: {e}"))?,
            )
            .stderr(console_file)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn firecracker: {e}"))?;

        // per-run, so `debug`. The three numbers are the ones a run's cost and
        // its refusals are argued from: what it was sold (vcpus/mem) and how
        // big the image it had to build was.
        tracing::debug!(
            target: "ducktape::sandbox",
            slot = %slot_of(run_dir),
            vcpus = cfg.vcpus,
            mem_mib = cfg.mem_mib,
            workspace_bytes = size,
            assets = assets.len(),
            tunnels = manifest.tunnel_ports.len(),
            "VMM spawned; waiting for the guest"
        );

        // the VM owns both directories from here: its `Drop` removes them on
        // every path out, including the boot failures below. The caller's
        // `ScratchDir`s are disarmed against this same `Ok`.
        let mut vm = MicroVm {
            vmm,
            run_dir: run_dir.to_path_buf(),
            workspace_image: cfg.workspace.clone(),
            console: console.clone(),
            vsock_uds: cfg.vsock_uds.clone(),
            tunnels,
            tunnel_run,
            booted_at: tokio::time::Instant::now(),
        };

        // 4. the guest dials back
        let stream = match tokio::time::timeout(GUEST_CONNECT_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => stream,
            Ok(Err(e)) => {
                return Err(vm
                    .boot_failure(
                        "guest_vsock_accept_failed",
                        &format!("accept the guest vsock: {e}"),
                    )
                    .await);
            }
            Err(_) => {
                return Err(vm
                    .boot_failure(
                        "guest_never_dialled",
                        "the guest never dialled back before the boot timeout",
                    )
                    .await);
            }
        };

        // the boot budget this run actually spent, which is the number a slow
        // node is diagnosed from — and the only place it is ever measured.
        tracing::debug!(
            target: "ducktape::sandbox",
            slot = %slot_of(run_dir),
            boot_ms = started.elapsed().as_millis() as u64,
            "the guest dialled back"
        );

        let io = spawn_pump(stream);
        Ok((vm, io))
    }

    /// the run's console output, for a failure whose cause is only visible
    /// there. Truncated: a kernel panic's trace is long and the useful part is
    /// the end.
    fn console_tail(&self) -> String {
        let raw = std::fs::read_to_string(&self.console).unwrap_or_default();
        let tail: Vec<&str> = raw.lines().rev().take(20).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    /// a boot failure, with the console attached. Kills the VMM first: whatever
    /// went wrong, a guest nobody is listening to must not keep running.
    ///
    /// `reason` is the stable snake_case token the log is counted by; `what` is
    /// the prose that reaches the caller's error string. The event is a `warn`
    /// and not an `error` because the run is refused, not the node: the daemon
    /// takes the next run.
    async fn boot_failure(&mut self, reason: &'static str, what: &str) -> String {
        let _ = self.vmm.kill().await;
        tracing::warn!(
            target: "ducktape::sandbox",
            reason,
            slot = %slot_of(&self.run_dir),
            // NOT the console tail: it is guest-authored and unbounded, and one
            // kernel-panic trace would evict a large slice of the 4096-line
            // ring. It rides the returned error instead, where the caller can
            // decide.
            "the microVM never came up; the run is refused"
        );
        format!("{what}\nguest console:\n{}", self.console_tail())
    }

    /// wait for the VMM to exit, then walk the workspace image back into
    /// `workdir`.
    ///
    /// Ordered, not concurrent: the guest syncs and unmounts before it halts,
    /// so reading the image before the VMM is gone risks reading a filesystem
    /// with an open journal.
    ///
    /// BOUNDED by [`firecracker_api::MAX_VM_LIFETIME`], counted from the boot.
    /// The invoke loop's timeouts bound the arrival of the guest's exit FRAME,
    /// not the VM's exit — after that frame the guest still has to sync,
    /// unmount and halt, and `duck-guest-init`'s halt ends in `pause()` when
    /// `reboot(2)` fails. Unbounded, one such guest blocks this await forever:
    /// the run's task never completes, and the VMM keeps its vcpus and its
    /// whole memory footprint with no timer anywhere to reap it.
    pub async fn collect(mut self, workdir: &Path) -> Result<(), String> {
        let deadline = self.booted_at + firecracker_api::MAX_VM_LIFETIME;
        let waited = tokio::time::timeout_at(deadline, self.vmm.wait())
            .await
            .ok()
            .map(|exited| exited.map(|_| ()));
        if waited.is_none() {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason = "vm_lifetime_exceeded",
                slot = %slot_of(&self.run_dir),
                lifetime_s = firecracker_api::MAX_VM_LIFETIME.as_secs(),
                "the guest never halted inside the VM's lifetime; killing the VMM"
            );
            let _ = self.vmm.kill().await;
        }
        // either way `self` drops at the end of this fn, so the run's
        // directories are reaped on the expiry path too.
        vmm_exit_outcome(waited)?;
        let started = std::time::Instant::now();
        let read_back = crate::workspace_image::read_back(&self.workspace_image, workdir);
        // the workspace round trip is the run's longest non-model step (13.8 s
        // for a 1.7 GB tree), and it is invisible from outside — the caller
        // only sees `collect` return. Timed here or nowhere.
        tracing::debug!(
            target: "ducktape::sandbox",
            copy_back_ms = started.elapsed().as_millis() as u64,
            ok = read_back.is_ok(),
            "workspace read back"
        );
        read_back
    }

    /// stop the VM now. Used when the run is abandoned rather than finished.
    pub async fn terminate(&mut self) {
        tracing::debug!(
            target: "ducktape::sandbox",
            reason = "run_abandoned",
            "killing the microVM before its guest exited"
        );
        let _ = self.vmm.kill().await;
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }
}

/// the host end of one tunnel: every guest connection on this vsock port is
/// spliced to `service_port` on this host's loopback.
///
/// The destination is CLOSED OVER, not read off the wire. The services bind
/// LOOPBACK, not a routable interface, so nothing outside this process can
/// reach them — and the guest reaches exactly this one address because it is
/// the only address this function will ever dial. That property is what
/// replaces the container backend's nft input chain, and it is stronger: there
/// is no rule to get wrong, because there is no destination the guest can name.
async fn serve_tunnel(listener: UnixListener, service_port: u16, run: Arc<TunnelRun>) {
    loop {
        let Ok((guest, _)) = listener.accept().await else {
            return;
        };
        let Ok(permit) = Arc::clone(&run.permits).try_acquire_owned() else {
            run.refuse();
            continue; // `guest` drops here, closing the socket we just accepted.
        };
        let mut splices = run.splices.lock().unwrap();
        // opportunistic reap: a long-lived run's tunnel sees far more
        // connections than its cap, and nothing else ever drains a finished
        // task out of the set.
        while splices.try_join_next().is_some() {}
        splices.spawn(splice(guest, service_port, permit));
    }
}

/// splice one guest connection to `service_port` on this host's loopback
/// until EOF or an error ends either direction, or the shared [`Activity`]
/// deadline decides the CONNECTION has been idle — a direction going quiet on
/// its own (a request whose body ended while its response keeps streaming)
/// never ends the splice by itself.
async fn splice(guest: UnixStream, service_port: u16, _permit: tokio::sync::OwnedSemaphorePermit) {
    let service = match TcpStream::connect(("127.0.0.1", service_port)).await {
        Ok(service) => service,
        // The guest asked for the one address this tunnel can dial and
        // nothing answered. Silent, this reaches the operator as a CLI
        // that hangs and then times out with no cause anywhere — the
        // broker being down looks exactly like a slow model.
        Err(error) => {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason = "tunnel_dial_failed",
                service_port,
                %error,
                "a guest tunnel had nothing to connect to on this host"
            );
            return;
        }
    };
    let (mut guest_read, mut guest_write) = guest.into_split();
    let (mut broker_read, mut broker_write) = service.into_split();
    let activity = Activity::new();
    let up = copy_streaming(&mut guest_read, &mut broker_write, &activity);
    let down = copy_streaming(&mut broker_read, &mut guest_write, &activity);
    let watchdog = idle_watchdog(&activity, TUNNEL_IDLE_TIMEOUT);
    // race, not join: EOF or an error in either direction, or the watchdog
    // deciding the connection as a whole has moved no byte in
    // TUNNEL_IDLE_TIMEOUT, tears the whole connection down — whichever of the
    // three happens first.
    tokio::select! {
        _ = up => {}
        _ = down => {}
        _ = watchdog => {}
    }
}

/// the last time any byte moved in either direction of one spliced
/// connection. Shared by both copy loops and the watchdog in [`splice`] so
/// idleness is judged for the connection as a whole, never per direction.
struct Activity(Mutex<tokio::time::Instant>);

impl Activity {
    fn new() -> Self {
        Self(Mutex::new(tokio::time::Instant::now()))
    }

    fn touch(&self) {
        *self.0.lock().unwrap() = tokio::time::Instant::now();
    }

    fn last(&self) -> tokio::time::Instant {
        *self.0.lock().unwrap()
    }
}

/// copy from `reader` to `writer` until EOF or a read/write error — no
/// per-direction idle timeout here; that is [`idle_watchdog`]'s job, over the
/// shared [`Activity`] mark, so one silent direction cannot end an otherwise
/// live connection. `activity` is touched after every read AND after every
/// write completes: touching only on read would let a writer that never
/// accepts bytes (a stalled peer) hold the splice open forever, since a
/// read-only mark never ages while `write_all` blocks.
async fn copy_streaming<R, W>(reader: &mut R, writer: &mut W, activity: &Activity)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(n) if n > 0 => n,
            _ => break, // EOF or a read error ends the copy.
        };
        activity.touch();
        if writer.write_all(&buf[..read]).await.is_err() {
            break;
        }
        activity.touch();
    }
    let _ = writer.shutdown().await;
}

/// sleeps until `activity` has been unmoved for `idle`, re-checking on each
/// wake since a copy loop may have touched it while this slept. The one place
/// that decides a spliced connection's shared deadline has actually elapsed.
async fn idle_watchdog(activity: &Activity, idle: std::time::Duration) {
    loop {
        let deadline = activity.last() + idle;
        tokio::time::sleep_until(deadline).await;
        let still_idle = tokio::time::Instant::now() >= activity.last() + idle;
        if still_idle {
            return;
        }
    }
}

/// one run's guest-tunnel bookkeeping, shared across every tunnel the run has
/// open: the guest picks how many connections to hold, not which port it
/// holds them on, so the cap and the splice tasks it spends are per RUN.
struct TunnelRun {
    permits: Arc<Semaphore>,
    refused: AtomicU64,
    splices: Mutex<JoinSet<()>>,
}

impl TunnelRun {
    fn new(max_connections: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_connections)),
            refused: AtomicU64::new(0),
            splices: Mutex::new(JoinSet::new()),
        }
    }

    /// record one more refusal, warning on the first and then every Nth: an
    /// unconditional `warn!` here is a guest-controlled log bomb, and the
    /// `attempts` count is itself the diagnosis.
    fn refuse(&self) {
        const LOG_EVERY: u64 = 100;
        let attempts = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
        if attempts == 1 || attempts.is_multiple_of(LOG_EVERY) {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason = "tunnel_connection_refused",
                attempts,
                max_connections = MAX_TUNNEL_CONNECTIONS,
                "a guest tunnel connection was refused; the run's connection cap is full"
            );
        }
    }

    /// end every splice this run has open. Called from [`MicroVm`]'s own
    /// `Drop` so a run's tunnels do not outlive the run — aborting the accept
    /// loops alone stops new connections but leaves already-spliced ones
    /// running, since they were spawned onto the runtime, not into `tunnels`.
    fn shutdown(&self) {
        self.splices.lock().unwrap().abort_all();
    }
}

/// One directory a run owns: created here, removed when this value drops,
/// unless [`Self::disarm`] hands it on.
///
/// It closes the window between a directory's `create_dir_all` and the
/// [`MicroVm`] whose `Drop` removes the run's scratch. A run has two of them —
/// the run directory and the vsock socket directory — and every `?` in between
/// (an oversized workspace, a foreign binary in the executors directory, a
/// vsock path over `SUN_LEN`, a VMM that will not spawn) used to leave them
/// behind with nothing to reap them: a node that refuses runs leaked one pair
/// per attempt, and the socket directory sits on a tmpfs, i.e. the node's own
/// RAM.
pub struct ScratchDir {
    path: PathBuf,
    owned: bool,
}

impl ScratchDir {
    /// create `path`, 0700, and own it from this moment.
    ///
    /// 0700 keeps it off every other local account on a shared box (#1693): a
    /// bare `create_dir_all` lands 0755 under a normal umask, and everything
    /// this directory ever holds — `manifest.bin`'s run env (the run-action and
    /// broker bearer tokens), `workspace.ext4` (the seeded `.credentials.json`),
    /// `assets.ext4` — is secret for the run's whole lifetime. Firecracker/vz
    /// run as this same process's uid (no jailer, no setuid handoff), so 0700
    /// costs the VMM nothing.
    pub fn create(path: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("restrict {} permissions: {e}", path.display()))?;
        }
        Ok(Self { path, owned: true })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// stop owning it, because something whose own teardown removes it now
    /// does — the booted [`MicroVm`].
    pub fn disarm(mut self) {
        self.owned = false;
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&self.path)
            && self.path.exists()
        {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason = "run_dir_not_removed",
                slot = %slot_of(&self.path),
                %error,
                "a refused run left its directory behind"
            );
        }
    }
}

/// what the bounded wait for the VMM means for the run. `None` is the wait
/// hitting [`firecracker_api::MAX_VM_LIFETIME`].
///
/// A killed VM's workspace image is NOT read back: the guest never finished its
/// unmount, so the filesystem on it has an open journal and whatever it holds is
/// not the run's result. Losing the workspace is the honest outcome — reading a
/// torn one silently would land it on the host as if the run had succeeded.
fn vmm_exit_outcome(waited: Option<std::io::Result<()>>) -> Result<(), String> {
    match waited {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(format!("wait for the VMM: {error}")),
        None => Err(format!(
            "the guest did not halt within the microVM's {} s lifetime; the VMM was killed and the workspace was not read back",
            firecracker_api::MAX_VM_LIFETIME.as_secs()
        )),
    }
}

/// the run directory's own name — `/tmp/dt-vm-<slot>`, the id every one of a
/// run's files is under. It is the join key between a log line and the scratch
/// on disk, and it is not a path: the parent is the host's, the leaf is ours.
fn slot_of(run_dir: &Path) -> std::borrow::Cow<'_, str> {
    run_dir
        .file_name()
        .map_or(std::borrow::Cow::Borrowed("?"), |name| {
            name.to_string_lossy()
        })
}

/// Firecracker connects a guest's outbound vsock to `<uds_path>_<port>`.
fn vsock_port_path(uds: &Path, port: u32) -> PathBuf {
    let mut name = uds.as_os_str().to_os_string();
    name.push(format!("_{port}"));
    PathBuf::from(name)
}

/// wire the guest connection to duplex streams the caller can treat as pipes.
/// how many host→guest frames may queue before the writer backpressures.
///
/// Bounded on purpose: an unbounded queue would let a prompt larger than the
/// socket buffer pile up in host memory instead of slowing the reader that
/// produced it. Small, because the only producers are one prompt reader and the
/// occasional resize.
const INPUT_QUEUE: usize = 16;

fn spawn_pump(stream: UnixStream) -> MicroVmIo {
    // 64 KiB matches the invoke loop's read granularity.
    let (stdin_host, stdin_task) = tokio::io::duplex(64 * 1024);
    let (out_task, stdout_host) = tokio::io::duplex(64 * 1024);
    let (err_task, stderr_host) = tokio::io::duplex(64 * 1024);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(INPUT_QUEUE);

    tokio::spawn(frame_stdin(stdin_task, input_tx.clone()));
    let pump = tokio::spawn(pump_frames(stream, input_rx, out_task, err_task, exit_tx));

    MicroVmIo {
        stdin: stdin_host,
        stdout: stdout_host,
        stderr: stderr_host,
        exit: exit_rx,
        pump,
        input: input_tx,
    }
}

/// turn the caller's stdin writes into frames.
///
/// Its own task so a prompt larger than the socket buffer cannot block the
/// guest's output from being drained: the caller writing the prompt and the
/// caller reading the answer are frequently the same loop.
async fn frame_stdin(mut stdin: tokio::io::DuplexStream, input: tokio::sync::mpsc::Sender<Frame>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if input.send(Frame::Stdin(buf[..n].to_vec())).await.is_err() {
            return;
        }
    }
    // The prompt is complete. A headless guest closes the child's stdin on
    // this so a CLI blocking on EOF proceeds; an interactive one ignores it,
    // because a terminal has no EOF to close.
    let _ = input.send(Frame::StdinEof).await;
}

async fn pump_frames(
    stream: UnixStream,
    mut input: tokio::sync::mpsc::Receiver<Frame>,
    mut stdout: tokio::io::DuplexStream,
    mut stderr: tokio::io::DuplexStream,
    exit: tokio::sync::oneshot::Sender<i32>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    // host -> guest. ONE owner of the write half, which is why every inbound
    // lane (the prompt, keystrokes, resizes) is funnelled through one channel
    // rather than writing here directly.
    let feed = tokio::spawn(async move {
        while let Some(frame) = input.recv().await {
            if write_half
                .write_all(&guest_proto::encode(&frame))
                .await
                .is_err()
            {
                break;
            }
        }

        // Everything sendable is sent, but this task must NOT end: it owns the
        // write half, and Firecracker's vsock backend maps a host-side
        // half-close onto a full connection RESET rather than a half-close.
        // Dropping it here therefore kills the guest's output stream mid-run —
        // measured, the guest echoed the prompt and got EPIPE, and the host saw
        // a run that produced nothing and reported no exit code.
        //
        // The abort at the end of this function is what ends it, and that
        // close is also the guest's signal that its exit frame landed.
        std::future::pending::<()>().await
    });
    // From here every exit path ends that task, including the three that return
    // early below. Dropping a bare `JoinHandle` does NOT abort a tokio task, so
    // one unparseable guest frame used to leave the feed parked forever holding
    // the vsock write half — one task and one fd per affected run, for the
    // daemon's life, until the node's fd limit made every later run's listener
    // bind fail.
    let mut feed = AbortOnDrop(feed);

    // guest -> host
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut exit = Some(exit);
    'outbound: loop {
        let n = match read_half.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        loop {
            let frame = match guest_proto::decode(&mut pending) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                // A guest that speaks nonsense is untrusted input, not a bug.
                // Stop reading; the caller sees EOF and a missing exit code —
                // which reaches the operator as "guest halted without reporting
                // an exit code" and, unsaid, names nothing. The arm returns, so
                // this can fire at most once per run. No payload: the frame is
                // guest-authored.
                Err(_) => {
                    tracing::warn!(
                        target: "ducktape::sandbox",
                        reason = "frame_decode_failed",
                        "the guest sent a frame this host cannot parse; dropping the stream"
                    );
                    return;
                }
            };
            match frame {
                Frame::Stdout(bytes) => {
                    // A session reader may finish at its result before the
                    // CLI finishes cleanup. Keep draining to the exit frame.
                    let _ = stdout.write_all(&bytes).await;
                }
                Frame::Stderr(bytes) => {
                    let _ = stderr.write_all(&bytes).await;
                }
                // Last frame of the run, and closing here is the guest's
                // signal that it may reset. Firecracker relays the guest's
                // vsock writes asynchronously, so a guest that halted the
                // instant after writing would lose whatever the VMM had not
                // relayed yet — and the frame that goes missing is this one.
                Frame::Exit(code) => {
                    if let Some(tx) = exit.take() {
                        let _ = tx.send(code);
                    }
                    break 'outbound;
                }
                // the guest never sends these; a stray one is not worth failing
                // a finished run over.
                Frame::Stdin(_) | Frame::StdinEof | Frame::Resize { .. } => {}
            }
        }
    }
    // Awaited, not just aborted: the write half lives in that task, and the
    // guest is blocked on this socket closing. Letting the guard's `Drop` do it
    // leaves the close to whenever the runtime gets round to it, and this is
    // the one path where the guest is waiting on it.
    feed.abort_and_join().await;
}

/// A spawned task this scope owns: aborted when the value drops.
///
/// The plain `JoinHandle` does not do this — dropping one DETACHES the task —
/// so a scope with more than one exit path leaks every task it spawned on all
/// but the path that remembered to abort.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl AbortOnDrop {
    /// end the task and WAIT for it, on the path where something is waiting on
    /// what its teardown does.
    async fn abort_and_join(&mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1693: a bare `create_dir_all` lands 0755 under a normal umask, making
    /// the run directory — and everything staged into it (`manifest.bin`'s run
    /// env, `workspace.ext4`'s seeded credentials, `assets.ext4`) — readable by
    /// every other local account for the run's whole lifetime.
    #[test]
    fn scratch_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "dt-scratch-dir-test-{}-{unique}",
            std::process::id()
        ));
        let dir = ScratchDir::create(path.clone()).expect("create scratch dir");
        let mode = std::fs::metadata(dir.path())
            .expect("scratch dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} is readable by group/other: {mode:o}",
            dir.path().display()
        );
    }

    /// Firecracker's guest-outbound convention. Getting this wrong means
    /// nothing is listening where the guest dials, and the run produces no
    /// output at all — a silence indistinguishable from a hung CLI.
    #[test]
    fn the_guest_dials_the_port_suffixed_socket() {
        assert_eq!(
            vsock_port_path(Path::new("/run/user/1000/dt/v.sock"), 1024),
            PathBuf::from("/run/user/1000/dt/v.sock_1024")
        );
    }

    /// The prompt is finished long before the run is, and the task that sent it
    /// OWNS the write half. Letting it return there drops that half, and
    /// Firecracker's vsock backend maps a host-side half-close onto a full
    /// connection RESET rather than a half-close — measured, the guest echoed
    /// its prompt straight into EPIPE and the run reached the operator as
    /// "produced nothing, reported no exit code".
    ///
    /// Parsed from the source rather than exercised, because the behaviour
    /// being guarded belongs to Firecracker's backend: a `UnixStream::pair`
    /// honours the half-close, so a socketpair test passes either way and
    /// guards nothing. What IS checkable here is the shape that avoids it.
    #[test]
    fn the_feed_task_parks_instead_of_returning() {
        let src = include_str!("microvm.rs");
        let feed = src
            .split_once("let feed = tokio::spawn(")
            .expect("the feed task")
            .1
            .split_once("\n    });")
            .expect("the feed task is one spawn block")
            .0;
        assert!(
            feed.contains("std::future::pending::<()>().await"),
            "the feed task owns the write half and must park, never return:\n{feed}"
        );
    }

    /// The lifetime backstop is WIRED, and an expiry is an error rather than a
    /// silently torn workspace.
    ///
    /// `collect` used to await the VMM unbounded on the success path, and
    /// `MAX_VM_LIFETIME` was referenced nowhere in the tree: a guest that
    /// reported its exit code and then wedged on halt (a `umount` blocked by a
    /// lingering grandchild, a kernel whose power-off returns) blocked the run's
    /// task forever with the VM's whole memory footprint still charged.
    ///
    /// The mapping only, because the timeout itself is the runtime's and there
    /// is no seam for a fake VMM handle: `collect` owns a real
    /// `tokio::process::Child`.
    #[test]
    fn a_vm_that_outlives_its_lifetime_fails_the_run() {
        assert!(vmm_exit_outcome(Some(Ok(()))).is_ok());

        let expired = vmm_exit_outcome(None).expect_err("an expired lifetime fails the run");
        assert!(
            expired.contains("lifetime") && expired.contains("not read back"),
            "the operator must be told the workspace is gone, not just that it took long: {expired}"
        );

        let failed = vmm_exit_outcome(Some(Err(std::io::Error::other("no child"))))
            .expect_err("a wait error fails the run");
        assert!(failed.contains("no child"), "{failed}");
    }

    /// A guest frame this host cannot parse ends the feed task too.
    ///
    /// That arm is an early `return`, and dropping a `JoinHandle` DETACHES a
    /// tokio task rather than aborting it — so the feed used to stay parked on
    /// its `pending()` for the daemon's life, holding the run's vsock write
    /// half. A compute daemon accumulated one task and one fd per affected run
    /// until its fd limit made every later run's listener bind fail.
    ///
    /// The feed task owns the input receiver, so the sender closing IS that
    /// task ending — no other observer of it exists.
    #[tokio::test]
    async fn a_decode_failure_ends_the_feed_task() {
        let (host, guest) = UnixStream::pair().expect("socketpair");
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(INPUT_QUEUE);
        let (out_task, _stdout) = tokio::io::duplex(1024);
        let (err_task, _stderr) = tokio::io::duplex(1024);
        let (exit_tx, _exit_rx) = tokio::sync::oneshot::channel();
        let pump = tokio::spawn(pump_frames(host, input_rx, out_task, err_task, exit_tx));

        // a header claiming more than MAX_FRAME_BYTES: refused by `decode`
        // before a byte of payload is read.
        let (_guest_read, mut guest_write) = guest.into_split();
        guest_write
            .write_all(&[0u8, 0xff, 0xff, 0xff, 0xff])
            .await
            .expect("write a frame the host cannot parse");

        pump.await.expect("the pump returns on a decode failure");
        tokio::time::timeout(std::time::Duration::from_secs(5), input_tx.closed())
            .await
            .expect("the feed task is still parked, holding the vsock write half");
    }

    #[tokio::test]
    async fn closed_output_readers_do_not_discard_the_guest_exit() {
        let (host, mut guest) = UnixStream::pair().expect("socketpair");
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(INPUT_QUEUE);
        let (out_task, stdout) = tokio::io::duplex(1024);
        let (err_task, stderr) = tokio::io::duplex(1024);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        drop(stdout);
        drop(stderr);
        let pump = tokio::spawn(pump_frames(host, input_rx, out_task, err_task, exit_tx));
        for frame in [
            Frame::Stdout(b"after result".to_vec()),
            Frame::Stderr(b"cleanup".to_vec()),
            Frame::Exit(0),
        ] {
            guest.write_all(&guest_proto::encode(&frame)).await.unwrap();
        }
        assert_eq!(exit_rx.await.expect("guest exit survives closed readers"), 0);
        pump.await.unwrap();
        input_tx.closed().await;
    }

    /// The two halves of the ownership contract `boot` documents: a directory
    /// that was never handed on is removed, and one that was is left to the
    /// [`MicroVm`] that will remove it.
    #[test]
    fn a_scratch_dir_outlives_its_guard_only_once_disarmed() {
        let base = std::env::temp_dir().join(format!("dt-scratch-{}", std::process::id()));
        let refused = base.join("refused");
        let booted = base.join("booted");

        ScratchDir::create(refused.clone()).expect("create");
        assert!(
            !refused.exists(),
            "a refused run left {}",
            refused.display()
        );

        ScratchDir::create(booted.clone()).expect("create").disarm();
        assert!(
            booted.exists(),
            "a disarmed guard removed {}",
            booted.display()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A unix socket path is capped near 108 bytes. The run directory is
    /// chosen by the caller, so the constraint is documented on `boot` — this
    /// pins the arithmetic that makes it checkable.
    #[test]
    fn a_run_directory_under_xdg_runtime_stays_inside_sun_len() {
        let path = vsock_port_path(
            Path::new("/run/user/1000/ducktape/run-0123456789/v.sock"),
            1024,
        );
        assert!(
            path.as_os_str().len() < 108,
            "{} is {} bytes",
            path.display(),
            path.as_os_str().len()
        );
    }

    /// a loopback TCP echo service: what a spliced tunnel connection talks to
    /// in these tests, standing in for the real broker/http listener a tunnel
    /// dials in production.
    async fn spawn_echo_service() -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind echo service");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        if stream.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    fn unique_socket_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dt-tunnel-test-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    /// round-trips one byte through `stream` and asserts the echo service
    /// answered — proof the connection is actually spliced, not merely open.
    async fn assert_spliced(stream: &mut UnixStream) {
        stream
            .write_all(b"x")
            .await
            .expect("write to a live tunnel");
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("the echo answers before the test timeout")
            .expect("read the echo back");
        assert_eq!(n, 1, "the tunnel closed instead of echoing");
        assert_eq!(&buf, b"x");
    }

    /// #1873: a guest's tunnel connections are capped per run. The
    /// (cap+1)th concurrent connection must be refused — closed with nothing
    /// ever spliced to it — while the connections already under the cap keep
    /// working.
    #[tokio::test]
    async fn a_connection_past_the_cap_is_refused_while_earlier_ones_stay_open() {
        let service_port = spawn_echo_service().await;
        let path = unique_socket_path("cap");
        let listener = UnixListener::bind(&path).expect("bind tunnel socket");
        let run = Arc::new(TunnelRun::new(2));
        let accept = tokio::spawn(serve_tunnel(listener, service_port, Arc::clone(&run)));

        let mut first = UnixStream::connect(&path).await.expect("connect 1");
        assert_spliced(&mut first).await;
        let mut second = UnixStream::connect(&path).await.expect("connect 2");
        assert_spliced(&mut second).await;

        // the cap is 2; a third concurrent connection must be refused.
        let mut third = UnixStream::connect(&path).await.expect("connect 3");
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), third.read(&mut buf))
            .await
            .expect("the refused connection closes before the test timeout")
            .expect("a closed socket reads Ok(0), not an error");
        assert_eq!(n, 0, "the connection past the cap was not closed");
        assert_eq!(
            run.refused.load(Ordering::Relaxed),
            1,
            "the refusal was not counted"
        );

        // the connections under the cap were never touched by the refusal.
        assert_spliced(&mut first).await;
        assert_spliced(&mut second).await;

        accept.abort();
    }

    /// #1873: tearing a run down (what [`MicroVm`]'s own `Drop` calls) closes
    /// every splice it still has open, not just its accept loops.
    #[tokio::test]
    async fn shutdown_closes_every_spliced_connection() {
        let service_port = spawn_echo_service().await;
        let path = unique_socket_path("shutdown");
        let listener = UnixListener::bind(&path).expect("bind tunnel socket");
        let run = Arc::new(TunnelRun::new(4));
        let accept = tokio::spawn(serve_tunnel(listener, service_port, Arc::clone(&run)));

        let mut a = UnixStream::connect(&path).await.expect("connect a");
        assert_spliced(&mut a).await;
        let mut b = UnixStream::connect(&path).await.expect("connect b");
        assert_spliced(&mut b).await;

        run.shutdown();

        for stream in [&mut a, &mut b] {
            let mut buf = [0u8; 1];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("a torn-down splice closes before the test timeout")
                .expect("a closed socket reads Ok(0), not an error");
            assert_eq!(n, 0, "shutdown left a splice running");
        }

        accept.abort();
    }

    /// #1881: a request direction that goes silent once its body ends must
    /// not cut off a response still streaming the other way, even across
    /// several `TUNNEL_IDLE_TIMEOUT` windows. A per-direction timer (the old
    /// behavior) closes this tunnel after the first window; the shared
    /// [`Activity`] deadline must not.
    #[tokio::test(start_paused = true)]
    async fn a_streaming_response_survives_a_silent_request_direction() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind service");
        let service_port = listener.local_addr().expect("local addr").port();
        let (service_tx, service_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = service_tx.send(stream);
        });

        let path = unique_socket_path("silent-request");
        let tunnel_listener = UnixListener::bind(&path).expect("bind tunnel socket");
        let run = Arc::new(TunnelRun::new(1));
        let accept = tokio::spawn(serve_tunnel(tunnel_listener, service_port, Arc::clone(&run)));

        let mut guest = UnixStream::connect(&path).await.expect("connect guest");
        // the whole "request": sent once, then the guest→service direction
        // never sends another byte for the rest of the test.
        guest.write_all(b"req").await.expect("send the request");
        let mut service = service_rx.await.expect("service accepted the splice");
        let mut request = [0u8; 3];
        service
            .read_exact(&mut request)
            .await
            .expect("service reads the request");
        assert_eq!(&request, b"req");

        // the response streams for several idle windows while the request
        // direction stays silent; each round trip must still arrive.
        const ROUNDS: u8 = 3;
        for round in 0..ROUNDS {
            tokio::time::advance(TUNNEL_IDLE_TIMEOUT - std::time::Duration::from_secs(1)).await;
            service
                .write_all(&[round])
                .await
                .expect("service writes a response chunk");
            let mut chunk = [0u8; 1];
            // a plain await, not a raced timeout: under a paused clock a
            // `tokio::time::timeout` here would compete with the production
            // watchdog's own sleep and can resolve on a virtual-clock
            // auto-advance before the real cross-task splice hops (guest ->
            // up copy -> broker -> service, and back down) ever run — the
            // event to wait on is the byte arriving, not a deadline.
            guest
                .read_exact(&mut chunk)
                .await
                .expect("read the response chunk");
            assert_eq!(
                chunk[0], round,
                "a response chunk was lost while the request direction was silent"
            );
        }

        // the service ending its response is what closes the tunnel here —
        // not an idle timeout that should never have fired.
        drop(service);
        let mut buf = [0u8; 1];
        let n = guest
            .read(&mut buf)
            .await
            .expect("a closed socket reads Ok(0), not an error");
        assert_eq!(n, 0, "the tunnel did not close when the response ended");

        accept.abort();
    }

    /// #1881: when NEITHER direction has moved a byte for `TUNNEL_IDLE_TIMEOUT`,
    /// the shared deadline must still close the tunnel.
    #[tokio::test(start_paused = true)]
    async fn a_tunnel_silent_in_both_directions_closes_at_the_deadline() {
        let service_port = spawn_echo_service().await;
        let path = unique_socket_path("both-silent");
        let listener = UnixListener::bind(&path).expect("bind tunnel socket");
        let run = Arc::new(TunnelRun::new(1));
        let accept = tokio::spawn(serve_tunnel(listener, service_port, Arc::clone(&run)));

        let mut guest = UnixStream::connect(&path).await.expect("connect guest");
        // a plain round trip, not `assert_spliced`'s timeout-wrapped read: see
        // the comment in the streaming-response test above for why a raced
        // timer is unsound here under a paused clock.
        guest.write_all(b"x").await.expect("write to a live tunnel");
        let mut echoed = [0u8; 1];
        guest
            .read_exact(&mut echoed)
            .await
            .expect("the echo answers");
        assert_eq!(&echoed, b"x", "the tunnel closed instead of echoing");

        // silence in both directions across the whole deadline.
        tokio::time::advance(TUNNEL_IDLE_TIMEOUT + std::time::Duration::from_secs(1)).await;

        let mut buf = [0u8; 1];
        let n = guest
            .read(&mut buf)
            .await
            .expect("a closed socket reads Ok(0), not an error");
        assert_eq!(n, 0, "a tunnel idle in both directions was not closed");

        accept.abort();
    }
}
