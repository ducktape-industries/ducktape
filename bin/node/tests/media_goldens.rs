//! The native media engine's golden vectors: the exact bytes every huddle
//! wire layout produces and refuses, and what the NATIVE hub mixes from three
//! speakers' Opus frames. The realtime call guest ports `media_service`'s
//! voice engine to wasm; these fixtures are the reference it must reproduce
//! byte for byte, and this suite is the proof the native crate still does.
//!
//! Every vector is keyed by the `media_service` symbol that produces it
//! (`tests/fixtures/media-goldens/vectors.json`); the two mixed 20 ms frames
//! sit beside it as raw `i16` little-endian PCM. `regenerate_goldens` (ignored)
//! rewrites the fixtures from the native crate after an INTENDED wire change —
//! a mismatch here without one is a wire regression, not a stale fixture.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_plane::sim::{LinkModel, SimNet};
use data_plane::{
    AdmissionPolicy, DataPlane, DatagramFlow, DatagramPolicy, FlowId, PeerId, PlaneConfig, Service,
    sim::SimEndpoint,
};
use media_service::call_wire::{self, CapturedFrame, PeerFrame};
use media_service::video::{
    Assembly, CallControl, RATE_LADDER_KBPS, Reassembler, VideoHeader, decode_fragment,
    encode_fragment, fragment_frame, frame::MAX_FRAGMENT_PAYLOAD, frame::MAX_FRAME_BYTES,
    step_down, step_up,
};
use media_service::voice::{
    FRAME_SAMPLES, MAX_ENCODED, MediaHeader, VoiceConfig, VoiceDecoder, VoiceEncoder, VoiceEngine,
    media,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

const VECTORS: &str = include_str!("fixtures/media-goldens/vectors.json");
const MIXED: [&[u8]; 2] = [
    include_bytes!("fixtures/media-goldens/mixed-tick0.pcm"),
    include_bytes!("fixtures/media-goldens/mixed-tick1.pcm"),
];

/// the lane id `crates/modules/apps/chat/lanes.json` declares for `voice`.
const VOICE_LANE: Service = Service::from_lane_id(2);
/// the engine's own cadence — one jitter-buffer tick per frame.
const TICK: Duration = Duration::from_millis(20);
/// speakers in the mixing scenario: (square-wave half period in samples,
/// amplitude). Integer synthesis, so the PCM is bit-exact on every platform.
const SPEAKERS: [(usize, i16); 3] = [(48, 8000), (32, 6000), (20, 5000)];
/// frames each speaker sends: the engine's default prefill, so the first
/// playout tick renders seq 0 and the second seq 1.
const FRAMES: usize = 2;

type Vectors = BTreeMap<String, String>;

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn refused(error: impl std::fmt::Display) -> String {
    format!("refused:{error}")
}

fn pcm_bytes(pcm: &[i16; FRAME_SAMPLES]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// speaker `n`'s microphone at frame `tick`: a square wave continuing its
/// phase across frames (SILK strips DC, so a constant would decode to
/// silence).
fn square(speaker: usize, tick: usize) -> [i16; FRAME_SAMPLES] {
    let (half_period, amp) = SPEAKERS[speaker];
    let mut pcm = [0i16; FRAME_SAMPLES];
    for (i, sample) in pcm.iter_mut().enumerate() {
        let n = tick * FRAME_SAMPLES + i;
        *sample = if (n / half_period).is_multiple_of(2) {
            amp
        } else {
            -amp
        };
    }
    pcm
}

/// position-dependent fill, so a reassembly regression breaks exact equality
/// instead of hiding behind uniform bytes.
fn camera(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + seed) % 251) as u8).collect()
}

fn peer_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    key
}

/// each speaker's Opus frames, encoded in order by ONE encoder per speaker
/// (codec state carries from frame to frame, so the order is part of the
/// vector): `VoiceEncoder::new(VoiceConfig::default().bitrate_bits_per_sec)`.
fn opus_inputs() -> Vec<Vec<Vec<u8>>> {
    let bitrate = VoiceConfig::default().bitrate_bits_per_sec;
    (0..SPEAKERS.len())
        .map(|speaker| {
            let mut encoder = VoiceEncoder::new(bitrate).expect("encoder");
            (0..FRAMES)
                .map(|tick| encoder.encode(&square(speaker, tick)).expect("encode"))
                .collect()
        })
        .collect()
}

/// every wire layout, encode and refuse, keyed by producing symbol.
fn wire_vectors(opus: &[Vec<Vec<u8>>]) -> Vectors {
    let mut v = Vectors::new();
    let mut put = |name: &str, value: String| {
        v.insert(name.to_string(), value);
    };
    let peer = peer_key();
    let frame0 = &opus[0][0];

    // ---- media_service::voice::codec -------------------------------------
    put("voice::codec::MAX_ENCODED", MAX_ENCODED.to_string());
    let mut decoder = VoiceDecoder::new().expect("decoder");
    put(
        "voice::codec::VoiceDecoder::decode/refuse/empty_packet",
        refused(decoder.decode(&[]).unwrap_err()),
    );
    put(
        "voice::codec::VoiceDecoder::decode/refuse/toc_off_contract",
        refused(decoder.decode(&[0x58, 0x00]).unwrap_err()),
    );
    put(
        "voice::codec::VoiceDecoder::decode/refuse/packet_too_short",
        refused(decoder.decode(&frame0[..1]).unwrap_err()),
    );
    for (speaker, frames) in opus.iter().enumerate() {
        let mut decoder = VoiceDecoder::new().expect("decoder");
        for (tick, payload) in frames.iter().enumerate() {
            put(
                &format!("voice::codec::VoiceEncoder::encode/speaker{speaker}/tick{tick}"),
                hex::encode(payload),
            );
            put(
                &format!("voice::codec::VoiceDecoder::decode/speaker{speaker}/tick{tick}"),
                sha256(&pcm_bytes(&decoder.decode(payload).expect("decode"))),
            );
        }
    }

    // ---- media_service::voice::media (the mesh audio datagram) -----------
    let header = MediaHeader {
        epoch: 0x1112_1314,
        seq: 0x0102,
        timestamp: 0x0A0B_0C0D,
    };
    put(
        "voice::media::encode_frame",
        hex::encode(media::encode_frame(header, frame0).expect("frame")),
    );
    put(
        "voice::media::decode_frame/refuse/truncated",
        refused(media::decode_frame(&[0u8; media::MEDIA_HEADER_LEN - 1]).unwrap_err()),
    );
    put(
        "voice::media::encode_frame/refuse/payload_too_large",
        refused(media::encode_frame(header, &vec![0u8; media::MAX_OPUS_PAYLOAD + 1]).unwrap_err()),
    );

    // ---- media_service::call_wire (the /v1/call/ws binary leg) ----------
    put("call_wire::encode_audio", hex::encode(call_wire::encode_audio(frame0)));
    put(
        "call_wire::decode_audio/refuse/wrong_tag",
        call_wire::decode_audio(&[0x02, 0x01]).map_or("none".into(), hex::encode),
    );
    put(
        "call_wire::decode_audio/refuse/tag_only",
        call_wire::decode_audio(&[0x01]).map_or("none".into(), hex::encode),
    );
    put(
        "call_wire::decode_audio/refuse/over_max",
        call_wire::decode_audio(&[0x01; 2 + call_wire::MAX_AUDIO_PAYLOAD])
            .map_or("none".into(), hex::encode),
    );
    let captured = CapturedFrame {
        keyframe: true,
        ts_ms: 0x0102_0304,
        data: camera(64, 3),
    };
    put("call_wire::encode_captured", hex::encode(call_wire::encode_captured(&captured)));
    put(
        "call_wire::decode_captured/refuse/header_only",
        call_wire::decode_captured(&[0x02, 0x01, 0, 0, 0, 0])
            .map_or("none".into(), |f| hex::encode(f.data)),
    );
    put(
        "call_wire::decode_captured/refuse/wrong_tag",
        call_wire::decode_captured(&[0x03, 0x01, 0, 0, 0, 0, 0xAA])
            .map_or("none".into(), |f| hex::encode(f.data)),
    );
    let peer_frame = PeerFrame {
        peer,
        keyframe: false,
        ts_ms: 0x0A0B_0C0D,
        data: camera(64, 11),
    };
    put("call_wire::encode_peer", hex::encode(call_wire::encode_peer(&peer_frame)));
    put(
        "call_wire::decode_peer/refuse/header_only",
        call_wire::decode_peer(&[0x03; call_wire::WS_VIDEO_PEER_HEADER])
            .map_or("none".into(), |f| hex::encode(f.data)),
    );
    put(
        "call_wire::decode_peer/refuse/wrong_tag",
        call_wire::decode_peer(&[0x02; call_wire::WS_VIDEO_PEER_HEADER + 1])
            .map_or("none".into(), |f| hex::encode(f.data)),
    );

    // ---- media_service::video::frame + assembly (the mesh video datagrams)
    let picture = camera(MAX_FRAGMENT_PAYLOAD * 2 + 10, 5);
    let fragments = fragment_frame(9, 7, true, 12_345, &picture).expect("fragments");
    for (index, fragment) in fragments.iter().enumerate() {
        put(&format!("video::frame::fragment_frame/{index}"), hex::encode(fragment));
    }
    put(
        "video::frame::decode_fragment/refuse/truncated",
        refused(decode_fragment(&[0u8; 16]).unwrap_err()),
    );
    let bad_header = |frag_index: u16, frag_count: u16| {
        encode_fragment(
            VideoHeader {
                keyframe: false,
                frame_no: 0,
                frag_index,
                frag_count,
                ts_ms: 0,
                epoch: 0,
            },
            b"x",
        )
    };
    put(
        "video::frame::decode_fragment/refuse/zero_frag_count",
        refused(decode_fragment(&bad_header(0, 0)).unwrap_err()),
    );
    put(
        "video::frame::decode_fragment/refuse/index_out_of_range",
        refused(decode_fragment(&bad_header(2, 2)).unwrap_err()),
    );
    put(
        "video::frame::fragment_frame/refuse/empty",
        refused(fragment_frame(0, 0, false, 0, &[]).unwrap_err()),
    );
    put(
        "video::frame::fragment_frame/refuse/frame_too_large",
        refused(fragment_frame(0, 0, false, 0, &vec![0u8; MAX_FRAME_BYTES + 1]).unwrap_err()),
    );
    // fragments land out of order and the frame completes on the last one;
    // a repeat of any fragment after that is stale.
    let mut reassembler = Reassembler::default();
    let mut steps = Vec::new();
    let mut complete = None;
    for index in [2usize, 0, 1] {
        let (header, payload) = decode_fragment(&fragments[index]).expect("fragment");
        steps.push(match reassembler.insert(header, payload) {
            Assembly::Progress => "progress",
            Assembly::Stale => "stale",
            Assembly::Complete(frame) => {
                complete = Some(frame);
                "complete"
            }
        });
    }
    let (header, payload) = decode_fragment(&fragments[0]).expect("fragment");
    steps.push(match reassembler.insert(header, payload) {
        Assembly::Progress => "progress",
        Assembly::Stale => "stale",
        Assembly::Complete(_) => "complete",
    });
    let complete = complete.expect("the frame completes on its last fragment");
    put("video::assembly::Reassembler::insert/steps[2,0,1,0]", steps.join(","));
    put(
        "video::assembly::Reassembler::insert/complete",
        format!(
            "frame_no={} keyframe={} ts_ms={} data={}",
            complete.frame_no,
            complete.keyframe,
            complete.ts_ms,
            sha256(&complete.data)
        ),
    );
    assert_eq!(complete.data, picture, "reassembly must return the picture whole");
    put(
        "video::assembly::Reassembler::dropped_frames",
        reassembler.dropped_frames().to_string(),
    );

    // ---- media_service::video::control (the mesh call-control datagram) --
    put(
        "video::control::CallControl::KeyframeRequest",
        hex::encode(CallControl::KeyframeRequest.encode()),
    );
    put(
        "video::control::CallControl::Beacon{muted,camera_on,!sharing,speaking}",
        hex::encode(
            CallControl::Beacon {
                muted: true,
                camera_on: true,
                sharing: false,
                speaking: true,
            }
            .encode(),
        ),
    );
    put(
        "video::control::CallControl::RateHint{500}",
        hex::encode(CallControl::RateHint { max_kbps: 500 }.encode()),
    );
    put(
        "video::control::CallControl::decode/refuse/truncated_beacon",
        refused(CallControl::decode(&[0x02, 0x01]).unwrap_err()),
    );
    put(
        "video::control::CallControl::decode/refuse/unknown_tag",
        refused(CallControl::decode(&[99]).unwrap_err()),
    );
    put(
        "video::control::RATE_LADDER_KBPS",
        RATE_LADDER_KBPS.map(|r| r.to_string()).join(","),
    );
    put(
        "video::control::step_down/1200,800,500,300,700",
        [1200, 800, 500, 300, 700]
            .map(|r| step_down(r).to_string())
            .join(","),
    );
    put(
        "video::control::step_up/300,500,800,1200,700",
        [300, 500, 800, 1200, 700]
            .map(|r| step_up(r).to_string())
            .join(","),
    );

    // ---- the /v1/call/ws text leg: control json, tag = "type", snake_case
    // (`noded::CallClientControl` / `noded::CallServerControl`).
    let peer_hex = hex::encode(peer);
    put(
        "call_ws::CallClientControl::Recipients",
        serde_json::json!({"type": "recipients", "peers": [peer_hex]}).to_string(),
    );
    put(
        "call_ws::CallClientControl::Beacon",
        serde_json::json!({
            "type": "beacon", "muted": false, "camera_on": true, "sharing": false, "speaking": true
        })
        .to_string(),
    );
    put(
        "call_ws::CallClientControl::KeyframeRequest",
        serde_json::json!({"type": "keyframe_request", "peer": peer_hex}).to_string(),
    );
    put(
        "call_ws::CallServerControl::KeyframeRequest",
        serde_json::json!({"type": "keyframe_request"}).to_string(),
    );
    put(
        "call_ws::CallServerControl::PeerBeacon",
        serde_json::json!({
            "type": "peer_beacon", "peer": peer_hex,
            "muted": false, "camera_on": true, "sharing": false, "speaking": true
        })
        .to_string(),
    );
    put(
        "call_ws::CallServerControl::RateHint",
        serde_json::json!({"type": "rate_hint", "max_kbps": 500}).to_string(),
    );
    v
}

/// test stand-in for the node's consensus-derived admission view.
#[derive(Default)]
struct Admission {
    allowed: Mutex<Vec<(PeerId, Service, u64)>>,
}

impl AdmissionPolicy for Admission {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        self.allowed
            .lock()
            .expect("admission lock")
            .contains(&(peer, service, flow.as_u64()))
    }
}

/// the 3-speaker mixing scenario on the NATIVE engine: three raw voice-flow
/// senders each put `FRAMES` media datagrams (fixed epoch, seq 0.., the
/// golden Opus payloads) on the hub's flow; the hub's `VoiceEngine` pumps
/// them into per-speaker lanes and `playout` mixes one frame per tick.
/// Returns the mixed PCM per tick.
async fn mix_on_native_hub(opus: &[Vec<Vec<u8>>]) -> Vec<[i16; FRAME_SAMPLES]> {
    let hub = PeerId([9; 32]);
    let speakers: Vec<PeerId> = (1..=opus.len() as u8).map(|n| PeerId([n; 32])).collect();
    let net = SimNet::new();
    let flow = FlowId::derive(b"voice-channel:goldens");
    let admission = Arc::new(Admission::default());
    let link = LinkModel {
        latency: Duration::from_millis(5),
        bytes_per_sec: 1_000_000,
        drop_every: None,
        delay_every: None,
    };
    for speaker in &speakers {
        net.set_link(*speaker, hub, link);
    }
    {
        let mut allowed = admission.allowed.lock().expect("admission lock");
        for peer in speakers.iter().chain(std::iter::once(&hub)) {
            allowed.push((*peer, VOICE_LANE, flow.as_u64()));
        }
    }
    let plane = |peer: PeerId| {
        DataPlane::new(
            net.endpoint(peer),
            admission.clone(),
            PlaneConfig {
                bulk_bytes_per_sec: 600_000,
                bulk_burst_bytes: 16 * 1024,
            },
        )
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .expect("register voice flow")
    };
    let roster: Vec<[u8; 32]> = speakers.iter().map(|p| p.0).collect();
    let (_roster_tx, roster_rx) = watch::channel(roster);
    let engine = VoiceEngine::new(plane(hub), VoiceConfig::default(), roster_rx).expect("engine");
    let senders: Vec<DatagramFlow<SimEndpoint>> =
        speakers.iter().map(|peer| plane(*peer)).collect();
    for (sender, frames) in senders.iter().zip(opus) {
        for (seq, payload) in frames.iter().enumerate() {
            let header = MediaHeader {
                epoch: 1,
                seq: seq as u16,
                timestamp: (seq * FRAME_SAMPLES) as u32,
            };
            let frame = media::encode_frame(header, payload).expect("media frame");
            sender.send_to(hub, &frame).await.expect("send");
        }
    }
    // the paused clock only advances once every task is idle: by then the
    // sim link has delivered and the pump has buffered every datagram.
    tokio::time::sleep(TICK).await;
    let mixed: Vec<_> = (0..FRAMES).map(|_| engine.playout()).collect();
    let stats = engine.speaker_stats();
    assert_eq!(stats.len(), speakers.len(), "one lane per speaker");
    for speaker in &stats {
        assert_eq!(speaker.jitter.played as usize, FRAMES, "{speaker:?}");
        assert_eq!(speaker.jitter.gaps, 0, "{speaker:?}");
        assert_eq!(speaker.decode_errors, 0, "{speaker:?}");
        assert_eq!(speaker.bad_packets, 0, "{speaker:?}");
    }
    mixed
}

async fn produce() -> (Vectors, Vec<[i16; FRAME_SAMPLES]>) {
    let opus = opus_inputs();
    let mut vectors = wire_vectors(&opus);
    let mixed = mix_on_native_hub(&opus).await;
    for (tick, frame) in mixed.iter().enumerate() {
        vectors.insert(
            format!("voice::engine::VoiceEngine::playout/tick{tick}"),
            sha256(&pcm_bytes(frame)),
        );
    }
    (vectors, mixed)
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media-goldens")
}

#[tokio::test(start_paused = true)]
async fn the_native_engine_reproduces_the_committed_goldens() {
    let (vectors, mixed) = produce().await;
    let committed: Vectors = serde_json::from_str(VECTORS).expect("vectors.json parses");
    let drift: Vec<String> = committed
        .keys()
        .chain(vectors.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|name| committed.get(*name) != vectors.get(*name))
        .map(|name| {
            format!(
                "{name}\n  committed: {:?}\n  native:    {:?}",
                committed.get(name),
                vectors.get(name)
            )
        })
        .collect();
    assert!(drift.is_empty(), "wire vectors drifted:\n{}", drift.join("\n"));
    for (tick, frame) in mixed.iter().enumerate() {
        assert_eq!(
            pcm_bytes(frame),
            MIXED[tick],
            "mixed-tick{tick}.pcm drifted from the native mix"
        );
    }
}

/// the encoder and the mix are pure functions of their inputs: two runs in
/// one process agree, which is what makes a committed vector a contract.
#[tokio::test(start_paused = true)]
async fn the_goldens_are_deterministic_across_runs() {
    let first = produce().await;
    let second = produce().await;
    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1);
}

#[tokio::test(start_paused = true)]
#[ignore = "rewrites tests/fixtures/media-goldens from the native crate; run after an INTENDED wire change"]
async fn regenerate_goldens() {
    let (vectors, mixed) = produce().await;
    let dir = fixture_dir();
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let json = serde_json::to_string_pretty(&vectors).expect("json");
    std::fs::write(dir.join("vectors.json"), format!("{json}\n")).expect("write vectors");
    for (tick, frame) in mixed.iter().enumerate() {
        std::fs::write(dir.join(format!("mixed-tick{tick}.pcm")), pcm_bytes(frame))
            .expect("write pcm");
    }
}
