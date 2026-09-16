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
    pump: tokio::task::JoinHandle<()>,
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
            while let Some(samples) = frames.recv().await {
                replies.item(
                    id,
                    Ok(serde_json::to_vec(&serde_json::json!({"samples": samples}))
                        .expect("PCM samples")),
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
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Play {
        audio: u64,
        samples: Vec<i16>,
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
                .as_ref()
                .filter(|audio| audio.id == ask.audio)
                .ok_or("unknown audio resource")?;
            if ask.samples.len() != crate::call::FRAME_SAMPLES {
                return Err("playout requires one 960-sample PCM frame".into());
            }
            audio
                .playout
                .lock()
                .expect("audio playout")
                .push_frame(&ask.samples);
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
