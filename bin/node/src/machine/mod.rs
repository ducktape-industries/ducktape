//! `ducktape machine`: the machines `[machine]` names, talking to fleet in JSON through its own
//! schema. Each says it is ready, claims what fleet assigns it, runs it on its backend in a task of
//! its own, beats with its host's load, and reports each end once the run's outputs are in files.
mod backend;
mod decide;
mod out;
mod pods;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use borsh_json::Interface;
use commonware_cryptography::{Signer as _, ed25519};
use futures::{Stream, StreamExt as _};
use host::{Layer, RECENT};
use node::{Frame, Preconfirm};
use noded::wire::Change;
use noded::{Backend, Client, MachineEntry, MachineSettings, Settings, Workspace};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::{Sentence as _, json as said};
use backend::{Forge, Workload};
use decide::{Act, Ending, Held, Local};

const FLEET: &str = "ducktape-industries/fleet";
const FILES: &str = "files";
const TARGET: &str = "ducktape::machine";

pub fn init(workspace: &Workspace) -> Result<(), String> {
    let key = workspace
        .machine_key_or_create(rand_core::UnwrapErr(rand::rngs::SysRng))
        .sentence()?;
    println!("{}", hex::encode(key.public_key().as_ref()));
    Ok(())
}

pub fn consent(
    workspace: &Workspace,
    client: Client,
    account: u64,
    network: Option<String>,
    expires: Option<u64>,
) -> Result<(), String> {
    let key = workspace.machine_key().sentence()?;
    let asked = said::Consenting {
        account,
        key: None,
        scheme: abi::Scheme::Ed25519,
        network,
        expires,
    };
    let consent = crate::client_runtime()?.block_on(said::consent(&client, &key, asked))?;
    said::print(&consent);
    Ok(())
}

pub fn run(
    workspace: &Workspace,
    client: Client,
    config: Option<PathBuf>,
    child: bool,
) -> Result<(), String> {
    let settings = settings(workspace, config)?;
    if !child {
        let limits = noded::machine::Limits::of(&settings, None);
        noded::machine::limit_self(&limits).sentence()?;
    }
    let key = workspace.machine_key().sentence()?;
    let _logs = noded::Logs::install(noded::DEFAULT_LOG)?;
    crate::client_runtime()?.block_on(serve(workspace, client, key, settings))
}

fn work_root(workspace: &Workspace, settings: &MachineSettings) -> Result<PathBuf, String> {
    let work = match &settings.work {
        Some(work) => work.clone(),
        None => beside(workspace.dir())?,
    };
    noded::machine::private_root(&work).sentence()?;
    let work = std::fs::canonicalize(&work).sentence()?;
    let workspace = std::fs::canonicalize(workspace.dir()).sentence()?;
    let overlaps = work.starts_with(&workspace) || workspace.starts_with(&work);
    if overlaps {
        return Err(format!(
            "[machine] work {} and the workspace {} overlap, and a shell run could read its keys",
            work.display(),
            workspace.display()
        ));
    }
    Ok(work)
}

fn beside(workspace: &std::path::Path) -> Result<PathBuf, String> {
    let name = workspace
        .file_name()
        .ok_or_else(|| format!("the workspace {} has no folder name", workspace.display()))?;
    let mut work = name.to_owned();
    work.push("-machine");
    Ok(workspace.with_file_name(work))
}

fn fence(settings: &MachineSettings) -> Result<backend::Fence, String> {
    let podman = settings
        .machines
        .iter()
        .any(|machine| machine.backend == Backend::Podman);
    if !podman {
        return Ok(backend::Fence::default());
    }
    let cgroup = noded::machine::runs_cgroup().sentence()?;
    let cpus = noded::machine::narrowed().sentence()?;
    fenced(settings.memory, cgroup, cpus)
}

fn fenced(
    memory: Option<u64>,
    cgroup: Option<String>,
    cpus: Option<Vec<usize>>,
) -> Result<backend::Fence, String> {
    let uncapped = memory.is_some() && cgroup.is_none();
    if uncapped {
        return Err(
            "[machine] memory bounds a podman machine's containers only through the cgroup the \
             node makes for its child, and this machine runs outside one: run it as the node's \
             child where the node may make that cgroup, or leave memory unset"
                .into(),
        );
    }
    let cpus = cpus.map(|cpus| {
        let list: Vec<String> = cpus.iter().map(usize::to_string).collect();
        list.join(",")
    });
    Ok(backend::Fence { cgroup, cpus })
}

#[cfg(test)]
fn settings_of(text: &str) -> MachineSettings {
    Settings::parse(text).unwrap().machine.unwrap()
}

fn settings(workspace: &Workspace, config: Option<PathBuf>) -> Result<MachineSettings, String> {
    let read = match config {
        Some(path) => {
            let text = std::fs::read_to_string(&path).sentence()?;
            Settings::parse(&text)
                .map_err(|reason| format!("{} is refused: {reason}", path.display()))?
        }
        None => workspace.settings().sentence()?,
    };
    let machine = read.machine.ok_or("the settings hold no [machine] table")?;
    if machine.machines.is_empty() {
        return Err("[machine] names no machines".into());
    }
    for entry in &machine.machines {
        reached(entry)?;
    }
    Ok(machine)
}

fn reached(machine: &MachineEntry) -> Result<(), String> {
    match (machine.backend, &machine.service) {
        (Backend::Pods, None) => Err(format!(
            "machine {} runs on the pods service and names no service: name the egress service \
             this network registered for it",
            machine.name
        )),
        (Backend::Shell | Backend::Podman, Some(service)) => Err(format!(
            "machine {} names service {service}, which only a pods machine reaches",
            machine.name
        )),
        (Backend::Pods, Some(_)) | (Backend::Shell | Backend::Podman, None) => Ok(()),
    }
}

fn unpacked_summary(summary: String, unpacked: &[String]) -> String {
    if unpacked.is_empty() {
        return summary;
    }
    format!("{summary}; not packed: {}", unpacked.join("; "))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    Land,
    Pool,
}

pub struct Talk {
    client: Client,
    key: ed25519::PrivateKey,
    fleet_schema: Interface,
    files_schema: Interface,
}

impl Talk {
    async fn fleet(&self, op: Value, landing: Landing) -> Result<Value, String> {
        self.send(FLEET, &self.fleet_schema, op, landing).await
    }

    async fn fleet_ask(&self, query: Value) -> Result<Value, String> {
        self.ask(FLEET, &self.fleet_schema, query).await
    }

    async fn files(&self, op: Value, landing: Landing) -> Result<Value, String> {
        self.send(FILES, &self.files_schema, op, landing).await
    }

    async fn files_ask(&self, query: Value) -> Result<Value, String> {
        self.ask(FILES, &self.files_schema, query).await
    }

    async fn send(
        &self,
        program: &str,
        interface: &Interface,
        op: Value,
        landing: Landing,
    ) -> Result<Value, String> {
        let payload = interface.encode_op(&op).sentence()?;
        let status = self.client.status().await.sentence()?;
        let frame = Frame::sign_as(
            crate::CLIENT,
            &self.key,
            status.network.as_bytes(),
            status.anchor(),
            Some(crate::salt()),
            program,
            payload.clone(),
        );
        let sent = said::Sent {
            interface,
            program,
            payload: &payload,
        };
        match landing {
            Landing::Land => sent.land(&self.client, &frame).await,
            Landing::Pool => sent.submit(&self.client, &frame, Preconfirm::Off).await,
        }
    }

    async fn ask(
        &self,
        program: &str,
        interface: &Interface,
        query: Value,
    ) -> Result<Value, String> {
        let request = interface.encode_query(&query).sentence()?;
        let status = self.client.status().await.sentence()?;
        let answer = said::ask(
            &self.client,
            &self.key,
            &status,
            program,
            request,
            Layer::Confirmed,
        )
        .await?;
        said::reply(interface, program, &answer)
    }

    async fn identity(&self, query: &identity::Query) -> Result<identity::Reply, String> {
        let status = self.client.status().await.sentence()?;
        let program = status
            .roles
            .bound(abi::role::IDENTITY)
            .map_err(|refusal| refusal.sentence)?
            .to_owned();
        let answer = said::ask(
            &self.client,
            &self.key,
            &status,
            &program,
            abi::encode(query),
            Layer::Confirmed,
        )
        .await?;
        abi::decode(&answer).map_err(|refusal| refusal.sentence)
    }

    async fn account(&self) -> Result<u64, String> {
        let asked = identity::Query::OfKey {
            key: self.key.public_key().as_ref().to_vec(),
        };
        match self.identity(&asked).await? {
            identity::Reply::Number(Some(account)) => Ok(account),
            identity::Reply::Number(None) => Err(
                "machine.key acts as no account: make one with identity's create_agent and add \
                 the key to it with add_key"
                    .into(),
            ),
            _ => Err("identity answered OfKey with another reply".into()),
        }
    }

    async fn manager(&self, account: u64) -> Result<u64, String> {
        let asked = identity::Query::Profile { number: account };
        let profile = match self.identity(&asked).await? {
            identity::Reply::Profile(Some(profile)) => profile,
            identity::Reply::Profile(None) => {
                return Err(format!("identity holds no account {account}"));
            }
            _ => return Err("identity answered Profile with another reply".into()),
        };
        match profile.kind {
            identity::Kind::Program { manager, .. } => Ok(manager),
            identity::Kind::User => Ok(account),
            identity::Kind::System(program) => Err(format!(
                "account {account} is program {program}'s own, which no machine.key acts as"
            )),
        }
    }
}

struct Shared {
    talk: Talk,
    forge: Forge,
    clones: bool,
    work: PathBuf,
    containers: String,
    process: u32,
    fence: backend::Fence,
    heard: watch::Sender<u64>,
    block_ms: u64,
}

struct Running {
    machine: String,
    attempt: u32,
    ending: Ending,
    stopper: Option<oneshot::Sender<Ending>>,
    kill: Option<backend::Kill>,
}

enum Done {
    Claimed {
        machine: String,
        height: u64,
    },
    Started {
        run: u64,
        attempt: u32,
        kill: backend::Kill,
    },
    Finished {
        run: u64,
        attempt: u32,
    },
    Failed {
        reason: &'static str,
        run: u64,
        error: String,
    },
    Cleared,
}

const TASK_REASONS: [&str; 5] = [
    "workload_unread",
    "decline_refused",
    "claim_refused",
    "report_refused",
    "task_lost",
];

type Failure = (&'static str, String);

enum Event {
    Changed(Option<noded::Result<Change>>),
    Block(Option<noded::Result<Change>>),
    Done(Option<Done>),
    Joined(Option<Result<(tokio::task::Id, ()), tokio::task::JoinError>>),
    Signalled,
}

enum Flow {
    Serving,
    Stopped,
}

struct Serving {
    shared: Arc<Shared>,
    account: u64,
    manager: u64,
    machines: Vec<MachineEntry>,
    local: BTreeMap<u64, Running>,
    beats: BTreeMap<String, u64>,
    host: Option<decide::Cpu>,
    failures: BTreeMap<&'static str, u64>,
    done: mpsc::UnboundedSender<Done>,
    tasks: JoinSet<()>,
    tasked: std::collections::HashMap<tokio::task::Id, (u64, u32)>,
}

async fn serve(
    workspace: &Workspace,
    client: Client,
    key: ed25519::PrivateKey,
    settings: MachineSettings,
) -> Result<(), String> {
    let fleet_schema = said::interface(workspace, &client, FLEET).await?;
    let probe = decide::beat("", Some(&decide::Load::default()));
    let host = match fleet_schema.encode_op(&probe) {
        Ok(_) => host_sample().ok().map(|(cpu, _)| cpu),
        Err(_) => None,
    };
    let files_schema = said::interface(workspace, &client, FILES).await?;
    let status = client.status().await.sentence()?;
    let chain = format!("{}-{}", status.network, hex::encode(&status.genesis[..4]));
    let forge = Forge {
        chain,
        node: client.base().to_owned(),
    };
    let follow = client.clone();
    let talk = Talk {
        client,
        key,
        fleet_schema,
        files_schema,
    };
    let account = talk.account().await?;
    let manager = talk.manager(account).await?;
    let work = work_root(workspace, &settings)?;
    let containers = format!("ducktape-{}-{account}-", hex::encode(&status.genesis[..4]));
    let process = std::process::id();
    let fence = fence(&settings)?;
    let shared = Arc::new(Shared {
        talk,
        forge,
        clones: backend::clones(),
        work,
        containers,
        process,
        fence,
        heard: watch::channel(0).0,
        block_ms: status.block_time_ms,
    });
    let mut changes = follow.changes(FLEET).await.sentence()?;
    let mut blocks = follow.changes(RECENT).await.sentence()?;
    let (done, mut finished) = mpsc::unbounded_channel();
    let mut serving = Serving {
        shared,
        account,
        manager,
        machines: settings.machines,
        local: BTreeMap::new(),
        beats: BTreeMap::new(),
        host,
        failures: BTreeMap::new(),
        done,
        tasks: JoinSet::new(),
        tasked: std::collections::HashMap::new(),
    };
    serving.swept().await;
    let served = serving
        .served(&mut changes, &mut blocks, &mut finished)
        .await;
    serving.close().await;
    served
}

impl Serving {
    async fn served(
        &mut self,
        changes: &mut (impl Stream<Item = noded::Result<Change>> + Unpin),
        blocks: &mut (impl Stream<Item = noded::Result<Change>> + Unpin),
        finished: &mut mpsc::UnboundedReceiver<Done>,
    ) -> Result<(), String> {
        self.ready().await?;
        self.reconcile().await;
        let mut signalled = Box::pin(crate::signalled().sentence()?);
        loop {
            let event = tokio::select! {
                change = changes.next() => Event::Changed(change),
                block = blocks.next() => Event::Block(block),
                done = finished.recv() => Event::Done(done),
                joined = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    Event::Joined(joined)
                }
                () = &mut signalled => Event::Signalled,
            };
            let flow = match event {
                Event::Changed(change) => self.changed(change).await?,
                Event::Block(block) => self.block(block).await?,
                Event::Done(done) => self.finished(done)?,
                Event::Joined(joined) => self.joined(joined).await,
                Event::Signalled => Flow::Stopped,
            };
            if let Flow::Stopped = flow {
                return Ok(());
            }
        }
    }

    async fn close(&mut self) {
        for running in std::mem::take(&mut self.local).into_values() {
            if let Some(stopper) = running.stopper {
                let _ = stopper.send(Ending::Dropped);
            }
        }
        while self.tasks.join_next().await.is_some() {}
    }

    async fn swept(&mut self) {
        let podman = self
            .machines
            .iter()
            .any(|machine| machine.backend == Backend::Podman);
        if !podman {
            return;
        }
        match backend::swept(&self.shared.containers, &self.shared.fence).await {
            Ok(removed) => tracing::info!(
                target: TARGET,
                event = "machine_swept",
                removed,
                "the machine removed the containers an earlier run of it left"
            ),
            Err(error) => self.failed("containers_unswept", 0, &error),
        }
    }

    fn failed(&mut self, reason: &'static str, run: u64, error: &str) {
        let attempts = self.failures.entry(reason).or_default();
        *attempts += 1;
        if !attempts.is_power_of_two() {
            return;
        }
        tracing::warn!(
            target: TARGET,
            event = "machine_op_failed",
            run,
            reason,
            attempts = *attempts,
            %error,
            "the machine could not do what fleet asked of a run"
        );
    }

    async fn ready(&self) -> Result<(), String> {
        for machine in &self.machines {
            let ready = json!({ "ready": {
                "name": machine.name,
                "labels": machine.labels,
                "slots": machine.slots,
                "serves": serves(machine, self.manager),
            }});
            self.shared.talk.fleet(ready, Landing::Land).await?;
        }
        tracing::info!(
            target: TARGET,
            event = "machine_serving",
            account = self.account,
            machines = self.machines.len(),
            "the machine told fleet it is ready"
        );
        Ok(())
    }

    async fn changed(&mut self, change: Option<noded::Result<Change>>) -> Result<Flow, String> {
        let change = change
            .ok_or("the node closed fleet's change stream")?
            .sentence()?;
        if decide::heard(&change.writes) {
            self.shared
                .heard
                .send_modify(|seen| *seen = seen.wrapping_add(1));
        }
        if decide::touches(&change.writes) {
            self.reconcile().await;
        }
        Ok(Flow::Serving)
    }

    async fn block(&mut self, block: Option<noded::Result<Change>>) -> Result<Flow, String> {
        let height = block
            .ok_or("the node closed its block stream")?
            .sentence()?
            .height;
        let due: Vec<MachineEntry> = self
            .machines
            .iter()
            .filter(|machine| {
                let last = self.beats.get(&machine.name).copied().unwrap_or(0);
                decide::beat_due(height, last)
            })
            .cloned()
            .collect();
        if due.is_empty() {
            return Ok(Flow::Serving);
        }
        let load = self.load();
        for machine in due {
            let reads_its_host = reads_host(machine.backend);
            let carried = load.as_ref().filter(|_| reads_its_host);
            let beat = decide::beat(&machine.name, carried);
            match self.shared.talk.fleet(beat, Landing::Pool).await {
                Ok(_) => {
                    self.failures.remove("beat_unsent");
                    self.beats.insert(machine.name, height);
                }
                Err(error) => self.failed("beat_unsent", 0, &error),
            }
        }
        Ok(Flow::Serving)
    }

    fn load(&mut self) -> Option<decide::Load> {
        let before = self.host?;
        let (now, memory) = match host_sample() {
            Ok(sample) => sample,
            Err(error) => {
                self.failed("host_unread", 0, &error);
                return None;
            }
        };
        self.failures.remove("host_unread");
        self.host = Some(now);
        let started = self.local.values().filter(|run| run.kill.is_some()).count();
        let unstarted = self
            .local
            .values()
            .filter(|run| run.kill.is_none() && run.stopper.is_some())
            .count();
        let workloads = decide::Workloads {
            running: u64::try_from(started).unwrap_or(u64::MAX),
            waiting: u64::try_from(unstarted).unwrap_or(u64::MAX),
        };
        Some(decide::load(before, now, memory, workloads))
    }

    async fn reconcile(&mut self) {
        for machine in self.machines.clone() {
            let held = match self.holds(&machine.name).await {
                Ok(held) => {
                    self.failures.remove("holds_unread");
                    held
                }
                Err(error) => {
                    self.failed("holds_unread", 0, &error);
                    continue;
                }
            };
            let local = self.locals(&machine.name);
            for act in decide::acts(&held, &local) {
                self.act(&machine, act);
            }
        }
    }

    async fn holds(&self, name: &str) -> Result<Vec<Held>, String> {
        let mut held = Vec::new();
        let mut after = Value::Null;
        loop {
            let query = json!({ "holds": {
                "machine": { "account": self.account, "name": name },
                "page": { "after": after, "limit": null },
            }});
            let reply = self.shared.talk.fleet_ask(query).await?;
            let page = &reply["runs"];
            let items = page["items"]
                .as_array()
                .ok_or("fleet answered Holds with no runs")?;
            for item in items {
                held.push(decide::held(item)?);
            }
            match &page["next"] {
                Value::Null => return Ok(held),
                next => after = next.clone(),
            }
        }
    }

    fn locals(&self, name: &str) -> BTreeMap<u64, Local> {
        self.local
            .iter()
            .filter(|(_, run)| run.machine == name)
            .map(|(id, run)| {
                let mine = Local {
                    attempt: run.attempt,
                    ending: run.ending,
                };
                (*id, mine)
            })
            .collect()
    }

    fn act(&mut self, machine: &MachineEntry, act: Act) {
        match act {
            Act::Take { run, attempt } => self.take(machine, run, attempt),
            Act::Stop { run } => self.stop(run, Ending::Stopped),
            Act::Drop { run } => self.stop(run, Ending::Dropped),
            Act::Settle {
                run,
                attempt,
                state,
                summary,
            } => self.settle(machine, run, attempt, state, summary),
        }
    }

    fn take(&mut self, machine: &MachineEntry, run: u64, attempt: u32) {
        let (stopper, stop) = oneshot::channel();
        let running = Running {
            machine: machine.name.clone(),
            attempt,
            ending: Ending::Running,
            stopper: Some(stopper),
            kill: None,
        };
        self.local.insert(run, running);
        let task = Task {
            shared: self.shared.clone(),
            done: self.done.clone(),
            machine: machine.clone(),
            run,
            attempt,
        };
        let spawned = self.tasks.spawn(task.taken(stop));
        self.tasked.insert(spawned.id(), (run, attempt));
    }

    fn settle(
        &mut self,
        machine: &MachineEntry,
        run: u64,
        attempt: u32,
        state: &'static str,
        summary: &'static str,
    ) {
        let settling = Running {
            machine: machine.name.clone(),
            attempt,
            ending: Ending::Running,
            stopper: None,
            kill: None,
        };
        self.local.insert(run, settling);
        let task = Task {
            shared: self.shared.clone(),
            done: self.done.clone(),
            machine: machine.clone(),
            run,
            attempt,
        };
        let spawned = self.tasks.spawn(task.settled(state, summary));
        self.tasked.insert(spawned.id(), (run, attempt));
    }

    async fn joined(
        &mut self,
        joined: Option<Result<(tokio::task::Id, ()), tokio::task::JoinError>>,
    ) -> Flow {
        let (id, lost) = match joined {
            Some(Ok((id, ()))) => (id, None),
            Some(Err(error)) => (error.id(), Some(error.to_string())),
            None => return Flow::Serving,
        };
        let Some((run, attempt)) = self.tasked.remove(&id) else {
            return Flow::Serving;
        };
        if let Some(error) = lost {
            self.failed("task_lost", run, &error);
            self.lost(run, attempt).await;
        }
        Flow::Serving
    }

    async fn lost(&mut self, run: u64, attempt: u32) {
        let current = self
            .local
            .get(&run)
            .filter(|running| running.attempt == attempt);
        let Some(running) = current else {
            return;
        };
        let name = running.machine.clone();
        let kill = running.kill.clone();
        let machine = self
            .machines
            .iter()
            .find(|machine| machine.name == name)
            .cloned();
        match (&kill, &machine) {
            (Some(backend::Kill::Pod { name: pod, calls }), Some(machine)) => {
                let service = machine.service.as_deref().unwrap_or_default();
                pods::abandoned(&self.shared.talk, service, run, attempt, pod, calls).await;
            }
            (Some(kill), _) => backend::abandoned(kill).await,
            (None, _) => {}
        }
        self.local.remove(&run);
        if let Some(machine) = machine {
            self.settle(&machine, run, attempt, "errored", decide::LOST);
        }
    }

    fn stop(&mut self, run: u64, ending: Ending) {
        let Some(running) = self.local.get_mut(&run) else {
            return;
        };
        running.ending = ending;
        if let Some(stopper) = running.stopper.take() {
            let _ = stopper.send(ending);
        }
    }

    fn finished(&mut self, done: Option<Done>) -> Result<Flow, String> {
        let done = done.ok_or("the machine lost its runs' end channel")?;
        match done {
            Done::Claimed { machine, height } => {
                self.beats.insert(machine, height);
            }
            Done::Started { run, attempt, kill } => {
                let current = self
                    .local
                    .get_mut(&run)
                    .filter(|running| running.attempt == attempt);
                if let Some(running) = current {
                    running.kill = Some(kill);
                }
            }
            Done::Finished { run, attempt } => self.forget(run, attempt),
            Done::Failed { reason, run, error } => self.failed(reason, run, &error),
            Done::Cleared => {
                for reason in TASK_REASONS {
                    self.failures.remove(reason);
                }
            }
        }
        Ok(Flow::Serving)
    }

    fn forget(&mut self, run: u64, attempt: u32) {
        let current = self
            .local
            .get(&run)
            .is_some_and(|running| running.attempt == attempt);
        if current {
            self.local.remove(&run);
        }
    }
}

fn reads_host(backend: Backend) -> bool {
    match backend {
        Backend::Shell | Backend::Podman => true,
        Backend::Pods => false,
    }
}

fn host_sample() -> Result<(decide::Cpu, decide::Memory), String> {
    let stat = std::fs::read_to_string("/proc/stat").map_err(|error| error.to_string())?;
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|error| error.to_string())?;
    let cpu = decide::cpu_of(&stat).ok_or("/proc/stat has no cpu line")?;
    let memory =
        decide::memory_of(&meminfo).ok_or("/proc/meminfo has no MemTotal or MemAvailable")?;
    Ok((cpu, memory))
}

fn serves(machine: &MachineEntry, manager: u64) -> Value {
    match (&machine.serves, machine.backend) {
        (Some(accounts), _) => json!({ "listed": accounts }),
        (None, Backend::Shell) => json!({ "listed": [manager] }),
        (None, Backend::Podman | Backend::Pods) => json!("anyone"),
    }
}

struct Task {
    shared: Arc<Shared>,
    done: mpsc::UnboundedSender<Done>,
    machine: MachineEntry,
    run: u64,
    attempt: u32,
}

impl Task {
    async fn taken(self, stop: oneshot::Receiver<Ending>) {
        let outcome = self.took(stop).await;
        self.ended(outcome);
    }

    async fn settled(self, state: &'static str, summary: &'static str) {
        let outcome = self.report(state, summary.to_owned(), None).await;
        self.ended(outcome);
    }

    fn ended(&self, outcome: Result<(), Failure>) {
        let (run, attempt) = (self.run, self.attempt);
        let told = match outcome {
            Ok(()) => Done::Cleared,
            Err((reason, error)) => Done::Failed { reason, run, error },
        };
        let _ = self.done.send(told);
        let _ = self.done.send(Done::Finished { run, attempt });
    }

    async fn took(&self, stop: oneshot::Receiver<Ending>) -> Result<(), Failure> {
        let talk = &self.shared.talk;
        let (run, attempt) = (self.run, self.attempt);
        let asked = json!({ "workload": { "id": run } });
        let reply = talk
            .fleet_ask(asked)
            .await
            .map_err(|error| ("workload_unread", error))?;
        let workload =
            Workload::read(&reply["workload"]).map_err(|error| ("workload_unread", error))?;
        if let Some(why) = backend::refuses(self.machine.backend, &workload, self.shared.clones) {
            let decline = json!({ "decline": { "run": run, "attempt": attempt, "why": why } });
            talk.fleet(decline, Landing::Land)
                .await
                .map_err(|error| ("decline_refused", error))?;
            return Ok(());
        }
        let claim = json!({ "claim": { "run": run, "attempt": attempt } });
        let claimed = talk
            .fleet(claim, Landing::Land)
            .await
            .map_err(|error| ("claim_refused", error))?;
        if let Some(height) = claimed["landed"].as_u64() {
            let machine = self.machine.name.clone();
            let _ = self.done.send(Done::Claimed { machine, height });
        }
        let dir = self
            .shared
            .work
            .join(&self.machine.name)
            .join(format!("{run}-{attempt}"));
        if self.machine.backend == Backend::Pods {
            return pods::ran(self, &workload, dir, stop).await;
        }
        let name = format!(
            "{}p{}-{run}-{attempt}",
            self.shared.containers, self.shared.process
        );
        let started = backend::start(
            self.machine.backend,
            &dir,
            &name,
            &workload,
            talk,
            &self.shared.forge,
            &self.shared.fence,
        )
        .await;
        let (child, kill) = match started {
            Ok(started) => started,
            Err(error) => {
                let kept = format!("{error}; its folder stays in {}", dir.display());
                return self.report("errored", kept, None).await;
            }
        };
        let started = Done::Started {
            run,
            attempt,
            kill: kill.clone(),
        };
        let _ = self.done.send(started);
        let (end, ending) = backend::wait(child, kill, stop).await;
        match ending {
            Ending::Dropped => Ok(()),
            Ending::Running | Ending::Stopped => {
                let backend = self.machine.backend;
                let (listed, inputs) = (dir.clone(), workload.inputs.clone());
                let (packs, unpacked) =
                    backend::blocking(move || Ok(backend::packs(backend, &listed, &inputs)))
                        .await
                        .unwrap_or_else(|error| (Vec::new(), vec![error]));
                let out = workload
                    .out
                    .unwrap_or_else(|| format!("/shared/fleet/{run}/"));
                let end = decide::state_of(&end, ending);
                self.told(&dir, &out, end, packs, unpacked).await
            }
        }
    }

    async fn told(
        &self,
        dir: &std::path::Path,
        out: &str,
        (state, summary): (&'static str, String),
        packs: Vec<out::Packed>,
        mut unpacked: Vec<String>,
    ) -> Result<(), Failure> {
        let message = format!(
            "fleet run {} attempt {}: {}",
            self.run,
            self.attempt,
            state.to_lowercase()
        );
        match out::written(&self.shared.talk, dir, out, message, packs).await {
            Ok((written, refused)) => {
                unpacked.extend(refused);
                let summary = unpacked_summary(summary, &unpacked);
                self.report(state, summary, Some(written))
                    .await
                    .map_err(|(reason, error)| {
                        (
                            reason,
                            format!("{error}; its folder stays in {}", dir.display()),
                        )
                    })?;
                let gone = dir.to_path_buf();
                let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(gone)).await;
                Ok(())
            }
            Err(error) => {
                let kept = format!(
                    "{summary}; outputs not written, kept in {} on machine {}: {error}",
                    dir.display(),
                    self.machine.name
                );
                self.report(state, kept, None).await
            }
        }
    }

    async fn report(
        &self,
        state: &str,
        summary: String,
        out: Option<Value>,
    ) -> Result<(), Failure> {
        let report = json!({ "report": {
            "run": self.run,
            "attempt": self.attempt,
            "state": state,
            "summary": summary,
            "out": out,
        }});
        self.shared
            .talk
            .fleet(report, Landing::Land)
            .await
            .map(|_| ())
            .map_err(|error| ("report_refused", error))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;

    use crate::{Cli, MachineVerb, Verb};

    #[test]
    fn machine_init_and_run_parse_and_run_reads_node_toml_unless_given_a_file() {
        let init = Cli::try_parse_from(["ducktape", "machine", "init"]).unwrap();
        assert!(matches!(init.verb, Verb::Machine(MachineVerb::Init)));
        let run = Cli::try_parse_from(["ducktape", "machine", "run"]).unwrap();
        let Verb::Machine(MachineVerb::Run { config, child }) = run.verb else {
            panic!("not a machine run");
        };
        assert_eq!((config, child), (None, false));
        let args = [
            "ducktape", "machine", "run", "--config", "m.toml", "--child",
        ];
        let given = Cli::try_parse_from(args).unwrap();
        let Verb::Machine(MachineVerb::Run { config, child }) = given.verb else {
            panic!("not a machine run");
        };
        assert_eq!((config, child), (Some("m.toml".into()), true));
    }

    #[test]
    fn a_machine_consent_names_its_account_and_never_expires_unless_told() {
        let args = ["ducktape", "machine", "consent", "--account", "64"];
        let parsed = Cli::try_parse_from(args).unwrap();
        let Verb::Machine(MachineVerb::Consent {
            account,
            network,
            expires,
        }) = parsed.verb
        else {
            panic!("not a machine consent");
        };
        assert_eq!((account, network, expires), (64, None, None));
        let unnamed = Cli::try_parse_from(["ducktape", "machine", "consent"]);
        assert!(unnamed.is_err(), "a consent names the account it joins");
    }

    #[test]
    fn a_machine_reads_its_table_from_the_file_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = noded::Workspace::at(dir.path());
        let file = dir.path().join("m.toml");
        let text =
            "[machine]\n[[machine.machines]]\nname = \"desk\"\nbackend = \"podman\"\nslots = 2\n";
        std::fs::write(&file, text).unwrap();
        let read = super::settings(&workspace, Some(file)).unwrap();
        assert_eq!(read.machines[0].name, "desk");
        assert_eq!(read.machines[0].backend, noded::Backend::Podman);
        let empty = super::settings(&workspace, None).unwrap_err();
        assert!(empty.contains("no [machine]"), "{empty}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_work_root_sits_beside_the_workspace_and_never_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".ducktape");
        let workspace = noded::Workspace::at(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut settings = super::settings_of("[machine]\n");
        let work = super::work_root(&workspace, &settings).unwrap();
        let beside = std::fs::canonicalize(dir.path().join(".ducktape-machine")).unwrap();
        assert_eq!(work, beside);
        let around = dir.path().join("x");
        std::fs::create_dir_all(&around).unwrap();
        for overlapping in [
            home.join("runs"),
            around.join("..").join(".ducktape").join("runs"),
        ] {
            settings.work = Some(overlapping);
            let refused = super::work_root(&workspace, &settings).unwrap_err();
            assert!(refused.contains("overlap"), "{refused}");
        }
    }

    #[test]
    fn a_podman_machine_with_memory_runs_only_under_the_nodes_cgroup() {
        let refused = super::fenced(Some(1 << 30), None, None).unwrap_err();
        assert!(refused.contains("cgroup"), "{refused}");
        let open = super::fenced(None, None, None).unwrap();
        assert_eq!(open, super::backend::Fence::default());
        let cgroup = Some("/n/machine".to_owned());
        let fenced = super::fenced(Some(1 << 30), cgroup, Some(vec![6, 7])).unwrap();
        assert_eq!(fenced.cgroup.as_deref(), Some("/n/machine"));
        assert_eq!(fenced.cpus.as_deref(), Some("6,7"));
    }

    #[test]
    fn a_shell_machine_serves_its_manager_unless_it_lists_whom_and_podman_serves_anyone() {
        let machine = |backend, serves| noded::MachineEntry {
            name: "m".into(),
            labels: Vec::new(),
            backend,
            slots: 1,
            serves,
            service: None,
        };
        let shell = super::serves(&machine(noded::Backend::Shell, None), 15);
        assert_eq!(shell, json!({ "listed": [15] }));
        let podman = super::serves(&machine(noded::Backend::Podman, None), 15);
        assert_eq!(podman, json!("anyone"));
        let listed = super::serves(&machine(noded::Backend::Shell, Some(vec![17, 39])), 15);
        assert_eq!(listed, json!({ "listed": [17, 39] }));
        let pods = super::serves(&machine(noded::Backend::Pods, None), 15);
        assert_eq!(pods, json!("anyone"));
    }

    #[test]
    fn a_pods_machine_names_its_service_and_no_other_machine_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = noded::Workspace::at(dir.path());
        let file = dir.path().join("m.toml");
        let cases = [
            ("pods", "service = \"pods\"\n", None),
            ("pods", "", Some("names no service")),
            ("shell", "service = \"pods\"\n", Some("only a pods machine")),
            ("podman", "", None),
        ];
        for (backend, service, refused) in cases {
            let text = format!(
                "[machine]\n[[machine.machines]]\nname = \"m\"\nbackend = \"{backend}\"\nslots = 1\n{service}"
            );
            std::fs::write(&file, text).unwrap();
            let read = super::settings(&workspace, Some(file.clone()));
            match refused {
                None => assert!(read.is_ok(), "{backend}: {read:?}"),
                Some(words) => {
                    let why = read.unwrap_err();
                    assert!(why.contains(words), "{backend}: {why}");
                }
            }
        }
    }
}
