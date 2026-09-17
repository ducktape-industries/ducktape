//! A user-started deployed view driven by its events while its tab is hidden.
//! Its ordinary View ABI owns the session; replacement retires its resources.

use std::sync::{Arc, Mutex};

use futures::{StreamExt as _, stream::BoxStream};
use tokio::sync::{mpsc, watch};

use super::{Guest, Mounted, Slot, connection, intern, kernel, mounted, runtime, spawn_load, wire};

pub(crate) struct Session {
    revision: u64,
    pub input: watch::Sender<Vec<u8>>,
    pub events: BoxStream<'static, kernel::Answer>,
}

impl Session {
    /// A completed response still belongs to the network that started it.
    pub(crate) fn is_current(&self) -> bool {
        let current = connection().lock().expect("views rpc");
        current.rev == self.revision && current.client.is_some()
    }
}

pub(super) struct Attachment {
    id: u64,
    output: Option<mpsc::UnboundedSender<kernel::Answer>>,
    pub media: kernel::media::Devices,
}

impl Attachment {
    pub(super) fn active(&self) -> bool {
        self.output.is_some()
    }
}

pub(super) fn emit(guest: &mut Guest, id: u64, payload: Vec<u8>) {
    let result = match guest
        .session
        .as_ref()
        .and_then(|session| session.output.as_ref())
    {
        None => Err(wire::Refusal::new(
            "needs_session",
            "host.emit requires an active user-started session",
        )),
        Some(output) => output
            .send(Ok(payload))
            .map(|()| Vec::new())
            .map_err(|_| wire::Refusal::new("session_closed", "session output is closed")),
    };
    guest.reply(id, result);
}

pub(super) fn finish(guest: &mut Guest, id: u64, payload: &[u8]) {
    if !payload.is_empty() {
        guest.refuse(id, "malformed_request", "host.finish takes no payload");
        return;
    }
    let Some(session) = guest.session.as_mut() else {
        guest.refuse(
            id,
            "needs_session",
            "host.finish requires a running session",
        );
        return;
    };
    session.output.take();
    guest.reply(id, Ok(Vec::new()));
}

struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The shell calls this only for an explicit session action. Deployed views
/// reach the same entry through the native user-activation-gated session door.
pub(crate) fn start(module: &str, props: Vec<u8>, origin: &str) -> Result<Session, String> {
    let expected = ducktape_rpc::Client::new(origin).map_err(|error| error.to_string())?;
    let revision = {
        let connection = connection().lock().expect("views rpc");
        let matches_origin = connection
            .client
            .as_ref()
            .is_some_and(|client| client.origin() == expected.origin());
        if !matches_origin {
            return Err("session belongs to another connection".into());
        }
        connection.rev
    };
    // The app's own callers read a sentence, not a token: they show it or log
    // it. A guest gets the refusal whole, through [`start_at`].
    start_at(module, props, revision).map_err(|refusal| refusal.sentence)
}

/// Collect one background response; dropping the caller retires its session.
pub(crate) async fn request(module: &str, props: Vec<u8>, origin: &str) -> Result<Vec<u8>, String> {
    let mut session = start(module, props, origin)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = session.events.next().await {
        let chunk = chunk.map_err(|refusal| refusal.sentence)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) fn start_at(
    module: &str,
    props: Vec<u8>,
    revision: u64,
) -> Result<Session, wire::Refusal> {
    let valid_name = !module.is_empty()
        && module.len() <= 64
        && module
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if !valid_name {
        return Err(wire::Refusal::new(
            "malformed_request",
            "invalid session view or properties",
        ));
    }
    let connection = connection().lock().expect("views rpc").clone();
    if connection.client.is_none() {
        return Err(wire::Refusal::new("not_connected", "not connected"));
    }
    if connection.rev != revision {
        return Err(wire::Refusal::new(
            "stale_connection",
            "session belongs to a previous connection",
        ));
    }
    let module = module.to_owned();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (output, receiver) = mpsc::unbounded_channel();
    let (input, properties) = watch::channel(props);
    // Resolve after returning to the parent: it may hold the same view's
    // mounted lock while invoking session.start from its native button.
    let task = Task(runtime().spawn(async move {
        let (source, changes) = match session_source(&module, revision) {
            Ok(source) => source,
            Err(error) => {
                let _ = output.send(Err(error));
                return;
            }
        };
        run(source, id, revision, changes, properties, output).await;
    }));
    let events = futures::stream::unfold((receiver, task), |(mut receiver, task)| async move {
        receiver.recv().await.map(|event| (event, (receiver, task)))
    })
    .boxed();
    Ok(Session {
        revision,
        input,
        events,
    })
}

/// The network may change after start is queued: check it again before
/// touching the mounted registry, and ask the load of the very snapshot
/// that was checked. The connection lock is a leaf — it is let go before
/// the registry's and the seat's, because the window thread holds a seat
/// while its guest asks for the connection — so a move that lands in
/// between leaves a load that dies at install instead of a wedged app.
fn session_source(
    module: &str,
    revision: u64,
) -> Result<(Arc<Mutex<Mounted>>, watch::Receiver<()>), wire::Refusal> {
    let connection = connection().lock().expect("views rpc").clone();
    if connection.rev != revision || connection.client.is_none() {
        return Err(wire::Refusal::new(
            "stale_connection",
            "session belongs to a previous connection",
        ));
    }
    let module = intern(module);
    let source = mounted(module);
    let changes = {
        let mut state = source.lock().expect("session source");
        let changes = state.changes.subscribe();
        let needs_load = !state.in_flight && !matches!(state.slot, Slot::Ready(_));
        if needs_load {
            let generation = state.start(None);
            drop(spawn_load(module, &source, generation, connection));
        }
        changes
    };
    Ok((source, changes))
}

/// Immutable source metadata crosses the mounted lock; instantiation does
/// not. A cranelift instantiate is a second or more, and the window thread
/// draws under that same lock.
struct SessionCode {
    component: wasmtime::component::Component,
    module: &'static str,
    name: String,
    assets: Arc<crate::backend::view_source::Assets>,
    hash: Option<[u8; 32]>,
    revision: u64,
    identity: Arc<()>,
}

impl SessionCode {
    fn capture(guest: &Guest) -> Self {
        Self {
            component: guest.component.clone(),
            module: guest.module,
            name: guest.name.clone(),
            assets: guest.assets.clone(),
            hash: guest.hash,
            revision: guest.connection_rev,
            identity: guest.alive.clone(),
        }
    }

    fn instantiate(self) -> Result<(Guest, Arc<()>), String> {
        let mut guest = Guest::instantiate(self.module, &self.component, self.module)?;
        guest.name = self.name;
        guest.assets = self.assets;
        guest.hash = self.hash;
        guest.connection_rev = self.revision;
        guest.init(self.module)?;
        Ok((guest, self.identity))
    }
}

async fn run(
    source: Arc<Mutex<Mounted>>,
    id: u64,
    revision: u64,
    mut changes: watch::Receiver<()>,
    mut properties: watch::Receiver<Vec<u8>>,
    output: mpsc::UnboundedSender<kernel::Answer>,
) {
    let mut live = kernel::isolated_live_events();
    let code = loop {
        let same_network = connection().lock().expect("views rpc").rev == revision;
        if !same_network {
            return;
        }
        let loaded = {
            let state = source.lock().expect("session source");
            match &state.slot {
                Slot::Loading => Ok(None),
                Slot::Empty => Err(wire::Refusal::new(
                    "session_view_removed",
                    "session view was removed",
                )),
                Slot::Failed(error) => Err(wire::Refusal::new("session_load_failed", error.clone())),
                Slot::Ready(guest) => {
                    let current = guest.connection_rev == revision;
                    if current {
                        Ok(Some(SessionCode::capture(guest)))
                    } else if state.in_flight {
                        // Reconnection keeps the visible previous instance
                        // until its replacement loads. A new session must wait
                        // for the instance belonging to its own connection.
                        Ok(None)
                    } else {
                        Err(wire::Refusal::new(
                            "stale_connection",
                            "session view belongs to a previous connection",
                        ))
                    }
                }
            }
        };
        match loaded {
            Ok(Some(instance)) => break instance,
            Ok(None) => tokio::select! {
                _ = output.closed() => return,
                result = changes.changed() => if result.is_err() { return; },
            },
            Err(error) => {
                let _ = output.send(Err(error));
                return;
            }
        }
    };
    let (mut guest, source_identity) = match code.instantiate() {
        Ok(instance) => instance,
        Err(error) => {
            let _ = output.send(Err(wire::Refusal::new("session_load_failed", error)));
            return;
        }
    };
    if guest.connection_rev != revision {
        return;
    }
    loop {
        let same_network = connection().lock().expect("views rpc").rev == revision;
        let same_deployment = {
            let state = source.lock().expect("session source");
            matches!(&state.slot, Slot::Ready(current) if Arc::ptr_eq(&current.alive, &source_identity))
        };
        if !same_network || !same_deployment {
            break;
        }
        let (mut replies, deadline, again) =
            match turn(&mut guest, id, &output, &properties.borrow_and_update()) {
                Ok(step) => step,
                Err(error) => {
                    let _ = output.send(Err(error));
                    break;
                }
            };
        let finished = guest
            .session
            .as_ref()
            .is_some_and(|session| !session.active());
        if finished {
            break;
        }
        if again {
            tokio::task::yield_now().await;
            if output.is_closed() {
                break;
            }
            continue;
        }
        let clock = async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = output.closed() => break,
            result = properties.changed() => if result.is_err() { break; },
            result = changes.changed() => if result.is_err() { break; },
            result = replies.changed() => if result.is_err() { break; },
            event = live.recv() => match event {
                Ok(plane) => kernel::invalidate_live(&mut guest, Some(&plane)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => kernel::invalidate_live(&mut guest, None),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = clock => {},
        }
    }
}

fn turn(
    guest: &mut Guest,
    id: u64,
    output: &mpsc::UnboundedSender<kernel::Answer>,
    props: &[u8],
) -> Result<(watch::Receiver<()>, Option<std::time::Instant>, bool), wire::Refusal> {
    if guest.session.is_none() {
        guest.session = Some(Attachment {
            id,
            output: Some(output.clone()),
            media: kernel::media::Devices::new(),
        });
    }
    if guest
        .session
        .as_ref()
        .is_none_or(|session| session.id != id)
    {
        return Err(wire::Refusal::new(
            "wrong_session",
            "view belongs to another session",
        ));
    }
    // Subscribe before draining: a completion during redraw must still wake
    // this runner after it releases the guest lock.
    let changes = guest.replies.changes();
    let again = guest.redraw(&Some(props.to_vec()));
    if let Some(error) = &guest.fault {
        return Err(wire::Refusal::new("view_fault", error.clone()));
    }
    Ok((changes, kernel::next_tick(&guest.clocks), again))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_guest() -> Guest {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/views/call_view.wasm");
        Guest::load_from("call", &path)
            .expect("stage the call guest with ops/build-views.sh -p call-view")
    }

    #[tokio::test]
    async fn a_previous_network_cannot_start_a_session_on_the_new_connection() {
        let _turn = super::super::tests::connection_turn().await;
        let revision = {
            let mut connection = connection().lock().unwrap();
            connection.client = Some(ducktape_rpc::Client::new("http://127.0.0.1:3001").unwrap());
            connection.rev += 1;
            connection.rev - 1
        };
        let error = start_at("arbitrary-companion", b"{}".to_vec(), revision)
            .err()
            .expect("stale source refused");
        assert_eq!(error.reason, "stale_connection", "{}", error.sentence);
        let source_error = session_source("queued-before-network-switch", revision)
            .err()
            .expect("queued stale session refused before mounting");
        assert_eq!(
            source_error.reason, "stale_connection",
            "{}", source_error.sentence
        );
        assert!(
            !super::super::registry()
                .lock()
                .unwrap()
                .contains_key("queued-before-network-switch")
        );
    }

    #[tokio::test]
    async fn isolated_instance_inherits_code_without_parent_activation_or_subscriptions() {
        let _turn = super::super::tests::connection_turn().await;
        let mut source = call_guest();
        source.user_activation = Some(());
        source.live_subscriptions.push((51, "chat".into()));
        let (control, _receiver) = watch::channel(Vec::new());
        source.sessions.insert(52, control);
        let (mut isolated, identity) = SessionCode::capture(&source).instantiate().unwrap();
        assert!(Arc::ptr_eq(&identity, &source.alive));
        assert_eq!(isolated.hash, source.hash);
        assert_eq!(isolated.connection_rev, source.connection_rev);
        assert!(isolated.user_activation.is_none());
        assert!(isolated.sessions.is_empty());
        assert!(isolated.live_subscriptions.is_empty());
        isolated.live_subscriptions.push((61, "chat".into()));
        isolated.live_subscriptions.push((62, "block".into()));
        kernel::invalidate_live(&mut isolated, Some("chat"));
        assert!(matches!(
            isolated.pending.last(),
            Some(super::super::wire::Event::Response {
                id: 61,
                done: false,
                ..
            })
        ));
        let before = isolated.pending.len();
        kernel::invalidate_live(&mut isolated, None);
        assert_eq!(
            isolated.pending.len(),
            before + 2,
            "a lagged feed invalidates all isolated subscriptions"
        );
        assert_eq!(source.live_subscriptions, vec![(51, "chat".into())]);
        assert!(source.user_activation.is_some());
    }

    #[tokio::test]
    async fn session_output_drains_before_terminal() {
        let _turn = super::super::tests::connection_turn().await;
        let mut guest = call_guest();
        let (output, mut receiver) = mpsc::unbounded_channel();
        guest.session = Some(Attachment {
            id: 1,
            output: Some(output),
            media: kernel::media::Devices::new(),
        });
        let large = vec![0; 128 * 1024];
        emit(&mut guest, 1, large.clone());
        for id in 0..64 {
            emit(&mut guest, id as u64 + 2, vec![id as u8]);
        }
        finish(&mut guest, 101, &[]);
        assert!(!guest.session.as_ref().unwrap().active());
        emit(&mut guest, 102, b"after finish".to_vec());
        assert_eq!(receiver.recv().await.unwrap().unwrap(), large);
        for id in 0..64 {
            assert_eq!(receiver.recv().await.unwrap().unwrap(), vec![id as u8]);
        }
        assert!(receiver.recv().await.is_none());
    }

    #[tokio::test]
    async fn deployed_guest_finishes_without_changing_its_visible_source() {
        let _turn = super::super::tests::connection_turn().await;
        let guest = call_guest();
        let revision = guest.connection_rev;
        let mounted = Mounted::seat();
        let changes = {
            let mut state = mounted.lock().unwrap();
            state.slot = Slot::Ready(Box::new(guest));
            state.changes.subscribe()
        };
        let (_input, properties) = watch::channel(b"[]".to_vec());
        let (output, mut events) = mpsc::unbounded_channel();
        let runner = tokio::spawn(run(
            mounted.clone(),
            1,
            revision,
            changes,
            properties,
            output,
        ));
        let mut outputs = Vec::new();
        while let Some(event) = events.recv().await {
            let value: serde_json::Value = serde_json::from_slice(&event.unwrap()).unwrap();
            outputs.push(value);
        }
        runner.await.unwrap();
        assert!(
            outputs.iter().any(|value| value["kind"] == "error"),
            "guest emits its refusal before host.finish closes the stream"
        );
        assert!(
            matches!(&mounted.lock().unwrap().slot, Slot::Ready(guest) if guest.session.is_none()),
            "isolated session never changes its visible source"
        );
    }

    #[tokio::test]
    async fn replacement_ends_hidden_session_without_erasing_the_successor() {
        use tokio::io::AsyncReadExt as _;
        let _turn = super::super::tests::connection_turn().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            ducktape_rpc::Client::new(&format!("http://{}", listener.local_addr().unwrap()))
                .unwrap();
        connection().lock().unwrap().client = Some(client);
        let (requested, request) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            requested.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let guest = call_guest();
        let revision = guest.connection_rev;
        let mounted = Mounted::seat();
        let changes = {
            let mut state = mounted.lock().unwrap();
            state.slot = Slot::Ready(Box::new(guest));
            state.changes.subscribe()
        };
        let (_input, properties) =
            watch::channel(br#"{"channel":"room","muted":false,"source":"off"}"#.to_vec());
        let (output, mut events) = mpsc::unbounded_channel();
        let runner = tokio::spawn(run(
            mounted.clone(),
            1,
            revision,
            changes,
            properties,
            output,
        ));
        request.await.unwrap();
        {
            let mut state = mounted.lock().unwrap();
            assert!(matches!(&state.slot, Slot::Ready(guest) if guest.session.is_none()));
            state.slot = Slot::Ready(Box::new(call_guest()));
            state.generation += 1;
            state.changes.send_replace(());
        }
        while events.recv().await.is_some() {}
        runner.await.unwrap();
        assert!(
            matches!(&mounted.lock().unwrap().slot, Slot::Ready(guest) if guest.session.is_none())
        );
        server.abort();
    }
}
