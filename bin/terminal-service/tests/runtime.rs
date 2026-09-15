use agent_service::wire;
use ducktape_terminal::{
    runtime::Runtime,
    state::{Caller, Mode},
};
use std::collections::BTreeMap;

#[tokio::test]
async fn create_refusal_is_answered_and_stopping_the_runtime_finishes_its_task() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        provider_host::ProviderSet::empty(),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    let error = runtime
        .create(
            caller.clone(),
            Mode::Single,
            wire::Create {
                session: session.clone(),
                provider: "absent".into(),
                restricted: false,
                limits: BTreeMap::new(),
                credential: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("unknown_provider"), "{error}");
    assert!(runtime.replay(session, caller, 0).await.unwrap().ended);
    drop(runtime);
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct Provider {
    starts: AtomicUsize,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl provider_host::Provider for Provider {
    fn capability(&self) -> &str {
        "stub"
    }
    async fn run(&self, _: &str, _: &provider_host::RunContext) -> Result<String, String> {
        Err("interactive only".into())
    }
    async fn spawn_interactive(
        &self,
        _: &provider_host::RunContext,
        _: bool,
    ) -> Result<provider_host::InteractiveSession, String> {
        let second = self.starts.fetch_add(1, Ordering::SeqCst) == 1;
        if second {
            self.entered.notify_one();
            self.release.notified().await;
        }
        provider_host::InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
    }
}

fn providers(entered: Arc<Notify>, release: Arc<Notify>) -> provider_host::ProviderSet {
    let spec = provider_host::CapabilitySpec::parse(
        r#"
        spec = 1
        [capability]
        tag = "stub"
        description = "runtime test"
        [detect]
        bin = "cat"
        [invoke]
        args = []
        prompt = "stdin"
        [output]
        format = "text"
    "#,
        "test",
    )
    .unwrap();
    provider_host::ProviderSet::assemble(
        provider_host::SpecSet::from_specs(vec![spec]),
        vec![Box::new(Provider {
            starts: AtomicUsize::new(0),
            entered,
            release,
        })],
    )
}

fn create(session: &str) -> wire::Create {
    wire::Create {
        session: session.into(),
        provider: "stub".into(),
        restricted: false,
        limits: BTreeMap::new(),
        credential: None,
    }
}

#[tokio::test]
async fn cancelled_slow_create_does_not_block_existing_input_or_leave_a_pty() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, task) = Runtime::start(
        providers(entered.clone(), release.clone()),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller {
        account: 7,
        node: [1; 32],
    };
    let first = "0000000000000001";
    let second = "0000000000000002";
    runtime
        .create(caller.clone(), Mode::Single, create(first))
        .await
        .unwrap();
    let pending = {
        let runtime = runtime.clone();
        let caller = caller.clone();
        tokio::spawn(async move { runtime.create(caller, Mode::Single, create(second)).await })
    };
    entered.notified().await;
    let mut output_changes = runtime.changes();
    runtime
        .resize(first.into(), caller.clone(), 100, 30)
        .await
        .unwrap();
    runtime
        .input(first.into(), caller.clone(), b"still running\n".to_vec())
        .await
        .unwrap();
    loop {
        let replay = runtime
            .replay(first.into(), caller.clone(), 0)
            .await
            .unwrap();
        let output: Vec<u8> = replay
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        let echoed = output
            .windows(b"still running".len())
            .any(|part| part == b"still running");
        if echoed {
            assert!(
                runtime
                    .replay(first.into(), caller.clone(), replay.head)
                    .await
                    .unwrap()
                    .chunks
                    .iter()
                    .all(|chunk| chunk.seq > replay.head)
            );
            break;
        }
        output_changes.changed().await.unwrap();
    }
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let mut changes = runtime.changes();
    release.notify_one();
    loop {
        if runtime
            .replay(second.into(), caller.clone(), 0)
            .await
            .unwrap()
            .ended
        {
            break;
        }
        changes.changed().await.unwrap();
    }
    drop(runtime);
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn service_stop_cancels_an_unfinished_spawn_and_closes_running_ptys() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let (runtime, task) = Runtime::start(
        providers(entered.clone(), Arc::new(Notify::new())),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller {
        account: 7,
        node: [1; 32],
    };
    runtime
        .create(caller.clone(), Mode::Single, create("0000000000000001"))
        .await
        .unwrap();
    let pending = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .create(caller, Mode::Single, create("0000000000000002"))
                .await
        })
    };
    entered.notified().await;
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

fn committed_message(seq: u64, account: u64, text: &str) -> chat::MessageView {
    chat::MessageView {
        channel_id: "term-0000000000000001".into(),
        seq,
        head: chat::MessageHead {
            message_id: format!("m{seq}"),
            origin: sdk::Origin::Program(account),
            content_origin: sdk::Origin::Program(account),
            author: chat::Party::Account(account),
            revision: 1,
            blocks: ducktape_terminal::consensus::command_blocks(text),
            created_at: 0,
            rev: 0,
            edited_at: None,
            base_rev: None,
            deleted: false,
            thread: None,
            reply_count: 0,
            last_reply_seq: None,
        },
    }
}

#[tokio::test]
async fn shared_commands_are_ordered_deduplicated_and_do_not_accept_raw_input() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        providers(Arc::new(Notify::new()), Arc::new(Notify::new())),
        "test".into(),
        directory.path().into(),
    );
    let owner = Caller {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    let mut spec = create(&session);
    spec.restricted = true;
    runtime
        .create(owner.clone(), Mode::Shared, spec)
        .await
        .unwrap();
    assert!(
        runtime
            .input(session.clone(), owner.clone(), b"raw\n".to_vec())
            .await
            .is_err()
    );
    let messages = vec![
        committed_message(1, 7, "first"),
        committed_message(2, 8, "stranger"),
        committed_message(3, 7, "second"),
    ];
    let mut changes = runtime.changes();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            messages.clone(),
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            messages,
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            (4..=70)
                .map(|seq| committed_message(seq, 7, "bulk"))
                .collect(),
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            vec![committed_message(71, 7, "marker")],
        )
        .await
        .unwrap();
    loop {
        let replay = runtime
            .replay(session.clone(), owner.clone(), 0)
            .await
            .unwrap();
        let output: Vec<u8> = replay
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        let output = String::from_utf8_lossy(&output);
        if output.contains("marker") {
            assert!(!output.contains("stranger"));
            assert!(!output.contains("raw"));
            assert!(output.find("first").unwrap() < output.find("second").unwrap());
            // A PTY echoes input and cat writes it once more: at most two copies
            // per accepted command, even though its committed page was retried.
            assert!(output.matches("first").count() <= 2, "{output}");
            assert!(output.matches("second").count() <= 2, "{output}");
            break;
        }
        changes.changed().await.unwrap();
    }
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
}
