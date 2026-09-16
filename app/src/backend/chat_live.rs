//! An agent run anchored to a chat message, shown live in its thread
//! while it runs. The chain carries only the anchor and the committed reply;
//! the progress rides the node's `run-output:<dispatch>` topic. The guest
//! interprets provider output and builds the presentation. A row
//! lives exactly as long as its run is pending in `runs`: the entry prunes in
//! the block that posts the reply, so the committed message takes the row's
//! place.
//!
//! One reading covers the node. The Chat guest selects rows for its room.
//!
//! A reading is stamped with the CONNECTION it was taken over — endpoint, chain
//! id and connect attempt — because room ids are not unique across networks and
//! the endpoint is not unique across chains. A reading that crossed with a
//! reconnect is DROPPED by the handler (`live_agents_stale`), never assigned:
//! its emptiness describes a connection nobody is on, and writing it would
//! blank the cards the current one just installed.

use super::*;
use futures::SinkExt as _;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const PENDING_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// One agent run in flight, as the row drawn under its anchor.
///
/// Every field here is DRAWN or decides WHERE the card is drawn. Nothing that
/// changes on a clock belongs in it: the guest memoizes the timeline on this
/// value's hash, so an elapsed-millis field would have rebuilt the whole
/// message window on every poll tick for as long as any agent was working.
#[derive(Clone, Debug, Default, Hash, PartialEq, serde::Serialize)]
pub struct LiveAgentRow {
    /// The room the anchor is in — the one fact that decides whether this row
    /// reaches the screen at all.
    pub channel_id: String,
    pub anchor_seq: i64,
    /// The anchor's thread root when the anchor is itself a reply, else 0: the
    /// reply posts into that thread, so the rail draws the card there.
    pub thread_root: i64,
    pub run_id: String,
    /// the run's address, and the key its output stream is watched under
    pub dispatch_id: String,
    pub agent: String,
    pub status: String,
    /// Public committed facts used when this device cannot read stdout.
    pub public_progress: Option<serde_json::Value>,
    pub output: Vec<String>,
    pub output_error: String,
}

/// One reading of the node's pending runs, stamped with the connection it was
/// taken over: the endpoint, the chain that endpoint was serving, and the
/// connect attempt.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct LiveAgentNotice {
    pub rpc: String,
    pub chain_id: String,
    pub generation: i64,
    /// the key seated for signing when this reading was asked for, "" for none.
    pub signer_key: String,
    pub rows: Vec<LiveAgentRow>,
}

/// Whether this reading describes a connection that is no longer the one on
/// screen. A caller that gets `true` must DROP the reading and leave the rows
/// it already has alone — a stale reading is not evidence that nothing is
/// running, and assigning its (empty) rows would blank the cards the CURRENT
/// connection just installed until the next poll.
///
/// THE ENDPOINT IS NOT AN IDENTITY. A workspace switch brings the node back on
/// the same loopback port, so the url alone would call a new chain's reading
/// current — the same trap `live_resynced` names `chain_left_behind`, one plane
/// over. `chain_id` is the node's own pushed status and `generation` is the
/// connect attempt, so the three together name THIS connection to THIS chain.
///
/// AND THE SEAT IS PART OF THE IDENTITY. On a device that does not host the
/// node, what a reading was entitled to read is the SEATED KEY's
/// (`Reach::Signed`) — and a Settings unlock or lock moves that seat with the
/// endpoint, the chain and the connect attempt all unchanged
/// (Settings unlock/lock does not bump the connect generation). Without
/// this term, a reading taken under the previous key stays "current" across a
/// key switch.
pub fn live_agents_stale(
    notice: &LiveAgentNotice,
    rpc: &str,
    chain_id: &str,
    generation: i64,
    signer_key: &str,
) -> bool {
    notice.rpc != rpc
        || notice.chain_id != chain_id
        || notice.generation != generation
        || notice.signer_key != signer_key
}

/// The live rows, keyed by the dispatch whose output feeds them. The dispatch
/// id never reaches the screen — it is the watcher's handle, nothing the
/// reader can act on.
type Rows = Arc<Mutex<BTreeMap<String, LiveAgentRow>>>;

/// The connection a reading is stamped with, carried verbatim from the
/// subscription's own arguments — this task never learns it for itself, so a
/// reading cannot claim a connection the app was not on when it was asked for.
#[derive(Clone)]
struct Taken {
    rpc: String,
    chain_id: String,
    generation: i64,
    /// the seat this reading's entitlement belongs to — see
    /// [`live_agents_stale`].
    signer_key: String,
}

/// How many times a dropped output stream is re-dialed before the row keeps the
/// failure it last reported and stops trying. The pending poll is the clock, so
/// the attempts are one poll apart — a re-dial is never a second concurrent
/// watcher for the same run.
const MAX_OUTPUT_DIALS: u32 = 5;

/// Internal refusal marker. The snapshot replaces it with public committed
/// progress; keeping it internally prevents retries of an unauthorized read.
const OUTPUT_UNAVAILABLE: &str = "Working · progress unavailable from this device";

/// One run's output watcher and how many times it has been dialed.
struct Watcher {
    handle: tokio::task::JoinHandle<()>,
    dials: u32,
}

/// How this device can prove it may read a run's output. ONE discriminant,
/// decided once per subscription, because the two proofs reach the node by
/// different routes and a `bool` could not say which to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// it HOSTS this node: the 0600 service-link token out of the node's own
    /// workspace, presented on the subscribe frame.
    Workspace,
    /// it is pointed at a node it does not host, with the user key unlocked: the
    /// data-plane signature trio over a `?run=<dispatch>` upgrade. The node
    /// admits it for that ONE run, and only if that key created it
    /// (`noded::stream::admit_run_reader`), so a run someone else asked for is
    /// refused here the same as a stranger's would be.
    Signed,
    /// Neither. The card uses public committed progress, not private stdout.
    Nothing,
}

/// What the poll must do about one pending run's output watcher. ONE tagged
/// value, because "is there an entry in the map" was the whole question before
/// and it was the wrong one: a watcher whose socket dropped leaves a FINISHED
/// handle in the map, and a `contains_key` reads that as "being watched" — so a
/// single transient websocket failure left the run with no watcher for the rest
/// of its life.
#[derive(Debug, PartialEq)]
enum Dial {
    /// This device may not read THIS run's output. Never dialed, and never
    /// re-dialed: it is an entitlement, not a transient failure — whether it was
    /// settled before the first dial ([`Reach::Nothing`]) or by the node
    /// refusing the run as another key's.
    Unreadable,
    /// No watcher yet.
    First,
    /// A live watcher is on it — never a second one for one run.
    Watching,
    /// Its watcher finished early; dial again, this many times tried so far.
    Again(u32),
    /// It has been dialed [`MAX_OUTPUT_DIALS`] times. The row keeps the failure
    /// its last attempt folded in, which is what the reader needs to see.
    GaveUp,
}

/// Decide from the entitlement and the watcher's own LIVENESS — never from its
/// presence in the map.
///
/// `refused` is the node's answer about THIS run, folded into the row by the
/// watcher: a proof this device cannot make for this dispatch is as settled as
/// having no proof at all, so it spends no dials.
fn dial_for(reach: Reach, refused: bool, watcher: Option<(bool, u32)>) -> Dial {
    let unreadable = reach == Reach::Nothing || refused;
    if unreadable {
        return Dial::Unreadable;
    }
    let Some((finished, dials)) = watcher else {
        return Dial::First;
    };
    if !finished {
        return Dial::Watching;
    }
    if dials < MAX_OUTPUT_DIALS {
        return Dial::Again(dials);
    }
    Dial::GaveUp
}

fn snapshot(taken: &Taken, rows: &Rows) -> LiveAgentNotice {
    let rows = rows.lock().unwrap_or_else(|e| e.into_inner());
    LiveAgentNotice {
        rpc: taken.rpc.clone(),
        chain_id: taken.chain_id.clone(),
        generation: taken.generation,
        signer_key: taken.signer_key.clone(),
        rows: rows
            .values()
            .cloned()
            .map(|mut row| {
                let private_output = row.status == OUTPUT_UNAVAILABLE;
                if private_output {
                    row.status.clear();
                    row.public_progress.get_or_insert_with(
                        || serde_json::json!({"sessions":null,"delegations":null}),
                    );
                    row.output.clear();
                    row.output_error.clear();
                }
                row
            })
            .collect(),
    }
}

/// Every agent run this node holds in flight, live. Polls `runs` for the
/// pending set and keeps one output watcher per run; a run leaving the pending
/// set takes its row with it, which is how a completed, failed or cancelled
/// run reconciles — the committed reply (or nothing) stands alone afterwards.
pub fn chat_live_agents(
    rpc: String,
    chain_id: String,
    generation: i64,
    signer_key: String,
) -> futures::stream::BoxStream<'static, LiveAgentNotice> {
    use futures::StreamExt as _;
    let (sender, receiver) = tokio::sync::mpsc::channel::<LiveAgentNotice>(64);
    tokio::spawn(async move {
        // WHICH PROOF THIS DEVICE CAN MAKE, asked once. A device that hosts the
        // node reads the 0600 token out of its workspace; a device pointed at a
        // node it does not host signs the upgrade for ONE run, which the node
        // admits only for the key that created it. With the user key locked
        // there is no proof to make at all, and that is not a transient failure:
        // the card still carries the agent, the room, the anchor and a Stop, and
        // shows the public committed progress instead of private stdout.
        //
        // READ OFF THE SUBSCRIPTION'S OWN ARGUMENT, never from the process's
        // signer: `signer_key` is what this lane is KEYED on, so deciding the
        // entitlement from anything else would let the two disagree — a seat
        // taken after this subscription started would be a proof this task
        // believes in while no restart ever arrives to use it.
        let reach = if workspace_at(&rpc).is_some() {
            Reach::Workspace
        } else if signer_key.is_empty() {
            Reach::Nothing
        } else {
            Reach::Signed
        };
        let taken = Taken {
            rpc: rpc.clone(),
            chain_id,
            generation,
            signer_key,
        };
        let rows: Rows = Arc::default();
        let mut watchers: BTreeMap<String, Watcher> = BTreeMap::new();
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        while !sender.is_closed() {
            let discovery = chat_background(
                &rpc,
                serde_json::json!({"kind":"live_runs","labels":labels}),
            )
            .await;
            let Ok(discovery) = discovery else {
                tokio::time::sleep(PENDING_POLL).await;
                continue;
            };
            let Ok(next_labels) = serde_json::from_value(discovery["labels"].clone()) else {
                tokio::time::sleep(PENDING_POLL).await;
                continue;
            };
            labels = next_labels;
            let anchored: Vec<_> = discovery["records"]
                .as_array()
                .into_iter()
                .flatten()
                .collect();
            let progress_runs: Vec<String> = {
                let rows = rows.lock().unwrap_or_else(|error| error.into_inner());
                anchored
                    .iter()
                    .filter(|record| {
                        reach == Reach::Nothing
                            || rows
                                .get(record["dispatch_id"].as_str().unwrap_or_default())
                                .is_some_and(|row| row.status == OUTPUT_UNAVAILABLE)
                    })
                    .filter_map(|record| record["run_id"].as_str().map(str::to_owned))
                    .collect()
            };
            let progress = if progress_runs.is_empty() {
                serde_json::Value::Null
            } else {
                chat_background(
                    &rpc,
                    serde_json::json!({"kind":"run_progress","runs":progress_runs}),
                )
                .await
                .unwrap_or_default()
            };
            let mut seen = Vec::new();
            for record in anchored {
                let dispatch = record["dispatch_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                seen.push(dispatch.clone());
                // the node's own answer about this run, as the watcher left it.
                let refused = rows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&dispatch)
                    .is_some_and(|row| row.status == OUTPUT_UNAVAILABLE);
                let dial = dial_for(
                    reach,
                    refused,
                    watchers
                        .get(&dispatch)
                        .map(|watcher| (watcher.handle.is_finished(), watcher.dials)),
                );
                // SEATED ONCE AND THEN LEFT ALONE: a re-dial must not discard
                // the activity the dropped watcher already folded in, and an
                // unwatched row must not be rewritten every poll.
                let unseated = !rows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&dispatch);
                if unseated {
                    let agent_id = record["agent_id"].as_str().unwrap_or_default();
                    let row = LiveAgentRow {
                        channel_id: record["channel_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        anchor_seq: record["anchor_seq"].as_i64().unwrap_or(0),
                        thread_root: record["thread_root"].as_i64().unwrap_or(0),
                        run_id: record["run_id"].as_str().unwrap_or_default().to_string(),
                        dispatch_id: dispatch.clone(),
                        agent: labels
                            .get(agent_id)
                            .cloned()
                            .unwrap_or_else(|| agent_id.to_string()),
                        status: match reach {
                            Reach::Workspace | Reach::Signed => String::new(),
                            Reach::Nothing => OUTPUT_UNAVAILABLE.into(),
                        },
                        ..LiveAgentRow::default()
                    };
                    rows.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(dispatch.clone(), row);
                }
                if dial == Dial::Unreadable {
                    let run_id = record["run_id"].as_str().unwrap_or_default();
                    let public_progress = progress
                        .get(run_id)
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"sessions":null,"delegations":null}));
                    if let Some(row) = rows
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&dispatch)
                    {
                        row.public_progress = Some(public_progress);
                    }
                }
                let dials = match dial {
                    Dial::Unreadable | Dial::Watching | Dial::GaveUp => continue,
                    Dial::First => 1,
                    Dial::Again(tried) => tried + 1,
                };
                watchers.insert(
                    dispatch.clone(),
                    Watcher {
                        handle: tokio::spawn(watch_live_output(
                            taken.clone(),
                            reach,
                            dispatch,
                            rows.clone(),
                            sender.clone(),
                        )),
                        dials,
                    },
                );
            }
            // WHAT LEFT THE PENDING SET, read off the ROWS and not off the
            // watchers. A device that may not read output has no watchers at
            // all, so a sweep over their keys would have left every settled
            // run's card standing on screen for the life of the session.
            let gone: Vec<String> = rows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|dispatch| !seen.contains(dispatch))
                .cloned()
                .collect();
            for dispatch in gone {
                if let Some(watcher) = watchers.remove(&dispatch) {
                    watcher.handle.abort();
                }
                rows.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&dispatch);
            }
            if sender.send(snapshot(&taken, &rows)).await.is_err() {
                break;
            }
            tokio::time::sleep(PENDING_POLL).await;
        }
        // THE LANE IS GONE, SO ARE ITS SOCKETS. The loop ends when the
        // subscription is dropped — a reconnect, a network switch, or the SEAT
        // changing, all of which re-key it — and a `JoinHandle` dropped is not a
        // task stopped. Without this, a socket admitted under the previous key
        // would stay open until its next output line.
        for watcher in watchers.into_values() {
            watcher.handle.abort();
        }
    });
    futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (event, receiver))
    })
    .boxed()
}

/// `/v1/ws?run=<dispatch>` — the signed arm's path AND the exact string its
/// signature covers. Spelled once: a signature over a different path than the
/// request carries is a refusal with no diagnosis.
fn run_reader_path(dispatch: &str) -> String {
    format!("/v1/ws?run={dispatch}")
}

/// The upgrade request for one run's output: the ws address this node answers on
/// plus [`run_reader_path`]'s query, carrying the signature trio as headers.
///
/// Built from the same string the signature covered — see the test, and
/// `app::call::ws_request`, which is this shape for the huddle socket.
fn run_reader_request(
    rpc: &str,
    dispatch: &str,
    signed: [(&'static str, String); 3],
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let mut request = format!("{}?run={dispatch}", agent_ws_url(rpc))
        .into_client_request()
        .map_err(|error| format!("could not address the node: {error}"))?;
    for (name, value) in signed {
        let value = HeaderValue::from_str(&value)
            .map_err(|error| format!("the signature is not a header value: {error}"))?;
        request
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    Ok(request)
}

/// Did the node refuse to admit this device as the run's reader, as opposed to
/// the socket failing?
///
/// THE DIFFERENCE DECIDES WHETHER IT IS DIALED AGAIN. A refused upgrade is an
/// entitlement — asked once, folded as the unavailable status, never re-dialed.
/// A connection that never got an answer is a flaky socket and is worth the
/// [`MAX_OUTPUT_DIALS`] budget. Reading both as "unavailable" would have pinned
/// the card to that message for a node that was merely restarting.
fn refused_the_reader(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::http::StatusCode;
    matches!(error, Error::Http(response)
        if response.status() == StatusCode::FORBIDDEN
            || response.status() == StatusCode::UNAUTHORIZED)
}

/// Open the node's event socket for ONE run, with whichever proof this device
/// can make, and subscribe to its output ring.
///
/// The two arms present the proof at different moments — the token rides the
/// subscribe FRAME, the signature rides the UPGRADE — which is why this is one
/// function over a discriminant and not a token that is sometimes empty.
async fn open_run_output(
    rpc: &str,
    reach: Reach,
    dispatch: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let topic = format!("run-output:{dispatch}");
    let (mut socket, token) = match reach {
        Reach::Nothing => return Err(OUTPUT_UNAVAILABLE.to_string()),
        Reach::Workspace => {
            let (_, workspace) =
                workspace_at(rpc).ok_or_else(|| "no local workspace for this node".to_string())?;
            let token = read_link_token(&workspace)?;
            let (socket, _) = tokio_tungstenite::connect_async(agent_ws_url(rpc))
                .await
                .map_err(|error| format!("could not open the node event stream: {error}"))?;
            (socket, Some(token))
        }
        Reach::Signed => {
            let node_key = crate::backend::node_public_key(rpc).await?;
            let signed = crate::backend::seated_request_headers(
                "GET",
                &run_reader_path(dispatch),
                &node_key,
                b"",
            )
            .await
            .ok_or_else(|| OUTPUT_UNAVAILABLE.to_string())?;
            let request = run_reader_request(rpc, dispatch, signed)?;
            // THE NODE DECIDES HERE, not on the subscribe: an upgrade it does
            // not admit as this run's creator is refused as HTTP, so the socket
            // never opens.
            let (socket, _) = tokio_tungstenite::connect_async(request)
                .await
                .map_err(|error| {
                    if refused_the_reader(&error) {
                        OUTPUT_UNAVAILABLE.to_string()
                    } else {
                        format!("could not open the node event stream: {error}")
                    }
                })?;
            (socket, None)
        }
    };
    let mut subscribe = serde_json::json!({"op": "subscribe", "topics": [topic]});
    if let Some(token) = token {
        subscribe["token"] = serde_json::Value::String(token);
    }
    socket
        .send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|error| format!("could not subscribe to the agent run: {error}"))?;
    Ok(socket)
}

async fn watch_live_output(
    taken: Taken,
    reach: Reach,
    dispatch: String,
    rows: Rows,
    sender: tokio::sync::mpsc::Sender<LiveAgentNotice>,
) {
    let rpc = taken.rpc.clone();
    use futures::StreamExt as _;
    let watch = async {
        let topic = format!("run-output:{dispatch}");
        let mut socket = open_run_output(&rpc, reach, &dispatch).await?;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_ref()) else {
                continue;
            };
            if let Some(detail) = subscription_refusal(&value) {
                return Err(detail);
            }
            if value["topic"].as_str() != Some(topic.as_str()) {
                continue;
            }
            let Some(line) = value["item"]["line"].as_str() else {
                continue;
            };
            {
                let mut rows = rows.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(row) = rows.get_mut(&dispatch) {
                    row.output_error.clear();
                    row.output.push(line.to_owned());
                }
            }
            if sender.send(snapshot(&taken, &rows)).await.is_err() {
                return Ok(());
            }
        }
        Ok::<(), String>(())
    };
    if let Err(message) = watch.await {
        // AN ENTITLEMENT IS NOT A FAILURE. A node that would not admit this
        // device as the run's reader leaves an internal refusal sentinel.
        // `dial_for` reads it as settled, so it is asked once and never
        // re-dialed; `snapshot` shows public progress instead. Every other
        // failure is an error the reader can act on.
        {
            let mut rows = rows.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(row) = rows.get_mut(&dispatch) {
                match message.as_str() {
                    OUTPUT_UNAVAILABLE => row.status = message,
                    _ => row.output_error = message,
                }
            }
        }
        let _ = sender.send(snapshot(&taken, &rows)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DROPPED OUTPUT STREAM IS RE-DIALED, and the presence of a handle is not
    /// evidence that anything is watching. `contains_key` was the whole test
    /// before, and a finished task stays in the map — so one transient websocket
    /// failure left the run unwatched for the rest of its life while the card
    /// sat on whatever status it had reached.
    #[test]
    fn a_finished_watcher_is_redialed_until_the_budget_runs_out() {
        const READABLE: bool = false;
        assert_eq!(
            dial_for(Reach::Workspace, READABLE, None),
            Dial::First,
            "nothing is watching it yet"
        );
        assert_eq!(
            dial_for(Reach::Workspace, READABLE, Some((false, 1))),
            Dial::Watching,
            "a live watcher is left alone — never a second one for one run"
        );
        assert_eq!(
            dial_for(Reach::Workspace, READABLE, Some((true, 1))),
            Dial::Again(1),
            "its socket dropped, and the handle sitting in the map said nothing"
        );
        assert_eq!(
            dial_for(
                Reach::Workspace,
                READABLE,
                Some((true, MAX_OUTPUT_DIALS - 1))
            ),
            Dial::Again(MAX_OUTPUT_DIALS - 1),
            "the last attempt inside the budget"
        );
        assert_eq!(
            dial_for(Reach::Workspace, READABLE, Some((true, MAX_OUTPUT_DIALS))),
            Dial::GaveUp,
            "past the budget the row keeps the failure it last reported"
        );
        // A REMOTE DEVICE IS DIALED ON THE SAME TERMS. Its proof is a signature
        // instead of a token, and the node decides per run — so the re-dial
        // budget is about the socket, exactly as it is for the host device.
        assert_eq!(
            dial_for(Reach::Signed, READABLE, None),
            Dial::First,
            "a seated key is a proof this device can make"
        );
        assert_eq!(
            dial_for(Reach::Signed, READABLE, Some((true, 1))),
            Dial::Again(1)
        );
    }

    /// AN ENTITLEMENT IS ASKED ONCE. Two devices cannot read a run's stdout: one
    /// with no proof to offer at all (no workspace, key locked), and one whose
    /// signature the node would not accept for THIS run — it is not the key that
    /// created it. Neither is a flaky socket, so neither spends a re-dial, and
    /// the card still earns its place from the pending poll alone.
    #[test]
    fn a_device_that_may_not_read_output_never_dials_for_it() {
        for watcher in [None, Some((false, 0)), Some((true, 2))] {
            assert_eq!(
                dial_for(Reach::Nothing, false, watcher),
                Dial::Unreadable,
                "no state of a watcher makes an unprovable read dialable"
            );
            // the node refused this run to a device that CAN sign — someone
            // else asked for it.
            assert_eq!(
                dial_for(Reach::Signed, true, watcher),
                Dial::Unreadable,
                "a run that is not this key's stays unread, however the watcher sits"
            );
        }
        assert_eq!(
            OUTPUT_UNAVAILABLE, "Working · progress unavailable from this device",
            "the internal refusal marker remains distinct from public progress"
        );
    }

    #[test]
    fn refused_output_publishes_only_public_progress_without_redialing() {
        let taken = Taken {
            rpc: "http://node".into(),
            chain_id: "chain".into(),
            generation: 1,
            signer_key: "key".into(),
        };
        let rows: Rows = Arc::default();
        rows.lock().unwrap().insert(
            "dispatch".into(),
            LiveAgentRow {
                status: OUTPUT_UNAVAILABLE.into(),
                public_progress: Some(serde_json::json!({"sessions":null,"delegations":{"delegations":[{"status":"pending"}]}})),
                output: vec!["SECRET provider output".into()],
                ..LiveAgentRow::default()
            },
        );
        let notice = snapshot(&taken, &rows);
        let row = &notice.rows[0];
        assert!(row.status.is_empty());
        assert_eq!(
            row.public_progress.as_ref().unwrap()["delegations"]["delegations"][0]["status"],
            "pending"
        );
        assert!(row.output.is_empty());
        let encoded = serde_json::to_value(row).unwrap();
        assert!(!encoded.to_string().contains("SECRET"));
        let status = &rows.lock().unwrap()["dispatch"].status;
        assert_eq!(
            dial_for(Reach::Signed, status == OUTPUT_UNAVAILABLE, None),
            Dial::Unreadable
        );
    }

    /// THE SIGNATURE COVERS THE PATH THE REQUEST CARRIES. A proof over
    /// `/v1/ws?run=x` on a request to `/v1/ws` is refused with no diagnosis, so
    /// the path is spelled ONCE and the request is built from it.
    #[test]
    fn a_signed_run_read_carries_the_trio_over_the_exact_path_it_asks_for() {
        let dispatch = "a".repeat(64);
        let signed = [
            ("x-ducktape-key", "k".to_string()),
            ("x-ducktape-ts", "1".to_string()),
            ("x-ducktape-sig", "s".to_string()),
        ];
        let request =
            run_reader_request("http://127.0.0.1:8844", &dispatch, signed.clone()).expect("built");
        assert_eq!(
            request.uri().to_string(),
            format!("ws://127.0.0.1:8844/v1/ws?run={dispatch}")
        );
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            run_reader_path(&dispatch),
            "the signed string and the asked-for string are one string"
        );
        for (name, value) in signed {
            assert_eq!(request.headers().get(name).unwrap(), value.as_str());
        }
    }

    /// A REFUSED UPGRADE AND A DEAD SOCKET ARE DIFFERENT ANSWERS. One is an
    /// entitlement and is asked once; the other is worth the whole re-dial
    /// budget. Reading a restarting node as "unavailable from this device" would
    /// have pinned that message on the card for the rest of the run.
    #[test]
    fn only_an_http_refusal_settles_the_entitlement() {
        use tokio_tungstenite::tungstenite::Error;
        use tokio_tungstenite::tungstenite::http::{Response, StatusCode};
        let http = |status: StatusCode| {
            Error::Http(
                Response::builder()
                    .status(status)
                    .body(None)
                    .expect("built"),
            )
        };
        assert!(refused_the_reader(&http(StatusCode::FORBIDDEN)));
        assert!(refused_the_reader(&http(StatusCode::UNAUTHORIZED)));
        assert!(
            !refused_the_reader(&http(StatusCode::SERVICE_UNAVAILABLE)),
            "a node that cannot answer yet is not a node that said no"
        );
        assert!(
            !refused_the_reader(&Error::ConnectionClosed),
            "a dropped socket is re-dialable"
        );
        assert!(!refused_the_reader(&Error::Io(std::io::Error::other(
            "down"
        ))));
    }

    /// THE CONNECTION GUARD, and the endpoint is the WEAKEST third of it. A
    /// workspace switch brings the node back on the same loopback port, so a
    /// reading still in flight from the chain she left carries the url she is
    /// on — it has to be refused on the chain id or the connect attempt, and a
    /// caller that refuses it must leave the current rows alone rather than
    /// assign its emptiness.
    #[test]
    fn a_reading_from_a_connection_she_has_left_is_refused() {
        let here = "http://127.0.0.1:8844";
        let mine = "aa11";
        let notice = LiveAgentNotice {
            rpc: here.into(),
            chain_id: "testnet#abcd".into(),
            generation: 7,
            signer_key: mine.into(),
            rows: vec![LiveAgentRow {
                channel_id: "general".into(),
                anchor_seq: 2,
                agent: "Chief Duck".into(),
                ..LiveAgentRow::default()
            }],
        };
        assert!(
            !live_agents_stale(&notice, here, "testnet#abcd", 7, mine),
            "the reading for the connection on screen stands"
        );
        assert!(
            live_agents_stale(&notice, "http://127.0.0.1:9844", "testnet#abcd", 7, mine),
            "another endpoint"
        );
        assert!(
            live_agents_stale(&notice, here, "othernet#0f0f", 7, mine),
            "SAME URL, NEW CHAIN — a workspace switch keeps the port, so the \
             url alone would have called this reading current"
        );
        assert!(
            live_agents_stale(&notice, here, "testnet#abcd", 8, mine),
            "same url and chain, but a reconnect has happened since"
        );
        // THE SEAT MOVES WITHOUT THE CONNECTION MOVING. Settings unlocks and
        // locks in place and bumps no `connect_generation`, so a reading taken
        // under the previous key would otherwise still be "current" — on a
        // remote device that reading's entitlement WAS that key's.
        assert!(
            live_agents_stale(&notice, here, "testnet#abcd", 7, "bb22"),
            "same connection, a different key seated since"
        );
        assert!(
            live_agents_stale(&notice, here, "testnet#abcd", 7, ""),
            "same connection, the seat has been locked since"
        );
    }
}
