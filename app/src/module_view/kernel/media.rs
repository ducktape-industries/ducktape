//! Session-scoped PCM devices, image capture, and JPEG renderer resources.
//! These operations know no room, peer identity, transport or call control.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{Guest, Replies, runtime};

static DEVICE_OWNER: AtomicBool = AtomicBool::new(false);

struct Lease;

impl Drop for Lease {
    fn drop(&mut self) {
        crate::video::reset();
        DEVICE_OWNER.store(false, Ordering::Release);
    }
}

struct Audio {
    id: u64,
    _device: crate::call::AudioThread,
    muted: Arc<AtomicBool>,
    playout: Arc<Mutex<crate::call::PlayoutRing>>,
    /// one decoder per peer being heard, in first-heard order. A voice codec
    /// carries state across a peer's frames, so a decoder belongs to a
    /// stream, not to the device — and the device is where the codec lives,
    /// beside the microphone and the playout ring.
    decoders: Vec<(String, media_service::voice::VoiceDecoder)>,
    pump: tokio::task::JoinHandle<()>,
}

/// As many peers as the call guest seats. Reaching it means a call churned
/// through more than this many distinct speakers, and the oldest decoder is
/// dropped: that peer's next frame costs one frame of silence, where keeping
/// every decoder ever seen would cost the call unbounded memory.
const MAX_DECODERS: usize = 32;

/// The capture bitrate: the voice engine's default, ~80 bytes per 20 ms
/// frame against a 1 275-byte ceiling.
const VOICE_BITRATE: i32 = 32_000;

/// one peer's audio for one playout tick.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerFrame {
    peer: String,
    frame: Vec<u8>,
}

type Decoders = Vec<(String, media_service::voice::VoiceDecoder)>;

/// This peer's decoder, created on its first frame.
fn decoder_for<'a>(
    decoders: &'a mut Decoders,
    peer: &str,
) -> Option<&'a mut media_service::voice::VoiceDecoder> {
    let seated = decoders.iter().position(|(id, _)| id == peer);
    let index = match seated {
        Some(index) => index,
        None => {
            let decoder = media_service::voice::VoiceDecoder::new().ok()?;
            if decoders.len() == MAX_DECODERS {
                decoders.remove(0);
            }
            decoders.push((peer.to_owned(), decoder));
            decoders.len() - 1
        }
    };
    Some(&mut decoders[index].1)
}

/// Decode this tick's frames, each with the decoder belonging to the peer
/// that sent it, and mix them into one playout frame.
///
/// A frame the codec refuses costs that peer this tick and nothing else:
/// every byte here was chosen by a remote participant, and one bad packet
/// must not silence the room or take the session down.
fn mix_playout(decoders: &mut Decoders, frames: Vec<PeerFrame>) -> Vec<i16> {
    let mut mixed = [0i32; crate::call::FRAME_SAMPLES];
    for PeerFrame { peer, frame } in frames {
        let Some(decoder) = decoder_for(decoders, &peer) else {
            continue;
        };
        let Ok(samples) = decoder.decode(&frame) else {
            continue;
        };
        for (mix, sample) in mixed.iter_mut().zip(samples) {
            *mix += i32::from(sample);
        }
    }
    mixed
        .into_iter()
        .map(|sample| sample.clamp(i16::MIN.into(), i16::MAX.into()) as i16)
        .collect()
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

struct Video {
    id: u64,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Video {
    fn drop(&mut self) {
        self.pump.abort();
        self.shutdown.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        crate::video::use_source(crate::video::Source::Off);
    }
}

struct Image {
    key: String,
    alive: Arc<AtomicBool>,
    decoding: Arc<AtomicBool>,
}

impl Drop for Image {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        crate::video::forget_peer(&self.key);
    }
}

#[derive(Default)]
pub(crate) struct Devices {
    audio: Option<Audio>,
    video: Option<Video>,
    images: BTreeMap<u64, Image>,
    // Released after the resource fields: a successor cannot acquire the
    // device before the former owner has closed its streams and images.
    lease: Option<Lease>,
}

impl Devices {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn acquire(&mut self) -> Result<(), String> {
        if self.lease.is_some() {
            return Ok(());
        }
        DEVICE_OWNER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "media devices belong to another active session".to_owned())?;
        self.lease = Some(Lease);
        Ok(())
    }

    pub(crate) fn cancel(&mut self, id: u64) {
        if self.audio.as_ref().is_some_and(|audio| audio.id == id) {
            self.audio.take();
        }
        if self.video.as_ref().is_some_and(|video| video.id == id) {
            self.video.take();
        }
    }

    fn audio(&mut self, id: u64, replies: Arc<Replies>) -> Result<(), String> {
        self.acquire()?;
        if self.audio.is_some() {
            return Err("audio device is already open".into());
        }
        let muted = Arc::new(AtomicBool::new(false));
        let playout = Arc::new(Mutex::new(crate::call::PlayoutRing::default()));
        let (capture, mut frames) = tokio::sync::mpsc::channel(4);
        let mut device = crate::call::AudioThread::start(muted.clone(), capture, playout.clone());
        let ready = device.ready.take().expect("new audio device");
        let pump = runtime().spawn(async move {
            match ready.await {
                Ok(note) => replies.item(
                    id,
                    Ok(
                        serde_json::to_vec(&serde_json::json!({"ready": true, "note": note}))
                            .expect("audio state"),
                    ),
                    false,
                ),
                Err(_) => {
                    replies.item(id, Err("audio device thread failed".into()), true);
                    return;
                }
            }
            // The codec sits here, at the device: the guest is handed an
            // ENCODED frame and the host's verdict on whether it carried
            // sound, so PCM never reaches the guest, the node bridge or the
            // hub — a room's audio crosses nodes as ~80 bytes, not 1 920.
            let mut encoder = match media_service::voice::VoiceEncoder::new(VOICE_BITRATE) {
                Ok(encoder) => encoder,
                Err(error) => {
                    replies.item(id, Err(format!("voice encoder: {error}")), true);
                    return;
                }
            };
            while let Some(samples) = frames.recv().await {
                let Ok(frame) = <&[i16; crate::call::FRAME_SAMPLES]>::try_from(&samples[..]) else {
                    continue;
                };
                let sound = crate::call::carries_sound(&samples);
                let Ok(encoded) = encoder.encode(frame) else {
                    // one frame the codec refused, not a dead device
                    continue;
                };
                replies.item(
                    id,
                    Ok(
                        serde_json::to_vec(&serde_json::json!({"frame": encoded, "sound": sound}))
                            .expect("voice frame"),
                    ),
                    false,
                );
            }
            replies.item(id, Ok(Vec::new()), true);
        });
        self.audio = Some(Audio {
            id,
            _device: device,
            muted,
            playout,
            decoders: Vec::new(),
            pump,
        });
        Ok(())
    }

    fn video(
        &mut self,
        id: u64,
        source: &str,
        max_bytes: usize,
        replies: Arc<Replies>,
    ) -> Result<(), String> {
        let source = match source {
            "camera" => crate::video::Source::Camera,
            "screen" => crate::video::Source::Screen,
            _ => return Err("unknown capture source".into()),
        };
        if max_bytes == 0 {
            return Err("image byte budget must be positive".into());
        }
        self.acquire()?;
        self.video.take();
        crate::video::use_source(source);
        let (capture, mut frames) = tokio::sync::mpsc::channel(1);
        let (errors, mut failures) = tokio::sync::mpsc::channel(1);
        let (shutdown, closing) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("media-capture".into())
            .spawn(move || crate::video::capture_thread(capture, closing, errors, max_bytes))
            .map_err(|error| error.to_string())?;
        let pump = runtime().spawn(async move {
            loop {
                tokio::select! {
                    frame = frames.recv() => {
                        let Some(frame) = frame else { break; };
                        replies.item(id, Ok(serde_json::to_vec(&frame).expect("captured image")), false);
                    }
                    error = failures.recv() => {
                        let Some(error) = error else { break; };
                        replies.item(id, Err(error), false);
                    }
                }
            }
            replies.item(id, Ok(Vec::new()), true);
        });
        self.video = Some(Video {
            id,
            shutdown: Some(shutdown),
            thread: Some(thread),
            pump,
        });
        Ok(())
    }

    fn image(&mut self) -> Result<Vec<u8>, String> {
        self.acquire()?;
        static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let key = format!("image:{id}");
        self.images.insert(
            id,
            Image {
                key: key.clone(),
                alive: Arc::new(AtomicBool::new(true)),
                decoding: Arc::new(AtomicBool::new(false)),
            },
        );
        Ok(
            serde_json::to_vec(&serde_json::json!({"image": id, "key": key}))
                .expect("image handle"),
        )
    }

    fn put(&mut self, id: u64, jpeg: Vec<u8>) -> Result<Vec<u8>, String> {
        let image = self.images.get(&id).ok_or("unknown image resource")?;
        let busy = image.decoding.swap(true, Ordering::AcqRel);
        if busy {
            return Err("image decode is already pending".into());
        }
        let key = image.key.clone();
        let alive = image.alive.clone();
        let decoding = image.decoding.clone();
        runtime().spawn_blocking(move || {
            crate::video::store_image(key, jpeg, &alive);
            decoding.store(false, Ordering::Release);
        });
        Ok(Vec::new())
    }
}

pub(super) fn answer(guest: &mut Guest, operation: &str, id: u64, payload: &[u8]) {
    let Some(session) = guest.session.as_mut() else {
        guest.refuse(
            id,
            "media requires a user-started background session".into(),
        );
        return;
    };
    let result = request(
        &mut session.media,
        guest.replies.clone(),
        operation,
        id,
        payload,
    );
    match result {
        Ok(None) => {}
        Ok(Some(bytes)) => guest.reply(id, Ok(bytes)),
        Err(error) => guest.refuse(id, error),
    }
}

fn request(
    devices: &mut Devices,
    replies: Arc<Replies>,
    operation: &str,
    id: u64,
    payload: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    /// one playout tick: every peer that had a frame this tick, named, so
    /// each is decoded by its own decoder before the mix.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Play {
        audio: u64,
        frames: Vec<PeerFrame>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Mute {
        audio: u64,
        muted: bool,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Capture {
        source: String,
        max_bytes: usize,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Picture {
        image: u64,
        jpeg: Vec<u8>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DropImage {
        image: u64,
    }
    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
    let bytes = match operation {
        "audio" => {
            devices.audio(id, replies)?;
            return Ok(None);
        }
        "play" => {
            let ask: Play = decode(payload)?;
            let audio = devices
                .audio
                .as_mut()
                .filter(|audio| audio.id == ask.audio)
                .ok_or("unknown audio resource")?;
            if ask.frames.len() > MAX_DECODERS {
                return Err("playout carries at most one frame per seated peer".into());
            }
            let frame = mix_playout(&mut audio.decoders, ask.frames);
            audio
                .playout
                .lock()
                .expect("audio playout")
                .push_frame(&frame);
            Vec::new()
        }
        "mute" => {
            let ask: Mute = decode(payload)?;
            let audio = devices
                .audio
                .as_ref()
                .filter(|audio| audio.id == ask.audio)
                .ok_or("unknown audio resource")?;
            audio.muted.store(ask.muted, Ordering::Relaxed);
            Vec::new()
        }
        "video" => {
            let ask: Capture = decode(payload)?;
            devices.video(id, &ask.source, ask.max_bytes, replies)?;
            return Ok(None);
        }
        "image" => devices.image()?,
        "put" => {
            let ask: Picture = decode(payload)?;
            devices.put(ask.image, ask.jpeg)?
        }
        "drop" => {
            let ask: DropImage = decode(payload)?;
            devices
                .images
                .remove(&ask.image)
                .ok_or("unknown image resource")?;
            Vec::new()
        }
        _ => return Err("unknown media operation".into()),
    };
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(level: i16) -> Vec<u8> {
        let mut encoder =
            media_service::voice::VoiceEncoder::new(VOICE_BITRATE).expect("voice encoder");
        let mut pcm = [0i16; crate::call::FRAME_SAMPLES];
        for (index, sample) in pcm.iter_mut().enumerate() {
            // a tone, so the decoder has something to reconstruct
            *sample = if index % 48 < 24 { level } else { -level };
        }
        encoder.encode(&pcm).expect("one encoded frame")
    }

    /// The device decodes each peer with that peer's own decoder and mixes
    /// the result. A codec carries state across a stream, so mixing before
    /// decoding is not an option and the peer name is not decoration.
    #[test]
    fn playout_decodes_per_peer_and_mixes() {
        let mut decoders = Decoders::new();
        let one = mix_playout(
            &mut decoders,
            vec![PeerFrame {
                peer: "a".into(),
                frame: encoded(6_000),
            }],
        );
        assert_eq!(one.len(), crate::call::FRAME_SAMPLES);
        assert!(one.iter().any(|sample| *sample != 0), "a peer was heard");
        assert_eq!(decoders.len(), 1, "one decoder, kept for the next frame");

        let two = mix_playout(
            &mut decoders,
            vec![
                PeerFrame {
                    peer: "a".into(),
                    frame: encoded(6_000),
                },
                PeerFrame {
                    peer: "b".into(),
                    frame: encoded(6_000),
                },
            ],
        );
        assert_eq!(decoders.len(), 2);
        let loudest = |frame: &[i16]| frame.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            loudest(&two) > loudest(&one),
            "two talkers are louder than one: {} vs {}",
            loudest(&two),
            loudest(&one)
        );

        // silence is still a frame: the playout tick must not stall
        assert_eq!(
            mix_playout(&mut decoders, Vec::new()),
            vec![0i16; crate::call::FRAME_SAMPLES]
        );
    }

    /// These bytes come from a remote participant. A refused packet costs
    /// that peer one tick — never the room, never the session.
    #[test]
    fn a_refused_packet_costs_one_peer_one_tick() {
        let mut decoders = Decoders::new();
        let mixed = mix_playout(
            &mut decoders,
            vec![
                PeerFrame {
                    peer: "bad".into(),
                    frame: vec![0xff; 40],
                },
                PeerFrame {
                    peer: "good".into(),
                    frame: encoded(9_000),
                },
            ],
        );
        assert!(
            mixed.iter().any(|sample| *sample != 0),
            "the good peer was still heard"
        );
    }

    /// A call that churns through more speakers than it can seat evicts the
    /// oldest decoder rather than growing forever.
    #[test]
    fn decoders_are_bounded_by_the_seats() {
        let mut decoders = Decoders::new();
        for index in 0..MAX_DECODERS + 4 {
            assert!(decoder_for(&mut decoders, &format!("peer-{index}")).is_some());
        }
        assert_eq!(decoders.len(), MAX_DECODERS);
        assert_eq!(decoders[0].0, "peer-4", "the oldest went first");
    }

    /// The lease is what makes "one microphone, one call" true: it is the only
    /// thing standing between a second panel, a replaced view or a switched
    /// network and a second capture opened behind the first. Nothing here
    /// opens a device — `acquire` is the gate, and the image resource is the
    /// cheapest operation that takes it.
    ///
    /// `DEVICE_OWNER` is process-global on purpose (a device is), so this is
    /// the one test in the crate that takes it.
    #[test]
    fn one_session_holds_the_devices_and_dropping_it_releases_them() {
        let mut first = Devices::new();
        first
            .image()
            .expect("the first session takes the device lease");
        let mut second = Devices::new();
        let refused = second
            .image()
            .expect_err("a second session cannot open the same devices");
        assert!(
            refused.contains("another active session"),
            "refusal names the holder: {refused}"
        );
        first
            .image()
            .expect("the holder may open further resources on its own lease");

        drop(first);
        second
            .image()
            .expect("ending a session releases the devices for the next one");
    }
}
