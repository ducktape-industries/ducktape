//! Where a run's work happens: its folder (the work tree, its inputs, `out` and the log) and the
//! process that runs it, a shell command in its own process group or a podman container, and how
//! each one is stopped and cleared away.
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use abi::BlobId;
use noded::Backend;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use super::Talk;
use super::decide::{End, Ending};
use crate::Sentence as _;

const GIT_REMOTE: &str = "git-remote-duck";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub image: Option<String>,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub inputs: Vec<Input>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub out: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Forge {
        path: String,
        repo: String,
        commit: String,
    },
    Files {
        path: String,
        files: String,
        snapshot: Value,
    },
    Blob {
        path: String,
        blob: BlobId,
    },
    Scratch {
        path: String,
        memory: bool,
    },
}

impl Input {
    fn path(&self) -> &str {
        match self {
            Input::Forge { path, .. }
            | Input::Files { path, .. }
            | Input::Blob { path, .. }
            | Input::Scratch { path, .. } => path,
        }
    }
}

pub struct Forge {
    pub chain: String,
    pub node: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kill {
    Group(Option<u32>),
    Container(Container),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub name: String,
    pub manager: Vec<String>,
}

impl Container {
    fn args(&self, verb: &[&str]) -> Vec<String> {
        let mut args = self.manager.clone();
        args.extend(verb.iter().map(|word| word.to_string()));
        args.push(self.name.clone());
        args
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fence {
    pub cgroup: Option<String>,
    pub cpus: Option<String>,
}

impl Fence {
    fn manager(&self) -> Vec<String> {
        match self.cgroup {
            Some(_) => vec!["--cgroup-manager".into(), "cgroupfs".into()],
            None => Vec::new(),
        }
    }
}

impl Workload {
    pub fn read(workload: &Value) -> Result<Workload, String> {
        let spec = &workload["spec"];
        if spec.is_null() {
            return Err("fleet holds no such workload".into());
        }
        let env = spec["env"]
            .as_array()
            .map(|pairs| pairs.iter().map(pair).collect::<Result<Vec<_>, String>>())
            .transpose()?
            .unwrap_or_default();
        let inputs = spec["inputs"]
            .as_array()
            .map(|inputs| inputs.iter().map(input).collect::<Result<Vec<_>, String>>())
            .transpose()?
            .unwrap_or_default();
        Ok(Workload {
            image: optional(&spec["image"]),
            command: texts(&spec["command"]),
            env,
            workdir: optional(&spec["workdir"]),
            inputs,
            cpu: optional(&spec["asks"]["cpu"]),
            memory: optional(&spec["asks"]["memory"]),
            out: optional(&spec["out"]),
        })
    }
}

fn optional(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn texts(value: &Value) -> Vec<String> {
    let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
    items.iter().filter_map(optional).collect()
}

fn text(value: &Value, field: &str) -> Result<String, String> {
    optional(&value[field]).ok_or_else(|| format!("a workload input has no {field}"))
}

fn pair(value: &Value) -> Result<(String, String), String> {
    match value.as_array().map(Vec::as_slice) {
        Some([name, value]) => match (name.as_str(), value.as_str()) {
            (Some(name), Some(value)) => Ok((name.to_owned(), value.to_owned())),
            _ => Err(format!("an environment entry is no pair of texts: {value}")),
        },
        _ => Err(format!("an environment entry is no pair: {value}")),
    }
}

fn variant(value: &Value) -> Result<(&str, &Value), String> {
    let object = value.as_object().filter(|object| object.len() == 1);
    let named = object.and_then(|object| object.iter().next());
    named
        .map(|(name, body)| (name.as_str(), body))
        .ok_or_else(|| format!("{value} is no variant"))
}

fn input(value: &Value) -> Result<Input, String> {
    let (name, body) = variant(value)?;
    match name {
        "Forge" => Ok(Input::Forge {
            path: text(body, "path")?,
            repo: text(body, "repo")?,
            commit: text(body, "commit")?,
        }),
        "Files" => Ok(Input::Files {
            path: text(body, "path")?,
            files: text(body, "files")?,
            snapshot: body["snapshot"].clone(),
        }),
        "Blob" => Ok(Input::Blob {
            path: text(body, "path")?,
            blob: blob_of(&body["blob"])?,
        }),
        "Scratch" => Ok(Input::Scratch {
            path: text(body, "path")?,
            memory: body["memory"].as_bool().unwrap_or(false),
        }),
        other => Err(format!("an input this machine does not know: {other}")),
    }
}

pub fn blob_of(value: &Value) -> Result<BlobId, String> {
    let (kind, digest) = variant(value)?;
    let digest = digest.as_str().ok_or("a blob id's digest is no text")?;
    crate::blob_id(&format!("{}:{digest}", kind.to_lowercase()))
}

pub fn blob_json(id: &BlobId) -> Value {
    match id {
        BlobId::Sha256(digest) => json!({ "Sha256": hex::encode(digest) }),
        BlobId::Sha1(digest) => json!({ "Sha1": hex::encode(digest) }),
    }
}

pub fn refuses(backend: Backend, workload: &Workload, git: bool) -> Option<String> {
    let forges = workload
        .inputs
        .iter()
        .any(|input| matches!(input, Input::Forge { .. }));
    match (backend, &workload.image) {
        (Backend::Shell, Some(_)) => {
            return Some("this machine runs work as a shell, which runs no image".into());
        }
        (Backend::Podman, None) => {
            return Some("this machine runs work in podman, which needs an image".into());
        }
        (Backend::Shell, None) | (Backend::Podman, Some(_)) => {}
    }
    let unclonable = forges && !git;
    unclonable
        .then(|| format!("{GIT_REMOTE} is not on this machine's PATH, so it clones no forge input"))
}

pub fn clones() -> bool {
    on_path(GIT_REMOTE).is_some()
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|found| found.is_file())
}

fn resolved(program: &str) -> PathBuf {
    let named = program.contains('/');
    if named {
        return PathBuf::from(program);
    }
    on_path(program).unwrap_or_else(|| PathBuf::from(program))
}

pub fn inside(root: &Path, path: &str) -> Result<PathBuf, String> {
    let mut at = root.to_path_buf();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => at.push(part),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!("{path} reaches outside the run's work tree"));
            }
        }
    }
    Ok(at)
}

pub fn fresh(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    std::fs::create_dir_all(dir.join("work")).sentence()?;
    std::fs::create_dir_all(dir.join("out")).sentence()
}

pub async fn start(
    backend: Backend,
    dir: &Path,
    name: &str,
    workload: &Workload,
    talk: &Talk,
    forge: &Forge,
    fence: &Fence,
) -> Result<(Child, Kill), String> {
    if workload.command.is_empty() {
        return Err("the workload names no command".into());
    }
    let made = dir.to_path_buf();
    blocking(move || fresh(&made)).await?;
    let log = File::create(dir.join("log")).sentence()?;
    for (index, input) in workload.inputs.iter().enumerate() {
        if let Some(at) = host(backend, dir, index, input)? {
            place(input, &at, talk, forge, &log).await?;
        }
    }
    launch(backend, dir, name, workload, &log, fence)
}

fn host(
    backend: Backend,
    dir: &Path,
    index: usize,
    input: &Input,
) -> Result<Option<PathBuf>, String> {
    match (backend, input) {
        (Backend::Podman, Input::Scratch { memory: true, .. }) => Ok(None),
        (Backend::Podman, _) => Ok(Some(dir.join("mounts").join(index.to_string()))),
        (Backend::Shell, input) => inside(&dir.join("work"), input.path()).map(Some),
    }
}

async fn place(
    input: &Input,
    at: &Path,
    talk: &Talk,
    forge: &Forge,
    log: &File,
) -> Result<(), String> {
    match input {
        Input::Forge { repo, commit, .. } => clone(at, repo, commit, forge, log).await,
        Input::Files {
            files, snapshot, ..
        } => read_file(at, files, snapshot, talk).await,
        Input::Blob { blob, .. } => fetch_blob(at, *blob, talk).await,
        Input::Scratch { .. } => std::fs::create_dir_all(at).sentence(),
    }
}

async fn clone(
    at: &Path,
    repo: &str,
    commit: &str,
    forge: &Forge,
    log: &File,
) -> Result<(), String> {
    let url = format!("duck://{}/forge/{repo}", forge.chain);
    let node = format!("duck.{}.node={}", forge.chain, forge.node);
    let mut cloning = Command::new("git");
    cloning
        .args(["-c", &node, "clone", "--quiet", &url])
        .arg(at);
    git(cloning, log, repo).await?;
    let mut checkout = Command::new("git");
    checkout.arg("-C").arg(at).args([
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "--quiet",
        commit,
    ]);
    git(checkout, log, repo).await
}

async fn git(mut command: Command, log: &File, repo: &str) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone().sentence()?)
        .stderr(log.try_clone().sentence()?);
    let status = command.status().await.sentence()?;
    if status.success() {
        return Ok(());
    }
    Err(format!("git failed on forge input {repo}: {status}"))
}

async fn read_file(at: &Path, files: &str, snapshot: &Value, talk: &Talk) -> Result<(), String> {
    let stat = json!({ "Stat": { "path": files, "at": snapshot } });
    let stated = talk.files_ask(stat).await?;
    let size = stated["Stat"]["entry"]["node"]["File"]["size"]
        .as_u64()
        .ok_or_else(|| format!("{files} is no file in files"))?;
    let read = json!({ "Read": { "path": files, "at": snapshot, "offset": 0, "len": size } });
    let answered = talk.files_ask(read).await?;
    let contents = &answered["Contents"];
    let whole = contents["eof"] == json!(true);
    if !whole {
        return Err(format!("{files} did not come whole in one answer"));
    }
    let bytes = hex::decode(contents["bytes"].as_str().unwrap_or_default()).sentence()?;
    let at = at.to_path_buf();
    blocking(move || written(&at, &bytes)).await
}

async fn fetch_blob(at: &Path, id: BlobId, talk: &Talk) -> Result<(), String> {
    let framed = talk
        .client
        .blob(id)
        .await
        .sentence()?
        .ok_or_else(|| format!("the node holds no blob {}", crate::blob_text(&id)))?;
    let at = at.to_path_buf();
    blocking(move || {
        let (_, body) = blob::parse(&framed)
            .ok_or_else(|| format!("blob {} is not framed", crate::blob_text(&id)))?;
        written(&at, body)
    })
    .await
}

pub async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("the machine's disk work was lost: {error}"))?
}

fn written(at: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).sentence()?;
    }
    std::fs::write(at, bytes).sentence()
}

pub fn launch(
    backend: Backend,
    dir: &Path,
    name: &str,
    workload: &Workload,
    log: &File,
    fence: &Fence,
) -> Result<(Child, Kill), String> {
    match backend {
        Backend::Shell => shell(dir, workload, log),
        Backend::Podman => podman(dir, name, workload, log, fence),
    }
}

fn shell(dir: &Path, workload: &Workload, log: &File) -> Result<(Child, Kill), String> {
    let work = dir.join("work");
    let cwd = match &workload.workdir {
        Some(path) => inside(&work, path)?,
        None => work.clone(),
    };
    std::fs::create_dir_all(&cwd).sentence()?;
    let (program, args) = workload
        .command
        .split_first()
        .ok_or("the workload names no command")?;
    let mut command = Command::new(resolved(program));
    command
        .args(args)
        .current_dir(&cwd)
        .env_clear()
        .envs(workload.env.iter().map(|(name, value)| (name, value)))
        .env("OUT", dir.join("out"))
        .env("WORK", &work)
        .stdin(Stdio::null())
        .stdout(log.try_clone().sentence()?)
        .stderr(log.try_clone().sentence()?)
        .kill_on_drop(true);
    noded::machine::own_group(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("{program} does not start: {error}"))?;
    let kill = Kill::Group(child.id());
    Ok((child, kill))
}

fn podman(
    dir: &Path,
    name: &str,
    workload: &Workload,
    log: &File,
    fence: &Fence,
) -> Result<(Child, Kill), String> {
    let args = podman_args(dir, name, workload, fence)?;
    let mut command = Command::new("podman");
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().sentence()?)
        .stderr(log.try_clone().sentence()?)
        .kill_on_drop(true);
    noded::machine::own_group(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("podman does not start: {error}"))?;
    let container = Container {
        name: name.to_owned(),
        manager: fence.manager(),
    };
    Ok((child, Kill::Container(container)))
}

fn podman_args(
    dir: &Path,
    name: &str,
    workload: &Workload,
    fence: &Fence,
) -> Result<Vec<String>, String> {
    let image = workload
        .image
        .as_ref()
        .ok_or("podman runs an image, and the workload names none")?;
    let mut args = fence.manager();
    args.extend([
        "run".into(),
        "--rm".into(),
        "--name".into(),
        name.to_owned(),
    ]);
    if let Some(cgroup) = &fence.cgroup {
        args.extend(["--cgroup-parent".into(), cgroup.clone()]);
    }
    if let Some(cpus) = &fence.cpus {
        args.extend(["--cpuset-cpus".into(), cpus.clone()]);
    }
    args.extend(["-v".into(), format!("{}:/out", dir.join("out").display())]);
    for (index, input) in workload.inputs.iter().enumerate() {
        match host(Backend::Podman, dir, index, input)? {
            Some(at) => args.extend(["-v".into(), format!("{}:{}", at.display(), input.path())]),
            None => args.extend(["--tmpfs".into(), input.path().to_owned()]),
        }
    }
    if let Some(workdir) = &workload.workdir {
        args.extend(["-w".into(), workdir.clone()]);
    }
    if let Some(cpu) = &workload.cpu {
        args.extend(["--cpus".into(), cpus(cpu)?]);
    }
    if let Some(memory) = &workload.memory {
        let bytes = noded::memory_quantity(memory)
            .ok_or_else(|| format!("{memory} is no memory quantity"))?;
        args.extend(["--memory".into(), bytes.to_string()]);
    }
    for (variable, value) in &workload.env {
        args.extend(["-e".into(), format!("{variable}={value}")]);
    }
    args.push(image.clone());
    args.extend(workload.command.iter().cloned());
    Ok(args)
}

pub fn cpus(quantity: &str) -> Result<String, String> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let refused = || format!("{quantity} is no CPU quantity");
    match quantity.strip_suffix('m') {
        Some(milli) if digits(milli) => {
            let milli: u64 = milli.parse().map_err(|_| refused())?;
            Ok(format!("{}.{:03}", milli / 1000, milli % 1000))
        }
        Some(_) => Err(refused()),
        None => {
            let (whole, fraction) = quantity.split_once('.').unwrap_or((quantity, "0"));
            let decimal = digits(whole) && digits(fraction);
            decimal.then(|| quantity.to_owned()).ok_or_else(refused)
        }
    }
}

enum Waited {
    Exited(std::io::Result<std::process::ExitStatus>),
    Stopped(Ending),
}

pub async fn wait(mut child: Child, kill: Kill, stop: oneshot::Receiver<Ending>) -> (End, Ending) {
    let waited = tokio::select! {
        status = child.wait() => Waited::Exited(status),
        ending = stop => Waited::Stopped(ending.unwrap_or(Ending::Dropped)),
    };
    let (status, ending) = match waited {
        Waited::Exited(status) => (status, Ending::Running),
        Waited::Stopped(ending) => {
            killed(&kill, &mut child).await;
            (child.wait().await, ending)
        }
    };
    cleared(&kill).await;
    let end = match status {
        Ok(status) => status.code().map_or(End::Signalled, End::Exited),
        Err(error) => End::Unwaited(error.to_string()),
    };
    (end, ending)
}

async fn killed(kill: &Kill, child: &mut Child) {
    let ended = match kill {
        Kill::Group(Some(pid)) => noded::machine::kill_group(*pid).is_ok(),
        Kill::Group(None) => false,
        Kill::Container(container) => podman_quiet(container.args(&["kill"])).await,
    };
    if !ended {
        let _ = child.start_kill();
    }
}

pub async fn abandoned(kill: &Kill) {
    if let Kill::Container(container) = kill {
        podman_quiet(container.args(&["kill"])).await;
    }
    cleared(kill).await;
}

async fn cleared(kill: &Kill) {
    match kill {
        Kill::Group(Some(pid)) => {
            let _ = noded::machine::kill_group(*pid);
        }
        Kill::Group(None) => {}
        Kill::Container(container) => {
            podman_quiet(container.args(&["rm", "-f"])).await;
        }
    }
}

async fn podman_quiet<S: AsRef<std::ffi::OsStr>>(args: Vec<S>) -> bool {
    Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn listing(fence: &Fence, prefix: &str) -> Vec<String> {
    let mut args = fence.manager();
    args.extend(["ps", "-a", "--format", "{{.Names}}", "--filter"].map(str::to_owned));
    args.push(format!("name=^{prefix}"));
    args
}

fn removal(fence: &Fence, names: &[&str]) -> Vec<String> {
    let mut args = fence.manager();
    args.extend(["rm", "-f"].map(str::to_owned));
    args.extend(names.iter().map(|name| name.to_string()));
    args
}

pub async fn swept(prefix: &str, fence: &Fence) -> Result<usize, String> {
    let listed = Command::new("podman")
        .args(listing(fence, prefix))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .sentence()?;
    if !listed.status.success() {
        return Err(format!("podman ps failed: {}", listed.status));
    }
    let text = String::from_utf8_lossy(&listed.stdout);
    let names: Vec<&str> = text
        .lines()
        .filter(|name| left(name, prefix, noded::machine::alive))
        .collect();
    if names.is_empty() {
        return Ok(0);
    }
    let removed = podman_quiet(removal(fence, &names)).await;
    if !removed {
        return Err(format!(
            "podman rm -f of {} leftover containers failed",
            names.len()
        ));
    }
    Ok(names.len())
}

fn left(name: &str, prefix: &str, alive: impl Fn(u32) -> bool) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let owner = rest
        .strip_prefix('p')
        .and_then(|rest| rest.split('-').next())
        .and_then(|pid| pid.parse::<u32>().ok());
    match owner {
        Some(pid) => !alive(pid),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::decide::state_of;

    #[test]
    fn the_sweep_leaves_the_containers_of_a_machine_process_still_running() {
        let prefix = "ducktape-ab12cd34-31-";
        let alive = |pid| pid == 700;
        assert!(!left("ducktape-ab12cd34-31-p700-5-1", prefix, alive));
        assert!(left("ducktape-ab12cd34-31-p699-5-1", prefix, alive));
        assert!(left("ducktape-ab12cd34-31-5-1", prefix, alive));
        assert!(!left("ducktape-ab12cd34-32-p699-5-1", prefix, alive));
    }

    fn workload(command: &[&str]) -> Workload {
        Workload {
            image: None,
            command: command.iter().map(|part| part.to_string()).collect(),
            env: vec![("GREETING".into(), "hi".into())],
            workdir: None,
            inputs: Vec::new(),
            cpu: None,
            memory: None,
            out: None,
        }
    }

    fn opened(dir: &Path) -> File {
        fresh(dir).unwrap();
        File::create(dir.join("log")).unwrap()
    }

    #[tokio::test]
    async fn a_shell_run_sees_only_its_own_environment_and_ends_as_its_command_exits() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("1-1");
        let log = opened(&dir);
        let script = "echo \"$GREETING\"; echo \"home=$HOME cargo=$CARGO_MANIFEST_DIR\"; \
                      echo made > \"$OUT/x\"; exit 3";
        let shell = workload(&["sh", "-c", script]);
        let (child, kill) = launch(
            Backend::Shell,
            &dir,
            "unused",
            &shell,
            &log,
            &Fence::default(),
        )
        .unwrap();
        let (_kept, stop) = oneshot::channel();
        let (end, ending) = wait(child, kill, stop).await;
        assert_eq!((end.clone(), ending), (End::Exited(3), Ending::Running));
        assert_eq!(state_of(&end, ending).0, "Failed");
        let logged = std::fs::read_to_string(dir.join("log")).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines[0], "hi");
        assert_eq!(lines[1], "home= cargo=", "{logged}");
        assert_eq!(
            std::fs::read_to_string(dir.join("out/x")).unwrap(),
            "made\n"
        );
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("out/x"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "a run writes under umask 077");
        }
    }

    #[tokio::test]
    async fn a_stopped_shell_run_ends_with_its_whole_group_and_reads_as_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("2-1");
        let log = opened(&dir);
        let script = "tail -f /dev/null & wait";
        let shell = workload(&["sh", "-c", script]);
        let (child, kill) = launch(
            Backend::Shell,
            &dir,
            "unused",
            &shell,
            &log,
            &Fence::default(),
        )
        .unwrap();
        let (stopper, stop) = oneshot::channel();
        stopper.send(Ending::Stopped).unwrap();
        let (end, ending) = wait(child, kill, stop).await;
        assert_eq!((end.clone(), ending), (End::Signalled, Ending::Stopped));
        assert_eq!(state_of(&end, ending).0, "Cancelled");
    }

    #[tokio::test]
    async fn a_run_whose_stopper_goes_away_is_stopped_as_dropped() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("3-1");
        let log = opened(&dir);
        let shell = workload(&["sh", "-c", "tail -f /dev/null & wait"]);
        let (child, kill) = launch(
            Backend::Shell,
            &dir,
            "unused",
            &shell,
            &log,
            &Fence::default(),
        )
        .unwrap();
        let (stopper, stop) = oneshot::channel::<Ending>();
        drop(stopper);
        let (end, ending) = wait(child, kill, stop).await;
        assert_eq!((end, ending), (End::Signalled, Ending::Dropped));
    }

    #[test]
    fn a_workload_reads_from_fleets_json() {
        let row = json!({
            "id": 4,
            "by": 11,
            "height": 9,
            "spec": {
                "kind": "Job",
                "image": null,
                "command": ["make", "check"],
                "env": [["A", "1"]],
                "workdir": "/src",
                "inputs": [
                    {"Forge": {"path": "/src", "repo": "core", "commit": "abc"}},
                    {"Blob": {"path": "/in/b", "blob": {"Sha256": "00".repeat(32)}}},
                    {"Scratch": {"path": "/tmp", "memory": true, "size": null}}
                ],
                "asks": {"cpu": "500m", "memory": "1Gi"},
                "on": null,
                "out": null,
                "on_loss": "Retry",
                "notify": null
            }
        });
        let read = Workload::read(&row).unwrap();
        assert_eq!(read.command, ["make", "check"]);
        assert_eq!(read.env, [("A".to_owned(), "1".to_owned())]);
        assert_eq!(read.workdir.as_deref(), Some("/src"));
        assert_eq!(read.inputs.len(), 3);
        assert_eq!(
            read.inputs[1],
            Input::Blob {
                path: "/in/b".into(),
                blob: BlobId::Sha256([0; 32]),
            }
        );
        assert_eq!(
            (read.cpu.as_deref(), read.memory.as_deref()),
            (Some("500m"), Some("1Gi"))
        );
        assert_eq!(
            blob_json(&BlobId::Sha256([0; 32])),
            json!({"Sha256": "00".repeat(32)})
        );
        assert!(Workload::read(&json!(null)).is_err());
    }

    #[test]
    fn a_machine_declines_what_its_backend_cannot_run() {
        let plain = workload(&["true"]);
        let imaged = Workload {
            image: Some("toolchain".into()),
            ..plain.clone()
        };
        assert!(refuses(Backend::Shell, &plain, false).is_none());
        assert!(refuses(Backend::Shell, &imaged, true).is_some());
        assert!(refuses(Backend::Podman, &plain, true).is_some());
        assert!(refuses(Backend::Podman, &imaged, true).is_none());
        let cloning = Workload {
            inputs: vec![Input::Forge {
                path: "/src".into(),
                repo: "core".into(),
                commit: "abc".into(),
            }],
            ..plain
        };
        let refused = refuses(Backend::Shell, &cloning, false).unwrap();
        assert!(refused.contains(GIT_REMOTE), "{refused}");
        assert!(refuses(Backend::Shell, &cloning, true).is_none());
    }

    #[test]
    fn an_input_lands_inside_the_work_tree_and_never_above_it() {
        let root = Path::new("/w");
        assert_eq!(inside(root, "/src/a").unwrap(), Path::new("/w/src/a"));
        assert_eq!(inside(root, "out").unwrap(), Path::new("/w/out"));
        assert!(inside(root, "/src/../../key").is_err());
    }

    #[test]
    fn a_podman_run_goes_under_the_machines_cgroup_and_cpus_when_the_machine_has_them() {
        let imaged = Workload {
            image: Some("toolchain".into()),
            ..workload(&["make"])
        };
        let dir = Path::new("/w/1-1");
        let open = podman_args(dir, "n", &imaged, &Fence::default()).unwrap();
        assert_eq!(&open[..4], ["run", "--rm", "--name", "n"]);
        let fenced_flag = |arg: &String| arg.starts_with("--cgroup") || arg == "--cpuset-cpus";
        assert!(!open.iter().any(fenced_flag), "{open:?}");
        let fence = Fence {
            cgroup: Some("/system.slice/node.service/machine".into()),
            cpus: Some("6,7".into()),
        };
        let fenced = podman_args(dir, "n", &imaged, &fence).unwrap();
        assert_eq!(
            &fenced[..10],
            [
                "--cgroup-manager",
                "cgroupfs",
                "run",
                "--rm",
                "--name",
                "n",
                "--cgroup-parent",
                "/system.slice/node.service/machine",
                "--cpuset-cpus",
                "6,7"
            ]
        );
    }

    #[test]
    fn every_podman_call_for_a_fenced_container_names_the_manager_its_run_did() {
        let fence = Fence {
            cgroup: Some("/n/machine".into()),
            cpus: None,
        };
        let container = Container {
            name: "n".into(),
            manager: fence.manager(),
        };
        let manager = ["--cgroup-manager", "cgroupfs"];
        assert_eq!(
            container.args(&["kill"]),
            [manager[0], manager[1], "kill", "n"]
        );
        assert_eq!(
            container.args(&["rm", "-f"]),
            [manager[0], manager[1], "rm", "-f", "n"]
        );
        assert_eq!(&listing(&fence, "p-")[..3], [manager[0], manager[1], "ps"]);
        assert_eq!(
            removal(&fence, &["a"]),
            [manager[0], manager[1], "rm", "-f", "a"]
        );
        let ran = podman_args(
            Path::new("/w/1-1"),
            "n",
            &Workload {
                image: Some("toolchain".into()),
                ..workload(&["make"])
            },
            &fence,
        )
        .unwrap();
        assert_eq!(&ran[..2], manager);
        let open = Fence::default();
        assert_eq!(listing(&open, "p-")[0], "ps");
        assert_eq!(removal(&open, &["a"]), ["rm", "-f", "a"]);
        let unfenced = Container {
            name: "n".into(),
            manager: open.manager(),
        };
        assert_eq!(unfenced.args(&["kill"]), ["kill", "n"]);
    }

    #[test]
    fn a_cpu_quantity_reads_as_podmans_decimal() {
        assert_eq!(cpus("500m").unwrap(), "0.500");
        assert_eq!(cpus("2").unwrap(), "2");
        assert_eq!(cpus("1.5").unwrap(), "1.5");
        assert!(cpus("lots").is_err());
    }
}
