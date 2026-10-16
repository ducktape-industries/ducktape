//! One client's connection, over QUIC or a websocket, as the relay sees it: a stream of
//! inbound events read by tasks that know the transport, and an outlet the relay writes to that
//! a task per connection turns into control messages, group streams and datagrams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use halo::{Class, Pacer};
use live_wire::{
    Alias, CONTROL_BYTES, Control, DatagramHeader, Ending, GroupHeader, IDLE, MAX_STREAM_OBJECT,
    Priority, WS_MESSAGE, WsFrame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::futures::Notified;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use super::Config;
use super::groups::{self, GroupOut, Groups};

/// The control stream outranks every group stream of the connection.
const CONTROL_PRIORITY: i32 = 256;

#[derive(Debug)]
pub enum Inbound {
    Control(Control),
    Object {
        header: GroupHeader,
        object: u32,
        body: Bytes,
    },
    GroupEnd {
        alias: Alias,
        group: u64,
        complete: bool,
    },
    /// An object over its cap: the group is abandoned, the publisher refused, the session kept.
    Oversize {
        alias: Alias,
        group: u64,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
    /// Bytes that are not the wire: the session is closed as malformed.
    Garbled,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Control(Control),
    Object {
        header: GroupHeader,
        object: u32,
        body: Bytes,
        /// The track's priority, which the egress sets its group stream to: the one source of
        /// a track's priority.
        priority: Priority,
    },
    /// A whole cached group replayed to a late joiner, as one unit: the writer writes every
    /// object on one group stream, so a large replay is not N bounded sends that overflow and
    /// reset the group or close the session. `end` is `Some(complete)` when the cached group is
    /// closed; `None` when it is still being written and live objects follow on the same stream.
    Group {
        header: GroupHeader,
        priority: Priority,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
    },
    GroupEnd {
        alias: Alias,
        group: u64,
        complete: bool,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
}

/// Datagram-mode objects waiting for a connection or a peer: the newest per track, older ones
/// dropped, which is a datagram's semantics done in software.
#[derive(Default)]
pub struct Latest {
    slots: Mutex<HashMap<Alias, (DatagramHeader, Bytes)>>,
    ready: Notify,
}

impl Latest {
    /// Whether an older datagram for the track was dropped for this one.
    pub fn put(&self, header: DatagramHeader, payload: Bytes) -> bool {
        let replaced = self
            .slots
            .lock()
            .expect("the slots lock is never poisoned")
            .insert(header.alias, (header, payload))
            .is_some();
        self.ready.notify_one();
        replaced
    }

    pub fn take(&self) -> Vec<(DatagramHeader, Bytes)> {
        self.slots
            .lock()
            .expect("the slots lock is never poisoned")
            .drain()
            .map(|(_, datagram)| datagram)
            .collect()
    }

    /// Resolves once a datagram was put since it was last awaited (a permit is kept, so one
    /// put before the wait is not lost).
    pub fn ready(&self) -> Notified<'_> {
        self.ready.notified()
    }
}

/// The relay's handle on one connection's egress.
#[derive(Clone)]
pub struct Outlet {
    ordered: mpsc::Sender<Out>,
    latest: Arc<Latest>,
    closing: watch::Sender<Option<&'static str>>,
    /// Woken on every control message delivered: the session's loop re-reads what it holds, so
    /// one the relay emptied re-arms its idle timer without waiting on the client.
    told: Arc<Notify>,
}

impl std::fmt::Debug for Outlet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outlet").finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Queued,
    /// A datagram took the slot of an older one for the same track.
    Superseded,
    /// The ordered queue is full: the connection is closed as behind.
    Behind,
}

impl Outlet {
    pub fn send(&self, out: Out) -> Sent {
        if self.closed().is_some() {
            return Sent::Behind;
        }
        if let Out::Control(_) = &out {
            self.nudge();
        }
        if let Out::Datagram { header, payload } = out {
            return match self.latest.put(header, payload) {
                true => Sent::Superseded,
                false => Sent::Queued,
            };
        }
        match self.ordered.try_send(out) {
            Ok(()) => Sent::Queued,
            Err(_) => {
                self.close("behind");
                Sent::Behind
            }
        }
    }

    pub fn close(&self, reason: &'static str) {
        self.closing.send_if_modified(|closing| {
            let first = closing.is_none();
            if first {
                *closing = Some(reason);
            }
            first
        });
    }

    pub fn closed(&self) -> Option<&'static str> {
        *self.closing.borrow()
    }

    /// The session's requests changed with nothing to say about it: its loop re-reads what it
    /// holds, as it does on a control message.
    pub fn nudge(&self) {
        self.told.notify_one();
    }

    /// Resolves once a control message was delivered, or the session nudged, since it was last
    /// awaited (a permit is kept, so one delivered while the loop was in a step is not lost).
    pub async fn told(&self) {
        self.told.notified().await;
    }
}

/// What a connection's writer task takes from the outlet next.
enum Drained {
    Out(Out),
    Datagrams(Vec<(DatagramHeader, Bytes)>),
    Close,
}

struct Egress {
    ordered: mpsc::Receiver<Out>,
    latest: Arc<Latest>,
    closing: watch::Receiver<Option<&'static str>>,
}

impl Egress {
    /// What was queued before a close is still sent: a `GoAway` reaches the client.
    async fn next(&mut self) -> Drained {
        loop {
            if let Ok(out) = self.ordered.try_recv() {
                return Drained::Out(out);
            }
            let ready = self.latest.ready();
            let pending = self.latest.take();
            if !pending.is_empty() {
                return Drained::Datagrams(pending);
            }
            tokio::select! {
                out = self.ordered.recv() => {
                    return match out {
                        Some(out) => Drained::Out(out),
                        None => Drained::Close,
                    };
                }
                _ = self.closing.wait_for(|closing| closing.is_some()) => {
                    // A close follows what was sent before it; that goes out first.
                    if let Ok(out) = self.ordered.try_recv() {
                        return Drained::Out(out);
                    }
                    return Drained::Close;
                }
                () = ready => {}
            }
        }
    }
}

/// An outlet no connection drains, for a step that is tested without one.
#[cfg(test)]
pub fn unconnected() -> Outlet {
    outlet(&Config::default()).0
}

fn outlet(config: &Config) -> (Outlet, Egress) {
    let (ordered, receiving) = mpsc::channel(config.session_outbound.get());
    let latest = Arc::new(Latest::default());
    let (closing, closed) = watch::channel(None);
    let outlet = Outlet {
        ordered,
        latest: latest.clone(),
        closing,
        told: Arc::new(Notify::new()),
    };
    let egress = Egress {
        ordered: receiving,
        latest,
        closing: closed,
    };
    (outlet, egress)
}

/// A connection as the session loop reads and writes it, whatever carries it. Dropping it
/// closes the connection: the writer gets `linger` to put out the last message, then the
/// connection is closed under it.
pub struct Link {
    pub inbound: mpsc::Receiver<Inbound>,
    pub outlet: Outlet,
    pub datagrams: bool,
    pub path: &'static str,
    pub closing: watch::Receiver<Option<&'static str>>,
    linger: Duration,
    readers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    connection: Option<quinn::Connection>,
}

/// Resolves when the outlet was closed by anyone: fan-out that found the session behind, a
/// re-check that found no account, the node going away.
pub async fn closed_elsewhere(closing: &mut watch::Receiver<Option<&'static str>>) -> &'static str {
    let closed = closing.wait_for(|closing| closing.is_some()).await;
    closed
        .ok()
        .and_then(|closing| *closing)
        .unwrap_or("relay_gone")
}

impl Drop for Link {
    fn drop(&mut self) {
        self.outlet.close("session_ended");
        for reader in &self.readers {
            reader.abort();
        }
        let reason = self.outlet.closed().unwrap_or("session_ended");
        if let Some(writer) = self.writer.take() {
            tokio::spawn(lingering(
                writer,
                self.connection.take(),
                reason,
                self.linger,
            ));
        }
    }
}

/// The writer ends on its own once it put out what was queued before the close and the client
/// took it; one held up by a client that stopped reading is cut off at `linger`, and the
/// connection with it.
async fn lingering(
    mut writer: JoinHandle<()>,
    connection: Option<quinn::Connection>,
    reason: &'static str,
    linger: Duration,
) {
    tokio::select! {
        _ = &mut writer => {}
        () = tokio::time::sleep(linger) => writer.abort(),
    }
    if let Some(connection) = connection {
        connection.close(0u32.into(), reason.as_bytes());
    }
}

/// `len: u32` little-endian, then the bytes: how a control message and an object are framed.
pub async fn framed<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(bytes).await
}

/// quinn's stream priority: 0 is our highest, so the higher the byte the lower the rank.
fn stream_priority(priority: Priority) -> i32 {
    i32::from(u8::MAX - priority)
}

// ---- QUIC ----

impl Link {
    /// A client's QUIC connection whose first bidirectional stream, the control stream, is open.
    pub fn quic(
        connection: quinn::Connection,
        control: (quinn::SendStream, quinn::RecvStream),
        pacer: Pacer,
        config: &Config,
    ) -> Link {
        let (control_send, control_recv) = control;
        let (inbound, receiving) = mpsc::channel(config.session_inbound.get());
        let (outlet, egress) = outlet(config);
        let closing = egress.closing.clone();
        let readers = vec![
            tokio::spawn(quic_control_reading(control_recv, inbound.clone())),
            tokio::spawn(quic_groups_reading(connection.clone(), inbound.clone())),
            tokio::spawn(quic_datagrams_reading(connection.clone(), inbound)),
        ];
        let writer = tokio::spawn(quic_writing(
            connection.clone(),
            control_send,
            egress,
            pacer,
            config.group_queue.get(),
        ));
        Link {
            inbound: receiving,
            datagrams: connection.max_datagram_size().is_some(),
            outlet,
            path: "quic",
            closing,
            linger: config.linger,
            readers,
            writer: Some(writer),
            connection: Some(connection),
        }
    }
}

async fn read_control(recv: &mut quinn::RecvStream) -> Result<Option<Control>, ()> {
    let len = match recv.read_u32_le().await {
        Ok(len) => len,
        Err(_) => return Ok(None),
    };
    let fits = len <= CONTROL_BYTES;
    if !fits {
        return Err(());
    }
    let mut bytes = vec![0u8; len as usize];
    recv.read_exact(&mut bytes).await.map_err(|_| ())?;
    abi::decode(&bytes).map(Some).map_err(|_| ())
}

async fn quic_control_reading(mut recv: quinn::RecvStream, inbound: mpsc::Sender<Inbound>) {
    loop {
        let event = match read_control(&mut recv).await {
            Ok(Some(control)) => Inbound::Control(control),
            Ok(None) => Inbound::Closed,
            Err(()) => Inbound::Garbled,
        };
        let ends = matches!(event, Inbound::Closed | Inbound::Garbled);
        let delivered = inbound.send(event).await.is_ok();
        if ends || !delivered {
            return;
        }
    }
}

async fn quic_groups_reading(connection: quinn::Connection, inbound: mpsc::Sender<Inbound>) {
    while let Ok(stream) = connection.accept_uni().await {
        tokio::spawn(quic_group_reading(stream, inbound.clone()));
    }
}

async fn quic_group_reading(mut stream: quinn::RecvStream, inbound: mpsc::Sender<Inbound>) {
    let mut header = [0u8; GroupHeader::LEN];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let Ok(header) = abi::decode::<GroupHeader>(&header) else {
        let _ = inbound.send(Inbound::Garbled).await;
        return;
    };
    let mut object = 0u32;
    // FIN between objects is a clean end; a reset, or a FIN inside a prefix or an object,
    // leaves the group short. The first read error is the word: the stream says nothing after
    // it.
    let complete = loop {
        let len = match prefix(&mut stream).await {
            Prefix::Len(len) => len as usize,
            Prefix::Fin => break true,
            Prefix::Cut => break false,
        };
        let fits = len <= MAX_STREAM_OBJECT;
        if !fits {
            // The stream is stopped at the object that does not fit; the publisher hears why,
            // and the group ends short so no entry for it is left behind.
            let _ = stream.stop(0u32.into());
            let _ = inbound
                .send(Inbound::Oversize {
                    alias: header.alias,
                    group: header.group,
                })
                .await;
            let _ = inbound
                .send(Inbound::GroupEnd {
                    alias: header.alias,
                    group: header.group,
                    complete: false,
                })
                .await;
            return;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            break false;
        }
        let delivered = inbound
            .send(Inbound::Object {
                header,
                object,
                body: body.into(),
            })
            .await
            .is_ok();
        if !delivered {
            return;
        }
        object += 1;
    };
    let _ = inbound
        .send(Inbound::GroupEnd {
            alias: header.alias,
            group: header.group,
            complete,
        })
        .await;
}

enum Prefix {
    Len(u32),
    /// The stream ended cleanly where a prefix would start.
    Fin,
    /// The stream ended inside a prefix, or was reset.
    Cut,
}

async fn prefix(stream: &mut quinn::RecvStream) -> Prefix {
    let mut bytes = [0u8; 4];
    match stream.read(&mut bytes[..1]).await {
        Ok(None) => return Prefix::Fin,
        Ok(Some(_)) => {}
        Err(_) => return Prefix::Cut,
    }
    match stream.read_exact(&mut bytes[1..]).await {
        Ok(()) => Prefix::Len(u32::from_le_bytes(bytes)),
        Err(_) => Prefix::Cut,
    }
}

async fn quic_datagrams_reading(connection: quinn::Connection, inbound: mpsc::Sender<Inbound>) {
    while let Ok(datagram) = connection.read_datagram().await {
        let Some(event) = datagram_arrived(datagram) else {
            continue;
        };
        if inbound.send(event).await.is_err() {
            return;
        }
    }
}

fn datagram_arrived(mut bytes: Bytes) -> Option<Inbound> {
    let fits = bytes.len() >= 16;
    if !fits {
        return None;
    }
    let header = bytes.split_to(16);
    let header: DatagramHeader = abi::decode(&header).ok()?;
    Some(Inbound::Datagram {
        header,
        payload: bytes,
    })
}

async fn quic_writing(
    connection: quinn::Connection,
    mut control: quinn::SendStream,
    mut egress: Egress,
    pacer: Pacer,
    group_queue: usize,
) {
    let _ = control.set_priority(CONTROL_PRIORITY);
    let mut groups = Groups::new(group_queue);
    // One group writer per group stream, at the track's priority.
    let spawn = |priority: Priority| {
        let (connection, pacer) = (connection.clone(), pacer.clone());
        move |header, queue| {
            tokio::spawn(quic_group_writing(
                connection, pacer, header, priority, queue,
            ));
        }
    };
    loop {
        match egress.next().await {
            Drained::Out(Out::Control(message)) => {
                if framed(&mut control, &abi::encode(&message)).await.is_err() {
                    return;
                }
            }
            Drained::Out(Out::Object {
                header,
                object,
                body,
                priority,
            }) => {
                // The client egress has no counter of its own; the lane counts its drops.
                let _ = groups.object(header, object, body, spawn(priority));
            }
            Drained::Out(Out::Group {
                header,
                priority,
                objects,
                end,
            }) => {
                let _ = groups.batch(header, objects, end, spawn(priority));
            }
            Drained::Out(Out::GroupEnd {
                alias,
                group,
                complete,
            }) => groups.end(alias, group, complete),
            Drained::Out(Out::Datagram { .. }) => {}
            Drained::Datagrams(datagrams) => {
                for (header, payload) in datagrams {
                    let frame = [abi::encode(&header), payload.to_vec()].concat();
                    let _ = connection.send_datagram(frame.into());
                }
            }
            Drained::Close => {
                // The last control message reaches the client before the link that owns the
                // connection closes it: the stream is finished and its acknowledgement awaited.
                // The link's `linger` cuts a writer off whose client never acknowledges.
                let _ = control.finish();
                let _ = control.stopped().await;
                return;
            }
        }
    }
}

/// One egress group stream: opened at the track's priority, paced, the header then each object
/// as it is queued; finished when the group completes, reset when it is cut short.
async fn quic_group_writing(
    connection: quinn::Connection,
    pacer: Pacer,
    header: GroupHeader,
    priority: Priority,
    mut queue: mpsc::Receiver<GroupOut>,
) {
    let Ok(stream) = connection.open_uni().await else {
        return;
    };
    let _ = stream.set_priority(stream_priority(priority));
    let mut paced = pacer.pace(stream, Class::Client);
    if groups::written(&mut paced, &header, &mut queue).await {
        let _ = paced.shutdown().await;
    } else {
        let _ = paced.into_inner().reset(0u32.into());
    }
}

// ---- websocket ----

/// The upgrade every `/v1/live` door runs: messages and frames capped at `WS_MESSAGE`.
pub fn ws_upgrade(upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
    upgrade
        .max_message_size(WS_MESSAGE)
        .max_frame_size(WS_MESSAGE)
}

impl Link {
    pub fn ws(socket: WebSocket, config: &Config) -> Link {
        let (sink, stream) = socket.split();
        let (inbound, receiving) = mpsc::channel(config.session_inbound.get());
        let (outlet, egress) = outlet(config);
        let closing = egress.closing.clone();
        let readers = vec![tokio::spawn(ws_reading(stream, inbound))];
        let writer = tokio::spawn(ws_writing(sink, egress));
        Link {
            inbound: receiving,
            outlet,
            datagrams: false,
            path: "ws",
            closing,
            linger: config.linger,
            readers,
            writer: Some(writer),
            connection: None,
        }
    }
}

type WsStream = futures::stream::SplitStream<WebSocket>;
type WsSink = futures::stream::SplitSink<WebSocket, Message>;

/// Reads the socket until it closes, fails, or is silent for `IDLE`: the websocket's idle limit
/// is the QUIC transport's, and a client pings within it (the pong is the socket's own).
async fn ws_reading(mut stream: WsStream, inbound: mpsc::Sender<Inbound>) {
    let mut counting: HashMap<(Alias, u64), u32> = HashMap::new();
    while let Ok(Some(message)) = tokio::time::timeout(IDLE, stream.next()).await {
        let events = match message {
            Ok(Message::Binary(bytes)) => ws_arrived(bytes, &mut counting),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Text(_)) => vec![Inbound::Garbled],
            Ok(Message::Close(_)) | Err(_) => vec![Inbound::Closed],
        };
        for event in events {
            let ends = matches!(event, Inbound::Closed | Inbound::Garbled);
            let delivered = inbound.send(event).await.is_ok();
            if ends || !delivered {
                return;
            }
        }
    }
    let _ = inbound.send(Inbound::Closed).await;
}

/// The caps QUIC's framing enforces, enforced on the websocket's: a control message within
/// `CONTROL_BYTES`, an object within `MAX_STREAM_OBJECT`.
fn ws_arrived(bytes: Bytes, counting: &mut HashMap<(Alias, u64), u32>) -> Vec<Inbound> {
    let Ok(frame) = abi::decode::<WsFrame>(&bytes) else {
        return vec![Inbound::Garbled];
    };
    match frame {
        WsFrame::Control(control) => {
            let fits = bytes.len() <= CONTROL_BYTES as usize;
            match fits {
                true => vec![Inbound::Control(control)],
                false => vec![Inbound::Garbled],
            }
        }
        WsFrame::Group {
            header,
            objects,
            end,
        } => {
            let key = (header.alias, header.group);
            let next = counting.entry(key).or_default();
            let mut events = Vec::new();
            for body in objects {
                let fits = body.len() <= MAX_STREAM_OBJECT;
                if !fits {
                    counting.remove(&key);
                    events.push(Inbound::Oversize {
                        alias: header.alias,
                        group: header.group,
                    });
                    // The group ends short, so no entry for it is left behind.
                    events.push(Inbound::GroupEnd {
                        alias: header.alias,
                        group: header.group,
                        complete: false,
                    });
                    return events;
                }
                let object = *next;
                *next += 1;
                events.push(Inbound::Object {
                    header,
                    object,
                    body: body.into(),
                });
            }
            let complete = match end {
                Ending::More => None,
                Ending::Complete => Some(true),
                Ending::Cut => Some(false),
            };
            if let Some(complete) = complete {
                counting.remove(&key);
                events.push(Inbound::GroupEnd {
                    alias: header.alias,
                    group: header.group,
                    complete,
                });
            }
            events
        }
        WsFrame::Datagram { header, payload } => vec![Inbound::Datagram {
            header,
            payload: payload.into(),
        }],
    }
}

/// The bytes a `WsFrame::Group` takes beyond its objects: the tag, the header, the vector's
/// length and `end`.
const WS_GROUP_FRAME: usize = 1 + GroupHeader::LEN + 4 + 1;

/// A replayed group as websocket frames, each within `WS_MESSAGE` encoded (an object costs its
/// bytes and a 4-byte length), so one big group does not hold the socket from control; the last
/// frame ends the group as the cache holds it, complete or cut, and a still-open group's live
/// objects follow in later frames. A closed group with no objects is one ending frame.
fn ws_group_frames(header: GroupHeader, objects: &[Bytes], end: Option<bool>) -> Vec<WsFrame> {
    let mut frames: Vec<WsFrame> = Vec::new();
    let mut slice: Vec<Vec<u8>> = Vec::new();
    let mut bytes = WS_GROUP_FRAME;
    for body in objects {
        let costs = 4 + body.len();
        if bytes + costs > WS_MESSAGE && !slice.is_empty() {
            frames.push(WsFrame::Group {
                header,
                objects: std::mem::take(&mut slice),
                end: Ending::More,
            });
            bytes = WS_GROUP_FRAME;
        }
        bytes += costs;
        slice.push(body.to_vec());
    }
    let ending = match end {
        None => Ending::More,
        Some(true) => Ending::Complete,
        Some(false) => Ending::Cut,
    };
    if !slice.is_empty() || end.is_some() {
        frames.push(WsFrame::Group {
            header,
            objects: slice,
            end: ending,
        });
    }
    frames
}

async fn ws_writing(mut sink: WsSink, mut egress: Egress) {
    loop {
        let frames = match egress.next().await {
            Drained::Out(Out::Control(control)) => vec![WsFrame::Control(control)],
            Drained::Out(Out::Object { header, body, .. }) => vec![WsFrame::Group {
                header,
                objects: vec![body.to_vec()],
                end: Ending::More,
            }],
            Drained::Out(Out::Group {
                header,
                objects,
                end,
                ..
            }) => ws_group_frames(header, &objects, end),
            // The session's queue is ordered and closes the session when it overflows, so no
            // group reaches here with a hole: a group ends complete or cut as the relay said.
            Drained::Out(Out::GroupEnd {
                alias,
                group,
                complete,
            }) => vec![WsFrame::Group {
                header: GroupHeader { alias, group },
                objects: Vec::new(),
                end: match complete {
                    true => Ending::Complete,
                    false => Ending::Cut,
                },
            }],
            Drained::Out(Out::Datagram { .. }) => Vec::new(),
            Drained::Datagrams(datagrams) => datagrams
                .into_iter()
                .map(|(header, payload)| WsFrame::Datagram {
                    header,
                    payload: payload.to_vec(),
                })
                .collect(),
            Drained::Close => {
                let _ = sink.close().await;
                return;
            }
        };
        for frame in frames {
            let sent = sink.send(Message::Binary(abi::encode(&frame).into())).await;
            if sent.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_datagram_slot_keeps_the_newest_per_track() {
        let (outlet, egress) = outlet(&Config::default());
        let header = |object| DatagramHeader {
            alias: 1,
            group: 0,
            object,
        };
        assert_eq!(
            outlet.send(Out::Datagram {
                header: header(1),
                payload: Bytes::from_static(&[1])
            }),
            Sent::Queued
        );
        assert_eq!(
            outlet.send(Out::Datagram {
                header: header(2),
                payload: Bytes::from_static(&[2])
            }),
            Sent::Superseded
        );
        assert_eq!(
            egress.latest.take(),
            vec![(header(2), Bytes::from_static(&[2]))]
        );
    }

    #[test]
    fn an_ordered_queue_that_overflows_closes_the_connection_as_behind() {
        let config = Config::default();
        let (outlet, _egress) = outlet(&config);
        let message = Out::Control(Control::GoAway { in_ms: 1 });
        for _ in 0..config.session_outbound.get() {
            assert_eq!(outlet.send(message.clone()), Sent::Queued);
        }
        assert_eq!(outlet.send(message), Sent::Behind);
        assert_eq!(outlet.closed(), Some("behind"));
    }

    #[test]
    fn a_session_closed_as_behind_takes_nothing_more() {
        let config = Config::default();
        let (outlet, mut egress) = outlet(&config);
        let message = Out::Control(Control::GoAway { in_ms: 1 });
        for _ in 0..config.session_outbound.get() {
            assert_eq!(outlet.send(message.clone()), Sent::Queued);
        }
        // the dropped object: the queue is full, the session closes as behind
        assert_eq!(outlet.send(message.clone()), Sent::Behind);
        // the writer frees room, then the group's end arrives: it must be refused
        assert!(egress.ordered.try_recv().is_ok());
        let end = Out::GroupEnd {
            alias: 1,
            group: 0,
            complete: true,
        };
        assert_eq!(outlet.send(end), Sent::Behind);
        let mut left = 0;
        while let Ok(out) = egress.ordered.try_recv() {
            assert!(matches!(out, Out::Control(_)), "a group end was queued");
            left += 1;
        }
        assert_eq!(left, config.session_outbound.get() - 1);
    }

    #[test]
    fn a_websocket_group_numbers_its_objects_across_slices() {
        let header = GroupHeader { alias: 3, group: 7 };
        let mut counting = HashMap::new();
        let first = ws_arrived(
            abi::encode(&WsFrame::Group {
                header,
                objects: vec![vec![1], vec![2]],
                end: Ending::More,
            })
            .into(),
            &mut counting,
        );
        let second = ws_arrived(
            abi::encode(&WsFrame::Group {
                header,
                objects: vec![vec![3]],
                end: Ending::Complete,
            })
            .into(),
            &mut counting,
        );
        let objects: Vec<u32> = first
            .iter()
            .chain(&second)
            .filter_map(|event| match event {
                Inbound::Object { object, .. } => Some(*object),
                _ => None,
            })
            .collect();
        assert_eq!(objects, vec![0, 1, 2]);
        assert!(matches!(
            second.last(),
            Some(Inbound::GroupEnd { complete: true, .. })
        ));
        assert!(counting.is_empty());
    }

    #[test]
    fn a_websocket_frame_over_the_wire_caps_is_garbled_or_oversize() {
        let mut counting = HashMap::new();
        let control = WsFrame::Control(Control::Watch {
            request: 1,
            namespace: "n".repeat(CONTROL_BYTES as usize),
        });
        assert!(matches!(
            ws_arrived(abi::encode(&control).into(), &mut counting).as_slice(),
            [Inbound::Garbled]
        ));
        let header = GroupHeader { alias: 3, group: 7 };
        let group = WsFrame::Group {
            header,
            objects: vec![vec![0; MAX_STREAM_OBJECT + 1]],
            end: Ending::Complete,
        };
        assert!(matches!(
            ws_arrived(abi::encode(&group).into(), &mut counting).as_slice(),
            [
                Inbound::Oversize { alias: 3, group: 7 },
                Inbound::GroupEnd {
                    alias: 3,
                    group: 7,
                    complete: false,
                },
            ]
        ));
    }

    #[test]
    fn a_replayed_group_within_one_object_size_is_one_ending_frame() {
        let header = GroupHeader { alias: 1, group: 2 };
        let objects = [Bytes::from_static(b"a"), Bytes::from_static(b"bc")];
        let frames = ws_group_frames(header, &objects, Some(true));
        assert_eq!(
            frames,
            vec![WsFrame::Group {
                header,
                objects: vec![b"a".to_vec(), b"bc".to_vec()],
                end: Ending::Complete
            }]
        );
        assert_eq!(
            ws_group_frames(header, &[], Some(true)),
            vec![WsFrame::Group {
                header,
                objects: Vec::new(),
                end: Ending::Complete
            }]
        );
        assert!(ws_group_frames(header, &[], None).is_empty());
    }

    #[test]
    fn a_replayed_group_past_one_object_size_is_sliced_and_only_the_last_slice_ends_it() {
        let header = GroupHeader { alias: 1, group: 2 };
        let big = Bytes::from(vec![0; MAX_STREAM_OBJECT]);
        let objects = [big.clone(), Bytes::from_static(b"x"), big.clone()];
        let frames = ws_group_frames(header, &objects, Some(true));
        let ends = |frames: &[WsFrame]| -> Vec<Ending> {
            frames
                .iter()
                .map(|frame| match frame {
                    WsFrame::Group { end, .. } => *end,
                    WsFrame::Control(_) | WsFrame::Datagram { .. } => unreachable!(),
                })
                .collect()
        };
        // The one-byte object rides with the first big one (both fit the message); the second
        // big one does not, so it is the frame that ends the group.
        assert_eq!(ends(&frames), vec![Ending::More, Ending::Complete]);
        // A cached group abandoned replays cut, in the frame: nothing is left to infer.
        assert_eq!(
            ends(&ws_group_frames(header, &objects, Some(false))),
            vec![Ending::More, Ending::Cut]
        );
        // Many small objects slice the same way: every frame fits and every object crosses once.
        let small = vec![Bytes::from_static(b"x"); 70_000];
        let sliced = ws_group_frames(header, &small, Some(true));
        assert!(sliced.len() > 1);
        let carried: usize = sliced
            .iter()
            .map(|frame| match frame {
                WsFrame::Group { objects, .. } => objects.len(),
                WsFrame::Control(_) | WsFrame::Datagram { .. } => unreachable!(),
            })
            .sum();
        assert_eq!(carried, 70_000, "every object crosses once");
        let empties = vec![Bytes::new(); 20_000];
        for frame in
            frames
                .iter()
                .chain(&sliced)
                .chain(&ws_group_frames(header, &empties, Some(true)))
        {
            assert!(
                abi::encode(frame).len() <= WS_MESSAGE,
                "each frame fits the socket"
            );
        }
        let open = ws_group_frames(header, &objects, None);
        assert_eq!(open.len(), 2);
        assert!(matches!(
            open.last(),
            Some(WsFrame::Group {
                end: Ending::More,
                ..
            })
        ));
    }

    #[test]
    fn a_cut_frame_from_a_publisher_ends_its_group_short() {
        let mut counting = HashMap::new();
        let header = GroupHeader { alias: 1, group: 2 };
        let cut = WsFrame::Group {
            header,
            objects: vec![b"a".to_vec()],
            end: Ending::Cut,
        };
        let events = ws_arrived(abi::encode(&cut).into(), &mut counting);
        assert!(matches!(
            events.as_slice(),
            [
                Inbound::Object { object: 0, .. },
                Inbound::GroupEnd {
                    complete: false,
                    ..
                }
            ]
        ));
        assert!(counting.is_empty(), "the group is over: its count is gone");
    }

    #[test]
    fn every_control_told_wakes_the_session_loop_and_an_object_does_not() {
        use futures::FutureExt as _;
        let (outlet, _egress) = outlet(&Config::default());
        assert!(outlet.told().now_or_never().is_none(), "nothing told yet");
        // A refusal on the admit path is a control message like any other: it wakes the loop,
        // which re-reads the session's requests and re-arms its idle timer from them.
        outlet.send(Out::Control(Control::Refused {
            request: 1,
            code: live_wire::Code::Duplicate,
        }));
        assert!(
            outlet.told().now_or_never().is_some(),
            "a control message wakes the loop"
        );
        assert!(outlet.told().now_or_never().is_none(), "once per delivery");
        outlet.send(Out::Object {
            header: GroupHeader { alias: 1, group: 1 },
            object: 0,
            body: Bytes::from_static(b"a"),
            priority: 0,
        });
        assert!(
            outlet.told().now_or_never().is_none(),
            "an object is not a word to the session"
        );
    }
}
