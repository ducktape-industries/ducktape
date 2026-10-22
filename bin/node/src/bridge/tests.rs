use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::stream::LocalBoxStream;
use serde_json::{Value, json};

use super::adapter::{Adapter, Event, Turn};
use super::chain::{Chain, Sent, seq_of, to_prefix};
use super::claude::{self, Claude};
use super::kept::{self, Where};
use super::relay::armed;
use super::run::Bridge;

const ME: u64 = 60;
const ROOM: &str = "room";

#[derive(Debug, PartialEq)]
enum Seen {
    Op(Value),
    Started(Where, Option<String>),
    Said(String),
    Finished,
    Stopped,
}

struct Net {
    seen: UnboundedSender<Seen>,
    rows: RefCell<BTreeMap<String, Value>>,
    changes: RefCell<BTreeMap<u64, Value>>,
    drafts: RefCell<BTreeSet<String>>,
    deaf: RefCell<BTreeSet<u64>>,
    follow: RefCell<Option<UnboundedReceiver<u64>>>,
}

impl Net {
    fn new(seen: UnboundedSender<Seen>) -> Net {
        Net {
            seen,
            rows: RefCell::default(),
            changes: RefCell::default(),
            drafts: RefCell::default(),
            deaf: RefCell::default(),
            follow: RefCell::default(),
        }
    }

    fn listen(&self) -> UnboundedSender<u64> {
        let (heard, follow) = mpsc::unbounded();
        *self.follow.borrow_mut() = Some(follow);
        heard
    }

    fn message(
        &self,
        change: u64,
        id: &str,
        seq: u64,
        thread: Option<u64>,
        author: u64,
        text: &str,
    ) {
        let row = json!({
            "message_id": id,
            "channel_id": ROOM,
            "seq": seq,
            "thread": thread,
            "author": { "account": author },
            "deleted": false,
            "text": text,
        });
        self.rows.borrow_mut().insert(id.to_owned(), row);
        self.attribute(change, id);
    }

    fn attribute(&self, change: u64, id: &str) {
        let source = json!({ "program": "chat", "object": { "kind": "message", "id": id } });
        self.changes.borrow_mut().insert(
            change,
            json!({ "seq": change, "kind": "added", "detail": "", "source": source }),
        );
    }

    fn woke(&self, change: u64, wake: u64) {
        let detail = hex::encode(format!("Wake {wake} is over."));
        let source =
            json!({ "program": "wake", "object": { "kind": "wake", "id": wake.to_string() } });
        self.changes.borrow_mut().insert(
            change,
            json!({ "seq": change, "kind": "added", "detail": detail, "source": source }),
        );
    }
}

impl Chain for Net {
    fn account(&self) -> u64 {
        ME
    }

    fn identity(&self) -> &str {
        "identity"
    }

    async fn query(&self, program: &str, query: Value) -> Result<Value, String> {
        Ok(match (program, &query) {
            ("attribution", query) if query.get("change").is_some() => {
                let seq = query["change"]["seq"].as_u64().unwrap();
                let change = self.changes.borrow().get(&seq).cloned();
                json!({ "change": change })
            }
            ("attribution", query) if query.get("changes_to").is_some() => {
                assert_eq!(query["changes_to"]["recipient"], json!(ME));
                let items: Vec<Value> = self.changes.borrow().values().cloned().collect();
                json!({ "changes": { "height": 0, "items": items, "next": null } })
            }
            ("chat", query) if query.get("message_by_id").is_some() => {
                let id = query["message_by_id"]["message_id"].as_str().unwrap();
                json!({ "message": self.rows.borrow().get(id).cloned() })
            }
            ("identity", query) if query.get("hears").is_some() => {
                let from = query["hears"]["from"].as_u64().unwrap();
                json!({ "hears": !self.deaf.borrow().contains(&from) })
            }
            _ => panic!("{program} was asked {query}"),
        })
    }

    async fn send(&self, program: &str, op: Value) -> Result<Sent, String> {
        self.seen.unbounded_send(Seen::Op(op.clone())).unwrap();
        let (name, body) = op.as_object().unwrap().iter().next().unwrap();
        let applied = Sent::Applied(Value::Null);
        Ok(match (program, name.as_str()) {
            ("chat", "drafting") => {
                let channel = body["channel_id"].as_str().unwrap().to_owned();
                self.drafts.borrow_mut().insert(channel);
                applied
            }
            ("chat", "undraft") => {
                self.drafts
                    .borrow_mut()
                    .remove(body["channel_id"].as_str().unwrap());
                applied
            }
            ("chat", "post") => {
                let id = body["message_id"].as_str().unwrap().to_owned();
                if self.rows.borrow().contains_key(&id) {
                    return Ok(Sent::Rejected {
                        reason: "taken".into(),
                        sentence: "a message has that id".into(),
                    });
                }
                let channel = body["channel_id"].as_str().unwrap().to_owned();
                self.drafts.borrow_mut().remove(&channel);
                let row =
                    json!({ "message_id": id, "channel_id": channel, "author": { "account": ME } });
                self.rows.borrow_mut().insert(id, row);
                applied
            }
            _ => panic!("{program} was sent {op}"),
        })
    }

    async fn follow(&self) -> Result<LocalBoxStream<'_, Result<u64, String>>, String> {
        let follow = self.follow.borrow_mut().take().expect("one follow a run");
        Ok(follow.map(Ok).boxed_local())
    }
}

struct Script {
    seen: UnboundedSender<Seen>,
    sessions: RefCell<Vec<UnboundedSender<Event>>>,
}

struct Played {
    seen: UnboundedSender<Seen>,
    events: UnboundedSender<Event>,
}

impl Turn for Played {
    async fn say(&mut self, text: &str) -> Result<(), String> {
        self.seen
            .unbounded_send(Seen::Said(text.to_owned()))
            .unwrap();
        Ok(())
    }

    fn finish(&mut self) {
        self.seen.unbounded_send(Seen::Finished).unwrap();
    }

    fn stop(&mut self) {
        self.seen.unbounded_send(Seen::Stopped).unwrap();
        let _ = self.events.unbounded_send(Event::Ended("stopped".into()));
    }
}

impl Adapter for Script {
    type Turn = Played;

    fn start(
        &self,
        at: &Where,
        resume: Option<&str>,
    ) -> Result<(Played, LocalBoxStream<'static, Event>), String> {
        let (events, played) = mpsc::unbounded();
        self.sessions.borrow_mut().push(events.clone());
        let started = Seen::Started(at.clone(), resume.map(str::to_owned));
        self.seen.unbounded_send(started).unwrap();
        let turn = Played {
            seen: self.seen.clone(),
            events,
        };
        Ok((turn, played.boxed_local()))
    }
}

impl Script {
    fn play(&self, session: usize, event: Event) {
        self.sessions.borrow()[session]
            .unbounded_send(event)
            .unwrap();
    }
}

struct World {
    net: Net,
    script: Script,
    seen: UnboundedReceiver<Seen>,
    armed: UnboundedSender<(u64, String)>,
    heard_armed: Option<UnboundedReceiver<(u64, String)>>,
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn world() -> World {
    let (sender, seen) = mpsc::unbounded();
    let (armed, heard_armed) = mpsc::unbounded();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.json");
    World {
        net: Net::new(sender.clone()),
        script: Script {
            seen: sender,
            sessions: RefCell::default(),
        },
        seen,
        armed,
        heard_armed: Some(heard_armed),
        _dir: dir,
        path,
    }
}

fn play<F: Future<Output = ()>>(
    net: &Net,
    script: &Script,
    path: &Path,
    armed: UnboundedReceiver<(u64, String)>,
    driver: F,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut bridge = Bridge::open(net, script, path).unwrap();
        let follow = net.follow().await.unwrap();
        tokio::select! {
            biased;
            ran = bridge.run(follow, armed, std::future::pending()) => panic!("the bridge ended: {ran:?}"),
            () = driver => {}
        }
    });
}

async fn next(seen: &mut UnboundedReceiver<Seen>) -> Seen {
    seen.next().await.expect("the bridge goes on")
}

async fn op(seen: &mut UnboundedReceiver<Seen>) -> Value {
    match next(seen).await {
        Seen::Op(op) => op,
        other => panic!("not an op: {other:?}"),
    }
}

fn at(thread: u64) -> Where {
    Where {
        channel: ROOM.into(),
        thread,
    }
}

fn post(seq: u64, thread: u64, text: &str) -> Value {
    json!({ "post": {
        "channel_id": ROOM,
        "message_id": format!("bridge-{ME}-{ROOM}-{seq}"),
        "text": text,
        "thread": thread,
    } })
}

fn draft(thread: u64, text: &str, doing: Option<&str>) -> Value {
    json!({ "drafting": {
        "channel_id": ROOM,
        "thread": thread,
        "text": text,
        "anew": false,
        "doing": doing,
    } })
}

fn said(seen: Seen, wanted: &str) {
    match seen {
        Seen::Said(text) => assert!(text.contains(wanted), "{text} says no {wanted}"),
        other => panic!("not said: {other:?}"),
    }
}

#[test]
fn a_mention_is_drafted_and_posted_and_input_mid_turn_waits_for_the_answer_then_goes_to_the_same_session()
 {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(1, "m1", 5, None, 7, "hello");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "<@7> wrote message 5 in #room:\nhello");
            script.play(0, Event::Session("s1".into()));
            script.play(0, Event::Text("thinking".into()));
            assert_eq!(op(seen).await, draft(5, "thinking", None));
            script.play(0, Event::Doing("mcp__ducktape__network_query".into()));
            assert_eq!(
                op(seen).await,
                draft(5, "", Some("mcp__ducktape__network_query"))
            );
            net.message(2, "m2", 6, Some(5), 7, "and more");
            net.message(3, "m3", 9, None, 7, "elsewhere");
            heard.unbounded_send(2).unwrap();
            heard.unbounded_send(3).unwrap();
            assert_eq!(
                next(seen).await,
                Seen::Started(at(9), None),
                "m2 waits for the turn's answer"
            );
            said(next(seen).await, "elsewhere");
            script.play(0, Event::Answer("first".into()));
            said(
                next(seen).await,
                "message 6 in thread 5 of #room:\nand more",
            );
            assert_eq!(op(seen).await, post(5, 5, "first"));
            script.play(0, Event::Answer("second".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            assert_eq!(op(seen).await, post(6, 5, "second"));
            script.play(0, Event::Ended("exit status: 0".into()));
            net.message(4, "m4", 10, Some(5), 7, "later");
            heard.unbounded_send(4).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), Some("s1".into())));
            said(next(seen).await, "later");
        },
    );
    let kept = kept::load(&world.path).unwrap().unwrap();
    assert_eq!(kept.last, 4);
    let place = &kept.places["chat/room/5"];
    assert_eq!(place.session.as_deref(), Some("s1"));
    assert!(!place.held, "no draft stands");
    let open: Vec<u64> = place.open.iter().map(|input| input.seq).collect();
    assert_eq!(open, [4]);
}

#[test]
fn an_adapter_that_folds_queued_messages_into_one_answer_still_answers_each_and_lets_go() {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(1, "m1", 5, None, 7, "one");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "one");
            net.message(2, "m2", 6, Some(5), 7, "two");
            net.message(3, "m3", 7, Some(5), 7, "three");
            net.message(4, "m4", 9, None, 7, "elsewhere");
            for seq in 2..=4 {
                heard.unbounded_send(seq).unwrap();
            }
            assert_eq!(
                next(seen).await,
                Seen::Started(at(9), None),
                "neither m2 nor m3 is said mid-turn"
            );
            said(next(seen).await, "elsewhere");
            script.play(0, Event::Answer("all at once".into()));
            said(next(seen).await, "two");
            assert_eq!(op(seen).await, post(5, 5, "all at once"));
            script.play(0, Event::Answer("two done".into()));
            said(next(seen).await, "three");
            assert_eq!(op(seen).await, post(6, 5, "two done"));
            script.play(0, Event::Answer("three done".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            assert_eq!(op(seen).await, post(7, 5, "three done"));
            script.play(0, Event::Ended("exit status: 0".into()));
        },
    );
    assert!(world.net.drafts.borrow().is_empty());
}

#[test]
fn a_session_that_dies_answers_its_turn_as_ended_and_hands_the_waiting_input_to_its_resumed_session()
 {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(1, "m1", 5, None, 7, "one");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "one");
            script.play(0, Event::Session("s1".into()));
            net.message(2, "m2", 6, Some(5), 7, "two");
            net.message(3, "m3", 7, Some(5), 7, "three");
            net.message(4, "m4", 9, None, 7, "elsewhere");
            for seq in 2..=4 {
                heard.unbounded_send(seq).unwrap();
            }
            assert_eq!(next(seen).await, Seen::Started(at(9), None));
            said(next(seen).await, "elsewhere");
            script.play(0, Event::Ended("signal: 9 (SIGKILL)".into()));
            let ended = "My session here ended (signal: 9 (SIGKILL)) before it answered.";
            assert_eq!(next(seen).await, Seen::Started(at(5), Some("s1".into())));
            said(next(seen).await, "two");
            assert_eq!(op(seen).await, post(5, 5, ended));
            script.play(2, Event::Answer("b".into()));
            said(next(seen).await, "three");
            assert_eq!(op(seen).await, post(6, 5, "b"));
            script.play(2, Event::Answer("c".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            assert_eq!(op(seen).await, post(7, 5, "c"));
        },
    );
}

#[test]
fn an_adapter_that_always_dies_after_init_answers_each_input_as_ended_once_and_lets_go() {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    let ended = "My session here ended (signal: 9 (SIGKILL)) before it answered.";
    let die = |session: usize| {
        script.play(session, Event::Session("s1".into()));
        script.play(session, Event::Ended("signal: 9 (SIGKILL)".into()));
    };
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(1, "m1", 5, None, 7, "one");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "one");
            net.message(2, "m2", 6, Some(5), 7, "two");
            net.message(3, "m3", 9, None, 7, "elsewhere");
            heard.unbounded_send(2).unwrap();
            heard.unbounded_send(3).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(9), None));
            said(next(seen).await, "elsewhere");
            die(0);
            assert_eq!(
                next(seen).await,
                Seen::Started(at(5), Some("s1".into())),
                "m2's turn"
            );
            said(next(seen).await, "two");
            assert_eq!(op(seen).await, post(5, 5, ended));
            die(2);
            assert_eq!(op(seen).await, post(6, 5, ended));
            net.message(4, "m4", 11, None, 7, "after");
            heard.unbounded_send(4).unwrap();
            assert_eq!(
                next(seen).await,
                Seen::Started(at(11), None),
                "nothing was said again in thread 5"
            );
        },
    );
    let kept = kept::load(&world.path).unwrap().unwrap();
    assert!(kept.places["chat/room/5"].open.is_empty());
}

#[test]
fn a_session_that_dies_before_it_starts_answers_that_it_ended() {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(1, "m1", 5, None, 7, "one");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "one");
            script.play(0, Event::Ended("exit status: 1".into()));
            let ended = "My session here ended (exit status: 1) before it started.";
            assert_eq!(op(seen).await, post(5, 5, ended));
        },
    );
    assert!(world.net.drafts.borrow().is_empty());
}

#[test]
fn a_mention_starts_at_once_whoever_else_answers_there_and_a_wake_the_model_armed_comes_back_to_its_place()
 {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen, armed) = (&world.net, &world.script, &mut world.seen, &world.armed);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.drafts.borrow_mut().insert(ROOM.into());
            net.message(1, "m1", 5, None, 7, "hello");
            heard.unbounded_send(1).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "hello");
            script.play(0, Event::Answer("one".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            assert_eq!(op(seen).await, post(5, 5, "one"));
            script.play(0, Event::Ended("exit status: 0".into()));
            armed.unbounded_send((7, "chat/room/5".into())).unwrap();
            net.woke(2, 7);
            heard.unbounded_send(2).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "Wake 7 is over.");
            script.play(1, Event::Answer("two".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            let wake = json!({ "post": {
                "channel_id": ROOM,
                "message_id": format!("bridge-{ME}-wake-7"),
                "text": "two",
                "thread": 5,
            } });
            assert_eq!(op(seen).await, wake);
            script.play(1, Event::Ended("exit status: 0".into()));
            net.woke(3, 8);
            heard.unbounded_send(3).unwrap();
            armed.unbounded_send((8, "chat/room/5".into())).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(5), None));
            said(next(seen).await, "Wake 8 is over.");
        },
    );
}

#[test]
fn a_bridge_killed_mid_turn_takes_its_draft_down_on_restart_and_resumes_the_open_input() {
    let mut world = world();
    let heard = world.net.listen();
    {
        let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
        play(
            net,
            script,
            &world.path,
            world.heard_armed.take().unwrap(),
            async {
                net.message(1, "m1", 5, None, 7, "hello");
                heard.unbounded_send(1).unwrap();
                assert_eq!(next(seen).await, Seen::Started(at(5), None));
                said(next(seen).await, "hello");
                script.play(0, Event::Session("s1".into()));
                script.play(0, Event::Text("working".into()));
                assert_eq!(op(seen).await, draft(5, "working", None));
            },
        );
    }
    assert_eq!(world.net.drafts.borrow().len(), 1, "the kill left a draft");
    let heard = world.net.listen();
    let (sender, mut seen) = mpsc::unbounded();
    let script = Script {
        seen: sender.clone(),
        sessions: RefCell::default(),
    };
    let net = Net {
        seen: sender,
        ..std::mem::replace(&mut world.net, Net::new(mpsc::unbounded().0))
    };
    let (_, armed) = mpsc::unbounded();
    play(&net, &script, &world.path, armed, async {
        let _keep = &heard;
        let mut three = Vec::new();
        for _ in 0..3 {
            three.push(next(&mut seen).await);
        }
        let (ops, session): (Vec<Seen>, Vec<Seen>) = three
            .into_iter()
            .partition(|seen| matches!(seen, Seen::Op(_)));
        let undraft = Seen::Op(json!({ "undraft": { "channel_id": ROOM } }));
        assert_eq!(ops, [undraft]);
        let [started, hello] = <[Seen; 2]>::try_from(session).unwrap();
        assert_eq!(started, Seen::Started(at(5), Some("s1".into())));
        said(hello, "hello");
        assert!(
            net.drafts.borrow().is_empty(),
            "the draft is down before the session drafts anything"
        );
    });
}

#[test]
fn the_bridge_answers_one_input_per_message_and_none_for_whom_it_does_not_answer() {
    let mut world = world();
    let heard = world.net.listen();
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.deaf.borrow_mut().insert(8);
            net.message(1, "m1", 5, None, 8, "from a stranger");
            net.message(2, "m2", 6, None, 7, "hello");
            net.attribute(3, "m2");
            net.message(4, "m4", 7, None, ME, "my own");
            for seq in 1..=4 {
                heard.unbounded_send(seq).unwrap();
            }
            assert_eq!(next(seen).await, Seen::Started(at(6), None));
            said(next(seen).await, "hello");
            script.play(0, Event::Answer("hi".into()));
            assert_eq!(next(seen).await, Seen::Finished);
            assert_eq!(op(seen).await, post(6, 6, "hi"));
        },
    );
    assert_eq!(kept::load(&world.path).unwrap().unwrap().last, 4);
}

#[test]
fn a_fresh_bridge_starts_after_the_accounts_history() {
    let mut world = world();
    let heard = world.net.listen();
    world.net.message(1, "m1", 5, None, 7, "old");
    let (net, script, seen) = (&world.net, &world.script, &mut world.seen);
    play(
        net,
        script,
        &world.path,
        world.heard_armed.take().unwrap(),
        async {
            net.message(2, "m2", 6, None, 7, "new");
            heard.unbounded_send(2).unwrap();
            assert_eq!(next(seen).await, Seen::Started(at(6), None));
        },
    );
}

#[test]
fn claude_runs_with_the_bridges_environment_less_the_password_and_its_own_config_folder() {
    let claude = |env: Vec<(&str, &str)>| Claude {
        program: "claude".into(),
        exe: "/bin/ducktape".into(),
        socket: "/s/mcp.sock".into(),
        config: "/s/bridge.json.claude".into(),
        args: vec!["--model".into(), "opus".into()],
        env: env
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
    };
    let given = vec![
        ("PATH", "/usr/bin"),
        ("ANTHROPIC_BASE_URL", "http://gate:8080"),
        ("ANTHROPIC_API_KEY", "gate-token"),
        ("DUCKTAPE_PASSWORD", "hunter2"),
    ];
    let command = claude(given).command(&at(5), Some("s1"));
    let std = command.as_std();
    let envs: Vec<(OsString, Option<OsString>)> = std
        .get_envs()
        .map(|(name, value)| (name.to_owned(), value.map(ToOwned::to_owned)))
        .collect();
    let wanted: Vec<(OsString, Option<OsString>)> = [
        ("ANTHROPIC_API_KEY", "gate-token"),
        ("ANTHROPIC_BASE_URL", "http://gate:8080"),
        ("CLAUDE_CONFIG_DIR", "/s/bridge.json.claude"),
        ("PATH", "/usr/bin"),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), Some(value.into())))
    .collect();
    assert_eq!(envs, wanted);
    let args: Vec<String> = std
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let at_end = &args[args.len() - 4..];
    assert_eq!(at_end, ["--resume", "s1", "--model", "opus"]);
    let mcp: Value =
        serde_json::from_str(&args[args.iter().position(|arg| arg == "--mcp-config").unwrap() + 1])
            .unwrap();
    assert_eq!(
        mcp["mcpServers"]["ducktape"]["args"],
        json!(["bridge", "relay", "/s/mcp.sock", "chat/room/5"])
    );
    let own = claude(vec![("CLAUDE_CONFIG_DIR", "/pod/claude")]).env();
    assert_eq!(
        own,
        [(
            OsString::from("CLAUDE_CONFIG_DIR"),
            OsString::from("/pod/claude")
        )]
    );
}

#[test]
fn claudes_stream_json_reads_as_session_text_tools_and_answers() {
    let read = |line: &str| claude::parse(line);
    assert_eq!(
        read(r#"{"type":"system","subtype":"init","session_id":"abc"}"#),
        [Event::Session("abc".into())]
    );
    assert_eq!(
        read(
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"mcp__ducktape__network_call","input":{}}]}}"#
        ),
        [
            Event::Text("hi".into()),
            Event::Doing("mcp__ducktape__network_call".into())
        ]
    );
    assert!(
        read(r#"{"type":"assistant","parent_tool_use_id":"t1","message":{"content":[{"type":"text","text":"sub"}]}}"#)
            .is_empty()
    );
    assert_eq!(
        read(r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#),
        [Event::Answer("done".into())]
    );
    assert_eq!(
        read(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#),
        [Event::Answer("claude stopped: error_max_turns".into())]
    );
    assert!(read("not json").is_empty());
    assert_eq!(
        claude::user("hello"),
        json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "text", "text": "hello" }] }, "parent_tool_use_id": null, "session_id": "default" })
    );
}

#[test]
fn only_the_accounts_written_to_rows_are_its_seqs_and_only_a_wakeon_arms_a_wake() {
    let prefix = to_prefix(ME);
    let key = [prefix.as_slice(), &9u64.to_be_bytes()].concat();
    assert_eq!(seq_of(&prefix, &key, true), Some(9));
    assert_eq!(seq_of(&prefix, &key, false), None);
    let other = [to_prefix(61).as_slice(), &9u64.to_be_bytes()].concat();
    assert_eq!(seq_of(&prefix, &other, true), None);
    assert_eq!(seq_of(&prefix, &key[..key.len() - 1], true), None);
    let answer = json!({ "ok": true, "returned": 4, "landed": 10 });
    let wake_on = json!({ "wake_on": { "event": { "height": { "height": 9 } } } });
    assert_eq!(armed("wake", &wake_on, &answer), Some(4));
    assert_eq!(
        armed("wake", &json!({ "cancel": { "wake": 4 } }), &answer),
        None
    );
    assert_eq!(armed("chat", &wake_on, &answer), None);
}
