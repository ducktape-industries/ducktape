//! A node's machine: `ducktape machine run` as the node's child, started with `[machine]`'s CPU
//! set, nice and memory cap (a cgroup of its own where the node may make one, else a per-process
//! address-space limit), stopped with the node, and started again at the next block it applies.
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::MachineSettings;
use crate::workspace::Workspace;

const TARGET: &str = "ducktape::machine";
#[cfg(any(target_os = "linux", test))]
const MACHINE: &str = "machine";
#[cfg(any(target_os = "linux", test))]
const AGENT: &str = "agent";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub cpus: Option<Vec<usize>>,
    pub nice: Option<i32>,
    pub memory: Option<u64>,
}

impl Limits {
    pub fn of(settings: &MachineSettings, rpc_cpus: Option<&[usize]>) -> Limits {
        let cpus = match &settings.cpus {
            Some(list) => Some(list.cpus().to_vec()),
            None => rpc_cpus.map(<[usize]>::to_vec),
        };
        Limits {
            cpus,
            nice: settings.nice,
            memory: settings.memory,
        }
    }

    fn unlimited(&self) -> bool {
        *self == Limits::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cap {
    Cgroup(PathBuf),
    Rlimit(u64),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capped {
    pub cap: Cap,
    pub reason: &'static str,
}

impl Cap {
    pub fn word(&self) -> &'static str {
        match self {
            Cap::Cgroup(_) => "cgroup",
            Cap::Rlimit(_) => "rlimit",
            Cap::None => "none",
        }
    }
}

pub fn cap(limits: &Limits) -> Capped {
    let Some(memory) = limits.memory else {
        return Capped {
            cap: Cap::None,
            reason: "memory_unset",
        };
    };
    match cgroup(memory, limits.cpus.as_deref()) {
        Ok(procs) => Capped {
            cap: Cap::Cgroup(procs),
            reason: "cgroup_made",
        },
        Err(reason) => Capped {
            cap: Cap::Rlimit(memory),
            reason,
        },
    }
}

#[cfg(target_os = "linux")]
fn cgroup(memory: u64, cpus: Option<&[usize]>) -> Result<PathBuf, &'static str> {
    let own = std::fs::read_to_string("/proc/self/cgroup").map_err(|_| "cgroup_unread")?;
    let path = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("cgroup_v1")?;
    let relative = path.trim_start_matches('/');
    if relative.is_empty() {
        return Err("cgroup_root");
    }
    let parent = std::path::Path::new("/sys/fs/cgroup").join(relative);
    let control = parent.join("cgroup.subtree_control");
    let enabled = std::fs::read_to_string(&control).map_err(|_| "cgroup_unread")?;
    let controls = |name: &str| enabled.split_whitespace().any(|held| held == name);
    if !controls("memory") {
        std::fs::write(&control, "+memory").map_err(|_| "memory_undelegated")?;
    }
    let child = parent.join(MACHINE);
    made(&child)?;
    let cpuset = controls("cpuset");
    let set = std::fs::write(child.join("memory.max"), memory.to_string());
    let pinned = match cpus {
        Some(cpus) if cpuset => {
            let list: Vec<String> = cpus.iter().map(usize::to_string).collect();
            std::fs::write(child.join("cpuset.cpus"), list.join(","))
        }
        _ => Ok(()),
    };
    let agent = child.join(AGENT);
    let placed = made(&agent);
    let handed = if cpuset { "+memory +cpuset" } else { "+memory" };
    let delegated = std::fs::write(child.join("cgroup.subtree_control"), handed);
    if set.is_err() || pinned.is_err() || placed.is_err() || delegated.is_err() {
        let _ = std::fs::remove_dir(&agent);
        let _ = std::fs::remove_dir(&child);
        return Err("cgroup_unset");
    }
    Ok(agent.join("cgroup.procs"))
}

#[cfg(target_os = "linux")]
fn made(dir: &std::path::Path) -> Result<(), &'static str> {
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err("cgroup_unmade"),
    }
}

#[cfg(not(target_os = "linux"))]
fn cgroup(_: u64, _: Option<&[usize]>) -> Result<PathBuf, &'static str> {
    Err("unsupported_os")
}

pub fn child(
    exe: PathBuf,
    args: &[OsString],
    limits: &Limits,
    cap: &Cap,
) -> std::io::Result<Command> {
    let mut command = Command::new(exe);
    command.args(args).kill_on_drop(true);
    held(&mut command, limits, cap)?;
    Ok(command)
}

#[cfg(target_os = "linux")]
fn held(command: &mut Command, limits: &Limits, cap: &Cap) -> std::io::Result<()> {
    use rustix::process::{
        Resource, Rlimit, Signal, getpid, getppid, set_parent_process_death_signal,
        setpriority_process, setrlimit,
    };
    use rustix::thread::{CpuSet, sched_setaffinity};
    let set = limits.cpus.as_ref().map(|cpus| {
        let mut set = CpuSet::new();
        for cpu in cpus {
            set.set(*cpu);
        }
        set
    });
    let nice = limits.nice;
    let (procs, rlimit) = match cap {
        Cap::Cgroup(procs) => (
            Some(std::fs::OpenOptions::new().write(true).open(procs)?),
            None,
        ),
        Cap::Rlimit(bytes) => (None, Some(*bytes)),
        Cap::None => (None, None),
    };
    let parent = getpid();
    unsafe {
        command.pre_exec(move || {
            if let Some(procs) = &procs {
                rustix::io::write(procs, b"0")?;
            }
            if let Some(set) = &set {
                sched_setaffinity(None, set)?;
            }
            if let Some(nice) = nice {
                setpriority_process(None, nice)?;
            }
            if let Some(bytes) = rlimit {
                let most = Rlimit {
                    current: Some(bytes),
                    maximum: Some(bytes),
                };
                setrlimit(Resource::As, most)?;
            }
            set_parent_process_death_signal(Some(Signal::TERM))?;
            orphaned(getppid(), parent)?;
            Ok(())
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn orphaned(
    now: Option<rustix::process::Pid>,
    parent: rustix::process::Pid,
) -> std::io::Result<()> {
    let still = now == Some(parent);
    if still {
        return Ok(());
    }
    Err(rustix::io::Errno::SRCH.into())
}

#[cfg(not(target_os = "linux"))]
fn held(_: &mut Command, _: &Limits, _: &Cap) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn limit_self(limits: &Limits) -> std::io::Result<()> {
    use rustix::process::{Resource, Rlimit, setpriority_process, setrlimit};
    if let Some(cpus) = &limits.cpus {
        crate::cores::pin(cpus)?;
    }
    if let Some(nice) = limits.nice {
        setpriority_process(None, nice)?;
    }
    if let Some(bytes) = limits.memory {
        let most = Rlimit {
            current: Some(bytes),
            maximum: Some(bytes),
        };
        setrlimit(Resource::As, most)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn limit_self(limits: &Limits) -> std::io::Result<()> {
    if limits.unlimited() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a machine's own CPU set, nice and memory cap are set only on Linux",
    ))
}

#[cfg(target_os = "linux")]
pub fn own_group(command: &mut Command) {
    use rustix::fs::Mode;
    use rustix::process::{Signal, getpid, getppid, set_parent_process_death_signal, umask};
    command.process_group(0);
    let parent = getpid();
    unsafe {
        command.pre_exec(move || {
            umask(Mode::from_raw_mode(0o077));
            set_parent_process_death_signal(Some(Signal::KILL))?;
            orphaned(getppid(), parent)?;
            Ok(())
        });
    }
}

#[cfg(target_os = "linux")]
pub fn private_root(root: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    let meta = std::fs::symlink_metadata(root)?;
    let folder = meta.file_type().is_dir();
    let ours = meta.uid() == rustix::process::getuid().as_raw();
    let private = meta.mode() & 0o077 == 0;
    if folder && ours && private {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "{} is not a folder this user alone may enter (owned by it, no access for group or others, no symlink)",
        root.display()
    )))
}

#[cfg(not(target_os = "linux"))]
pub fn private_root(_: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a machine runs work only on Linux, where it keeps each run's folder private and its \
         files under umask 077",
    ))
}

#[cfg(target_os = "linux")]
pub fn alive(pid: u32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn alive(_: u32) -> bool {
    true
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn own_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
pub fn own_group(_: &mut Command) {}

#[cfg(target_os = "linux")]
pub fn kill_group(pid: u32) -> std::io::Result<()> {
    use rustix::process::{Signal, kill_process_group};
    Ok(kill_process_group(pid_of(pid)?, Signal::KILL)?)
}

#[cfg(not(target_os = "linux"))]
pub fn kill_group(_: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a process group is stopped only on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn terminate(pid: u32) -> std::io::Result<()> {
    use rustix::process::{Signal, kill_process};
    Ok(kill_process(pid_of(pid)?, Signal::TERM)?)
}

#[cfg(not(target_os = "linux"))]
fn terminate(_: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the machine is asked to stop only on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn pid_of(pid: u32) -> std::io::Result<rustix::process::Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| std::io::Error::other(format!("{pid} is no process id")))
}

pub fn supported(limits: &Limits) -> std::io::Result<()> {
    let limited = !limits.unlimited();
    let unsupported = limited && !cfg!(target_os = "linux");
    if unsupported {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "[machine]'s cpus, nice and memory are set on the node's child only on Linux",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn runs_parent(own: &str) -> Option<String> {
    let path = own.lines().find_map(|line| line.strip_prefix("0::"))?;
    let machine = path.strip_suffix(AGENT)?.strip_suffix('/')?;
    let named = machine.rsplit('/').next() == Some(MACHINE);
    named.then(|| machine.to_owned())
}

#[cfg(target_os = "linux")]
pub fn runs_cgroup() -> std::io::Result<Option<String>> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    Ok(runs_parent(&own))
}

#[cfg(not(target_os = "linux"))]
pub fn runs_cgroup() -> std::io::Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
pub fn narrowed() -> std::io::Result<Option<Vec<usize>>> {
    let usable = crate::cores::usable()?;
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online")?;
    let online = crate::cores::CpuList::parse(online.trim()).map_err(std::io::Error::other)?;
    let fewer = online.cpus().iter().any(|cpu| !usable.contains(cpu));
    Ok(fewer.then_some(usable))
}

#[cfg(not(target_os = "linux"))]
pub fn narrowed() -> std::io::Result<Option<Vec<usize>>> {
    Ok(None)
}

pub fn keep(
    workspace: &Workspace,
    http: SocketAddr,
    limits: Limits,
    tips: watch::Receiver<u64>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<JoinHandle<()>> {
    supported(&limits)?;
    let exe = std::env::current_exe()?;
    let node = format!("http://{}", crate::transport::loopback(http));
    let args: Vec<OsString> = vec![
        "--workspace".into(),
        workspace.dir().as_os_str().to_owned(),
        "--node".into(),
        node.into(),
        "machine".into(),
        "run".into(),
        "--child".into(),
    ];
    let capped = cap(&limits);
    Ok(tokio::spawn(kept(
        exe, args, limits, capped, tips, shutdown,
    )))
}

async fn kept(
    exe: PathBuf,
    args: Vec<OsString>,
    limits: Limits,
    capped: Capped,
    mut tips: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempts: u64 = 0;
    loop {
        tips.borrow_and_update();
        attempts += 1;
        let spawned = child(exe.clone(), &args, &limits, &capped.cap).and_then(|mut c| c.spawn());
        match spawned {
            Ok(mut running) => {
                if attempts.is_power_of_two() {
                    limits_logged(attempts, &limits, &capped);
                }
                let exited = tokio::select! {
                    exited = running.wait() => exited,
                    () = down(&mut shutdown) => {
                        stop(&mut running).await;
                        return;
                    }
                };
                let clean = finished_cleanly(&exited);
                if attempts.is_power_of_two() {
                    exit_logged(attempts, exited);
                }
                if clean {
                    attempts = 0;
                }
            }
            Err(error) => {
                if attempts.is_power_of_two() {
                    tracing::warn!(
                        target: TARGET,
                        event = "machine_exited",
                        attempts,
                        %error,
                        reason = "machine_unspawned",
                        "the node could not start its machine; it tries again at the next block"
                    );
                }
            }
        }
        if !next_block(&mut tips, &mut shutdown).await {
            return;
        }
    }
}

fn finished_cleanly(exited: &std::io::Result<std::process::ExitStatus>) -> bool {
    matches!(exited, Ok(status) if status.success())
}

async fn next_block(tips: &mut watch::Receiver<u64>, shutdown: &mut watch::Receiver<bool>) -> bool {
    tips.borrow_and_update();
    tokio::select! {
        changed = tips.changed() => changed.is_ok(),
        () = down(shutdown) => false,
    }
}

async fn down(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|down| *down).await;
}

fn limits_logged(attempts: u64, limits: &Limits, capped: &Capped) {
    tracing::info!(
        target: TARGET,
        event = "machine_limits",
        attempts,
        memory = capped.cap.word(),
        reason = capped.reason,
        cpus = ?limits.cpus,
        nice = ?limits.nice,
        "the node started its machine"
    );
}

fn exit_logged(attempts: u64, exited: std::io::Result<std::process::ExitStatus>) {
    match exited {
        Ok(status) => tracing::warn!(
            target: TARGET,
            event = "machine_exited",
            attempts,
            %status,
            reason = "machine_ended",
            "the node's machine ended; the node starts it again at the next block"
        ),
        Err(error) => tracing::warn!(
            target: TARGET,
            event = "machine_exited",
            attempts,
            %error,
            reason = "machine_unwaited",
            "the node lost its machine; it starts it again at the next block"
        ),
    }
}

async fn stop(running: &mut tokio::process::Child) {
    let asked = running.id().map(terminate);
    let terminated = matches!(asked, Some(Ok(())));
    if !terminated {
        let _ = running.start_kill();
    }
    let _ = running.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_childs_cpus_are_its_own_else_the_rpc_cpus_else_none() {
        let settings = MachineSettings {
            nice: Some(10),
            memory: Some(1 << 30),
            ..MachineSettings::default()
        };
        let rpc = [6, 7];
        let limits = Limits::of(&settings, Some(&rpc));
        assert_eq!(limits.cpus, Some(vec![6, 7]));
        assert_eq!((limits.nice, limits.memory), (Some(10), Some(1 << 30)));
        assert_eq!(Limits::of(&settings, None).cpus, None);
        let pinned = MachineSettings {
            cpus: Some(crate::cores::CpuList::parse("2").unwrap()),
            ..settings
        };
        assert_eq!(Limits::of(&pinned, Some(&rpc)).cpus, Some(vec![2]));
        let unset = cap(&Limits::default());
        assert_eq!((unset.cap.word(), unset.reason), ("none", "memory_unset"));
    }

    #[test]
    fn a_machine_finds_the_cgroup_its_runs_go_under_only_in_the_one_the_node_made() {
        let made = "0::/system.slice/node.service/machine/agent\n";
        assert_eq!(
            runs_parent(made).as_deref(),
            Some("/system.slice/node.service/machine")
        );
        assert_eq!(runs_parent("0::/user.slice/session-2.scope\n"), None);
        assert_eq!(runs_parent("0::/x/agent\n"), None);
        assert_eq!(runs_parent("12:memory:/machine/agent\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_child_that_finished_cleanly_starts_its_count_again() {
        use std::os::unix::process::ExitStatusExt as _;
        let ended = |raw| Ok(std::process::ExitStatus::from_raw(raw));
        assert!(finished_cleanly(&ended(0)));
        assert!(!finished_cleanly(&ended(1 << 8)));
        assert!(!finished_cleanly(&ended(9)));
        assert!(!finished_cleanly(&Err(std::io::Error::other("lost"))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_work_root_is_made_private_and_one_that_is_not_ours_alone_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        private_root(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        private_root(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_root(&root).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(private_root(&link).is_err());
        assert!(alive(std::process::id()));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_child_runs_on_its_cpus_at_its_nice_under_its_address_space_limit() {
        let first = crate::cores::usable().unwrap()[0];
        let memory: u64 = 1 << 34;
        let limits = Limits {
            cpus: Some(vec![first]),
            nice: Some(19),
            memory: Some(memory),
        };
        let script = "grep Cpus_allowed_list /proc/self/status; \
                      cut -d' ' -f19 /proc/self/stat; ulimit -v";
        let args: [OsString; 2] = ["-c".into(), script.into()];
        let mut command = child("sh".into(), &args, &limits, &Cap::Rlimit(memory)).unwrap();
        command.stdout(std::process::Stdio::piped());
        let output = command.output().await.unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                format!("Cpus_allowed_list:\t{first}"),
                "19".to_owned(),
                (memory / 1024).to_string()
            ]
        );
    }
}
