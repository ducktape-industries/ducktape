//! The huddle media lane on TWO REAL NODES: real processes, a real overlay,
//! the real `GET /v1/call/ws?channel=<id>`, `call_wire` frames on the socket,
//! and a fan-out set read from consensus exactly as the app's roster poll
//! reads it. The node side is the realtime media executor driving the call
//! guest; this suite speaks only the CLIENT contract, so it holds whichever
//! guest sits behind the lane.
//!
//! What it pins, in order:
//!   1. admission is by token: no token and a wrong token are refused before
//!      the upgrade, an unnamed channel is a 400, the workspace token upgrades.
//!   2. `recipients` steers fan-out BOTH ways — it is the send list and the
//!      receive admission roster. A session steered to nobody refuses a peer's
//!      media (counted on the voice plane), and re-steering it to the roster
//!      is the whole cure: no reconnect, no rejoin.
//!   3. an Opus frame from A's client reaches B's client as the `[0x01]`
//!      downlink: one re-encoded 20 ms frame that decodes to audible PCM.
//!   4. a JPEG captured frame from A reaches B as `[0x03]` stamped with A's
//!      node key, whole.
//!   5. A's `beacon{speaking:true}` reaches B as `peer_beacon{speaking:true}`.
//!   6. B's `keyframe_request{peer:A}` reaches A as `keyframe_request`, and
//!      every server control frame parses — a `rate_hint` names a rung of the
//!      sender's ladder.

mod common;

use std::time::Duration;

use chat::{
    Channel, ChatMsg, ChatQuery, ChatReply, HUDDLE_JOIN_NS, PostPolicy, huddle_join_preimage,
};
use common::{Cluster, hex, unhex};
use commonware_cryptography::{Signer as _, ed25519};
use futures::{SinkExt as _, StreamExt as _};
use media_service::call_wire::{self, CapturedFrame};
use media_service::video::RATE_LADDER_KBPS;
use media_service::voice::{FRAME_SAMPLES, VoiceDecoder, VoiceEncoder};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// mesh formation + overlay handshake on a possibly-loaded box; polls exit
/// early, so generosity is free.
const READY: Duration = Duration::from_secs(180);
/// budget for one submitted op to finalize and read back elsewhere.
const FINALIZE: Duration = Duration::from_secs(60);
/// budget for media to cross once the fan-out set is right: the voice lane's
/// own first-contact handshake plus a few 20 ms frames.
const CROSSES: Duration = Duration::from_secs(45);

const CHANNEL: &str = "eng";
/// the downlink is a mix the hub re-encoded; a peer who is speaking at ±8000
/// decodes well above this, silence and codec noise well below.
const AUDIBLE: i16 = 1000;

type CallSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type CallSink = futures::stream::SplitSink<CallSocket, Message>;
type CallStream = futures::stream::SplitStream<CallSocket>;

/// two founders on one overlay, converged, tunnels carrying traffic.
fn two_founders() -> Cluster {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    // media rides the OVERLAY and nothing else: with no `wireguard_listen`
    // the node binds no realtime lane at all.
    cluster.wireguard = true;
    for idx in 0..2 {
        cluster.spawn(idx);
    }
    for idx in 0..2 {
        cluster.wait_marker(idx, "rpc listening on", READY);
        cluster.wait_marker(idx, "converged root_hash=", READY);
        cluster.wait_marker(idx, "peer handshake COMPLETE", READY);
    }
    cluster
}

/// This node's mesh identity as `/v1/status` publishes it — the SAME app
/// surface `backend::join_huddle` reads to fill `JoinHuddle.node`.
fn status_key(cluster: &Cluster, idx: usize) -> String {
    let (status, body) = cluster.http(idx, "GET", "/v1/status", None);
    assert_eq!(status, 200, "node {idx} status: {body}");
    body["public_key"]
        .as_str()
        .expect("the node publishes its mesh identity")
        .to_string()
}

/// A `JoinHuddle` op for the node at `seed` naming itself: `cluster.submit`
/// self-authors (the node re-signs with its own key regardless of origin), so
/// the author IS `node_hex`'s bytes here — `node_proof` is that same node
/// signing the join under [`HUDDLE_JOIN_NS`], proof it holds the key it names.
fn join_huddle_op(channel_id: &str, seed: u64, node_hex: &str) -> ChatMsg {
    let node = unhex(node_hex);
    let preimage = huddle_join_preimage(channel_id, &node);
    let node_proof = ed25519::PrivateKey::from_seed(seed)
        .sign(HUDDLE_JOIN_NS, &preimage)
        .as_ref()
        .to_vec();
    ChatMsg::JoinHuddle {
        channel_id: channel_id.into(),
        node,
        node_proof,
    }
}

/// The channel record as consensus holds it — the row the app's channel
/// view projects the huddle roster from.
fn channel_record(cluster: &Cluster, idx: usize, channel_id: &str) -> Option<Channel> {
    let reply = cluster.query(
        idx,
        "chat",
        &chat::encode_query(&ChatQuery::Channel {
            channel_id: channel_id.into(),
        }),
    )?;
    let ChatReply::Channel(found) = chat::decode_reply(&reply).ok()? else {
        return None;
    };
    found
}

/// Every node key on the huddle roster, in join order.
fn roster_nodes(cluster: &Cluster, idx: usize, channel_id: &str) -> Vec<String> {
    channel_record(cluster, idx, channel_id)
        .map(|channel| channel.huddle.iter().map(|m| hex(&m.node)).collect())
        .unwrap_or_default()
}

/// The fan-out set THE APP WOULD COMPUTE on this node: the roster's node keys
/// minus this device's own. Ours in the set would aim this device's media at
/// itself; the peer's missing from it is the silence.
fn fanout(cluster: &Cluster, idx: usize, channel_id: &str, me: &str) -> Vec<String> {
    roster_nodes(cluster, idx, channel_id)
        .into_iter()
        .filter(|node| node != me)
        .collect()
}

/// Datagrams node `idx`'s VOICE lane threw away because the sender was not
/// admitted: `rogue_datagrams` (a live flow, sender not in its roster) and
/// `unregistered_datagrams` (no flow at all) are the same fact from the
/// sender's side — nothing this node will ever hand up.
fn refused_voice_datagrams(cluster: &Cluster, idx: usize) -> u64 {
    const REFUSALS: [&str; 2] = [
        r#"kind="rogue_datagrams""#,
        r#"kind="unregistered_datagrams""#,
    ];
    let (status, body) = cluster.http_text(idx, "/metrics");
    assert_eq!(status, 200, "metrics exposition: {body}");
    body.lines()
        .filter(|line| line.starts_with("ducktape_dataplane_drops{"))
        .filter(|line| line.contains(r#"service="voice""#))
        .filter(|line| REFUSALS.iter().any(|kind| line.contains(kind)))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

fn call_url(cluster: &Cluster, idx: usize, query: &str) -> String {
    format!(
        "{}/v1/call/ws?{query}",
        cluster.http_base(idx).replacen("http://", "ws://", 1)
    )
}

fn workspace_token(cluster: &Cluster, idx: usize) -> String {
    noded::services::read_link_token(&cluster.workspace(idx))
        .expect("the node minted its workspace service-link token")
}

/// the http status an upgrade was refused with, before any socket existed.
async fn refused_upgrade(url: &str) -> u16 {
    match tokio_tungstenite::connect_async(url).await {
        Ok(_) => panic!("{url}: the upgrade must be refused"),
        Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("{url}: refused by transport, not by the node: {other}"),
    }
}

async fn open_call(cluster: &Cluster, idx: usize, channel_id: &str) -> CallSocket {
    let token = workspace_token(cluster, idx);
    let url = call_url(cluster, idx, &format!("channel={channel_id}&token={token}"));
    let Ok((socket, _)) = tokio_tungstenite::connect_async(&url).await else {
        panic!("node {idx}: the authenticated call socket did not upgrade");
    };
    socket
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

fn recipients_frame(peers: &[String]) -> Message {
    text(serde_json::json!({ "type": "recipients", "peers": peers }))
}

fn beacon_frame(speaking: bool) -> Message {
    text(serde_json::json!({
        "type": "beacon",
        "muted": false,
        "camera_on": true,
        "sharing": false,
        "speaking": speaking,
    }))
}

fn keyframe_request_frame(peer: &str) -> Message {
    text(serde_json::json!({ "type": "keyframe_request", "peer": peer }))
}

/// A 500 Hz square, not a constant frame: SILK's high-pass strips DC, so a
/// steady level would decode to converged silence at the far end.
fn loud_pcm(tick: usize) -> [i16; FRAME_SAMPLES] {
    let mut pcm = [0i16; FRAME_SAMPLES];
    for (i, sample) in pcm.iter_mut().enumerate() {
        let n = tick * FRAME_SAMPLES + i;
        *sample = if (n / 48).is_multiple_of(2) { 8000 } else { -8000 };
    }
    pcm
}

/// one second of the client's microphone, Opus-encoded ahead of the pump
/// (the socket carries encoded frames; PCM never crosses it).
fn loud_opus() -> Vec<Vec<u8>> {
    let mut encoder = VoiceEncoder::new(32_000).expect("encoder");
    (0..50)
        .map(|tick| encoder.encode(&loud_pcm(tick)).expect("encode"))
        .collect()
}

/// Position-dependent fill, so a reassembly regression breaks exact equality
/// instead of hiding behind uniform bytes.
fn camera_frame(seed: u8) -> Vec<u8> {
    (0..5000)
        .map(|i| ((i * 7 + usize::from(seed)) % 251) as u8)
        .collect()
}

/// What one leg publishes for as long as the test holds the handle: a mic
/// frame every 20 ms, a keyframe every 100 ms, and the 1 Hz beacon saying
/// this person is speaking — the same three things the app's session pumps.
async fn publish(mut out: CallSink, video: Vec<u8>) {
    let opus = loud_opus();
    let mut audio = tokio::time::interval(Duration::from_millis(20));
    let mut camera = tokio::time::interval(Duration::from_millis(100));
    let mut beacon = tokio::time::interval(Duration::from_secs(1));
    let mut ts: u32 = 0;
    let mut tick: usize = 0;
    loop {
        let frame = tokio::select! {
            _ = audio.tick() => {
                tick += 1;
                Message::Binary(call_wire::encode_audio(&opus[tick % opus.len()]).into())
            }
            _ = camera.tick() => {
                ts += 100;
                Message::Binary(call_wire::encode_captured(&CapturedFrame {
                    keyframe: true,
                    ts_ms: ts,
                    data: video.clone(),
                }).into())
            }
            _ = beacon.tick() => beacon_frame(true),
        };
        if out.send(frame).await.is_err() {
            return;
        }
    }
}

/// one server control frame, parsed against `CallServerControl`'s vocabulary;
/// anything else on the text leg is a contract break.
enum ServerControl {
    KeyframeRequest,
    PeerBeacon { peer: String, speaking: bool },
    RateHint { max_kbps: u32 },
}

fn parse_server_control(raw: &str) -> ServerControl {
    let control: serde_json::Value = serde_json::from_str(raw).expect("server control is json");
    match control["type"].as_str() {
        Some("keyframe_request") => ServerControl::KeyframeRequest,
        Some("peer_beacon") => ServerControl::PeerBeacon {
            peer: control["peer"].as_str().expect("peer_beacon names a peer").into(),
            speaking: control["speaking"]
                .as_bool()
                .expect("peer_beacon carries speaking"),
        },
        Some("rate_hint") => ServerControl::RateHint {
            max_kbps: u32::try_from(
                control["max_kbps"]
                    .as_u64()
                    .expect("rate_hint carries max_kbps"),
            )
            .expect("max_kbps fits u32"),
        },
        other => panic!("not a CallServerControl frame ({other:?}): {raw}"),
    }
}

/// Everything one participant must observe of another before the huddle is
/// worth the name: their voice in the downlink mix, their camera frame whole
/// and stamped with their key, their `speaking` on the control leg — and,
/// when `keyframe_kicked`, the hub relaying a peer's keyframe request to us.
async fn hear_and_see(inbound: &mut CallStream, peer: &str, video: &[u8], keyframe_kicked: bool) {
    let mut decoder = VoiceDecoder::new().expect("decoder");
    let (mut heard, mut seen, mut speaking, mut kicked) = (false, false, false, !keyframe_kicked);
    while let Some(Ok(message)) = inbound.next().await {
        match message {
            Message::Binary(bytes) => {
                if let Some(opus) = call_wire::decode_audio(&bytes) {
                    let pcm = decoder.decode(opus).expect("the downlink is one Opus frame");
                    heard |= pcm.iter().any(|sample| sample.abs() > AUDIBLE);
                } else if let Some(frame) = call_wire::decode_peer(&bytes) {
                    assert_eq!(hex(&frame.peer), peer, "a frame from someone else");
                    assert_eq!(frame.data, video, "the camera frame must cross whole");
                    seen = true;
                } else {
                    panic!("a binary frame that is neither audio nor peer video: {bytes:?}");
                }
            }
            Message::Text(raw) => match parse_server_control(&raw) {
                ServerControl::KeyframeRequest => kicked = true,
                ServerControl::PeerBeacon {
                    peer: named,
                    speaking: is_speaking,
                } => speaking |= named == peer && is_speaking,
                ServerControl::RateHint { max_kbps } => assert!(
                    RATE_LADDER_KBPS.contains(&max_kbps),
                    "a rate hint names a rung of the sender's ladder: {max_kbps}"
                ),
            },
            _ => {}
        }
        if heard && seen && speaking && kicked {
            return;
        }
    }
    panic!(
        "the call socket closed before {peer} was heard ({heard}), seen ({seen}), speaking ({speaking}) and keyframe-kicked ({kicked})"
    );
}

#[test]
#[ignore = "needs the realtime media executor and its `GET /v1/call/ws` route (feat/media-executor): at dev the route answers 404 and no node binds the chat voice/video lanes"]
fn admission_is_by_token() {
    let rt = Runtime::new().expect("runtime");
    let cluster = two_founders();
    let token = workspace_token(&cluster, 0);
    rt.block_on(async {
        // no credential at all: not a signed request, not the workspace.
        assert_eq!(
            refused_upgrade(&call_url(&cluster, 0, &format!("channel={CHANNEL}"))).await,
            401
        );
        // a token that is not this workspace's secret is the same refusal.
        let wrong = format!("channel={CHANNEL}&token={}", token.chars().rev().collect::<String>());
        assert_eq!(refused_upgrade(&call_url(&cluster, 0, &wrong)).await, 401);
        // the right token but no channel to derive the flows from.
        assert_eq!(
            refused_upgrade(&call_url(&cluster, 0, &format!("channel=&token={token}"))).await,
            400
        );
        // the right token upgrades, whether or not anyone is in the huddle.
        let mut socket = open_call(&cluster, 0, CHANNEL).await;
        socket.close(None).await.expect("close");
    });
}

#[test]
#[ignore = "needs the realtime media executor and its `GET /v1/call/ws` route (feat/media-executor): at dev the route answers 404 and no node binds the chat voice/video lanes"]
fn media_crosses_the_lane_once_recipients_steer_the_fan_out() {
    let rt = Runtime::new().expect("runtime");
    let cluster = two_founders();

    // ONE KEY VOCABULARY. The app stamps `/v1/status`.public_key into
    // `JoinHuddle`; the lane admits by the validator signer's key. If those
    // ever diverge the roster names nobody the media lane knows, and the
    // symptom is exactly the silence below with no bug in sight.
    let node_a = status_key(&cluster, 0);
    let node_b = status_key(&cluster, 1);
    assert_eq!(node_a, hex(&Cluster::identity(0)));
    assert_eq!(node_b, hex(&Cluster::identity(1)));
    assert_ne!(node_a, node_b);

    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: CHANNEL.into(),
            name: "Engineering".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&join_huddle_op(CHANNEL, 0, &node_a)),
    );
    cluster.await_committed(0, "A alone in the huddle", FINALIZE, || {
        (roster_nodes(&cluster, 0, CHANNEL) == [node_a.clone()]).then_some(())
    });

    // A's session opens while A is alone, and steers to what the roster says:
    // nobody.
    let mut leg_a = rt.block_on(open_call(&cluster, 0, CHANNEL));
    let alone = fanout(&cluster, 0, CHANNEL, &node_a);
    assert!(alone.is_empty(), "A joined an empty huddle: {alone:?}");
    rt.block_on(leg_a.send(recipients_frame(&alone)))
        .expect("A's session takes its fan-out");

    // B joins the huddle LATER.
    cluster.submit(
        1,
        "chat",
        &chat::encode_msg(&join_huddle_op(CHANNEL, 1, &node_b)),
    );
    for idx in 0..2 {
        cluster.await_committed(idx, "both nodes read a two-person roster", FINALIZE, || {
            let roster = roster_nodes(&cluster, idx, CHANNEL);
            (roster == [node_a.clone(), node_b.clone()]).then_some(())
        });
    }

    let leg_b = rt.block_on(open_call(&cluster, 1, CHANNEL));
    let (mut b_out, mut b_in) = leg_b.split();
    let b_sees = fanout(&cluster, 1, CHANNEL, &node_b);
    assert_eq!(
        b_sees,
        std::slice::from_ref(&node_a),
        "the newcomer's roster names A"
    );
    rt.block_on(b_out.send(recipients_frame(&b_sees)))
        .expect("B's session takes its fan-out");
    // B asks A for a keyframe: the hub must relay it to A's client as
    // `keyframe_request`.
    rt.block_on(b_out.send(keyframe_request_frame(&node_a)))
        .expect("B's session takes the keyframe request");

    // B talks and shows a camera from here to the end of the test.
    let b_video = camera_frame(3);
    let b_pump = rt.spawn(publish(b_out, b_video.clone()));

    // RECIPIENTS ARE THE ADMISSION ROSTER, COUNTED: A's voice lane refuses
    // B's datagrams because A was steered to a set without B. A refusal the
    // node itself reports — not an absence waited out.
    cluster.await_committed(0, "node A refuses the unsteered peer's media", CROSSES, || {
        (refused_voice_datagrams(&cluster, 0) > 0).then_some(())
    });

    // THE CURE, AND NOTHING ELSE: A re-reads the roster from consensus and
    // steers to it. No reconnect, no rejoin — the one message the session's
    // 1 s poll sends when the roster moves.
    let steered = fanout(&cluster, 0, CHANNEL, &node_a);
    assert_eq!(
        steered,
        std::slice::from_ref(&node_b),
        "A's poll now names B"
    );
    let (mut a_out, mut a_in) = leg_a.split();
    rt.block_on(a_out.send(recipients_frame(&steered)))
        .expect("A's session takes the re-steer");

    rt.block_on(async {
        tokio::time::timeout(CROSSES, hear_and_see(&mut a_in, &node_b, &b_video, true))
            .await
            .expect("B's voice, camera, speaking and keyframe kick must reach A once the fan-out names B");
    });

    // ...and the other way, which is the half a one-sided fix would leave
    // broken: "my mic doesn't work" and "I can't see them" are one bug.
    let a_video = camera_frame(11);
    let a_pump = rt.spawn(publish(a_out, a_video.clone()));
    rt.block_on(async {
        tokio::time::timeout(CROSSES, hear_and_see(&mut b_in, &node_a, &a_video, false))
            .await
            .expect("A's voice, camera and speaking must reach B");
    });

    a_pump.abort();
    b_pump.abort();
}
