//! Native audio devices and the shell adapter for a deployed call session.
//! Room protocol, transport frames and speaking policy belong to the guest.

use futures::{StreamExt as _, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const FRAME_SAMPLES: usize = 960;

#[derive(Clone, Debug, Hash, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallEvent {
    pub kind: String,
    pub message: String,
    pub status: Option<String>,
    pub peers: Vec<CallPeer>,
    pub stage: String,
    pub tiles: Vec<String>,
    pub video_live: bool,
    pub muted: bool,
    pub camera_on: bool,
    pub sharing: bool,
    pub speaking: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallPeer {
    pub peer: String,
    pub image: String,
    pub muted: bool,
    pub camera_on: bool,
    pub sharing: bool,
    pub speaking: bool,
}

impl CallEvent {
    fn failed(kind: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: kind.into(),
            status: Some(message.clone()),
            message,
            ..Self::default()
        }
    }
}

fn controls() -> &'static Mutex<Option<tokio::sync::watch::Sender<Vec<u8>>>> {
    static CONTROL: OnceLock<Mutex<Option<tokio::sync::watch::Sender<Vec<u8>>>>> = OnceLock::new();
    CONTROL.get_or_init(Mutex::default)
}

fn control(field: &str, value: serde_json::Value) {
    let guard = controls().lock().expect("session controls");
    let Some(control) = guard.as_ref() else {
        return;
    };
    control.send_modify(|bytes| {
        let Ok(mut props) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return;
        };
        props[field] = value;
        *bytes = serde_json::to_vec(&props).expect("session properties");
    });
}

pub fn call_set_muted(muted: bool) -> bool {
    control("muted", muted.into());
    muted
}

pub(crate) fn set_video_source(source: &str) {
    control("source", source.into());
}

/// The shell's explicit join action selects its deployed companion view.
/// The same background runtime is available to arbitrary deployed views.
pub fn call_session(rpc: String, channel_id: String) -> BoxStream<'static, CallEvent> {
    let props = serde_json::to_vec(
        &serde_json::json!({"channel": channel_id, "muted": false, "source": "off"}),
    )
    .expect("call properties");
    match crate::module_view::background::start("call", props, &rpc) {
        Ok(session) => {
            *controls().lock().expect("session controls") = Some(session.input.clone());
            session
                .events
                .map(move |event| {
                    let event = match event {
                        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                            CallEvent::failed("error", format!("session output: {error}"))
                        }),
                        Err(refusal) => CallEvent::failed("error", refusal.sentence),
                    };
                    if event.kind == "self" {
                        session.input.send_if_modified(|bytes| {
                            let Ok(mut props) = serde_json::from_slice::<serde_json::Value>(bytes)
                            else {
                                return false;
                            };
                            let source = match (event.camera_on, event.sharing) {
                                (_, true) => "screen",
                                (true, false) => "camera",
                                (false, false) => "off",
                            };
                            let unchanged =
                                props["muted"] == event.muted && props["source"] == source;
                            if unchanged {
                                return false;
                            }
                            props["muted"] = event.muted.into();
                            props["source"] = source.into();
                            *bytes = serde_json::to_vec(&props).expect("session properties");
                            true
                        });
                    }
                    event
                })
                .boxed()
        }
        Err(error) => {
            futures::stream::once(async move { CallEvent::failed("refused", error) }).boxed()
        }
    }
}

static VOICE_HEARD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Device-level audibility threshold used by the audio smoke probe.
const AUDIBLE: i16 = 1000;

/// How many mixed frames with sound in them this process has received.
#[cfg(test)]
pub(crate) fn voice_frames_heard() -> u64 {
    VOICE_HEARD.load(Ordering::Relaxed)
}

/// The playout ring: mixed 20 ms frames in, device-rate samples out. Caps at
/// ~200 ms and drops oldest — late audio is dead audio.
#[derive(Default)]
pub(crate) struct PlayoutRing {
    samples: VecDeque<i16>,
}

const PLAYOUT_CAP: usize = FRAME_SAMPLES * 10;

impl PlayoutRing {
    pub(crate) fn push_frame(&mut self, frame: &[i16]) {
        if frame
            .iter()
            .any(|sample| sample.unsigned_abs() > AUDIBLE as u16)
        {
            VOICE_HEARD.fetch_add(1, Ordering::Relaxed);
        }
        self.samples.extend(frame);
        while self.samples.len() > PLAYOUT_CAP {
            self.samples.pop_front();
        }
    }

    fn drain_into(&mut self, out: &mut [i16]) {
        for slot in out.iter_mut() {
            *slot = self.samples.pop_front().unwrap_or(0);
        }
    }
}

/// Whether a captured frame carried sound rather than room noise: mean
/// square over the frame, against a fixed floor.
///
/// This is a MEASUREMENT ON SAMPLES, so it belongs with the microphone — the
/// samples do not leave this process. What the room does with the verdict
/// (hangover, beacons, who is shown as speaking) is the call guest's, and it
/// receives the verdict, never the audio.
pub(crate) fn carries_sound(frame: &[i16]) -> bool {
    if frame.is_empty() {
        return false;
    }
    let energy: f64 = frame.iter().map(|sample| f64::from(*sample).powi(2)).sum();
    energy / frame.len() as f64 >= SOUND_FLOOR * SOUND_FLOOR
}

/// The amplitude a frame's mean square must reach to count as speech.
const SOUND_FLOOR: f64 = 400.0;

/// Accumulates mono 48 kHz i16 samples into exact voice frames.
#[derive(Default)]
pub struct FrameAccumulator {
    buffer: Vec<i16>,
}

impl FrameAccumulator {
    pub fn push(&mut self, samples: impl IntoIterator<Item = i16>) -> Vec<Vec<i16>> {
        self.buffer.extend(samples);
        let mut frames = Vec::new();
        while self.buffer.len() >= FRAME_SAMPLES {
            frames.push(self.buffer.drain(..FRAME_SAMPLES).collect());
        }
        frames
    }
}

/// Fold an interleaved buffer to mono i16: channels average per sample tick.
pub fn interleaved_to_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels)
        .map(|tick| {
            let sum: i32 = tick.iter().map(|sample| i32::from(*sample)).sum();
            (sum / tick.len() as i32) as i16
        })
        .collect()
}

/// f32 sample to i16, clamped.
pub fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// A linear resampler from `from_rate` to 48 kHz mono.
// ponytail: linear interpolation, fine for voice; swap for a windowed-sinc
// resampler if capture quality ever matters more than simplicity.
pub struct Resampler {
    step: f64,
    phase: f64,
    last: i16,
}

impl Resampler {
    pub fn new(from_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate) / 48_000.0,
            phase: 0.0,
            last: 0,
        }
    }

    pub fn push(&mut self, input: &[i16]) -> Vec<i16> {
        if (self.step - 1.0).abs() < f64::EPSILON {
            if let Some(last) = input.last() {
                self.last = *last;
            }
            return input.to_vec();
        }
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        for sample in input {
            // Emit every 48 kHz tick that lands before this input sample.
            while self.phase < 1.0 {
                let mixed =
                    f64::from(self.last) * (1.0 - self.phase) + f64::from(*sample) * self.phase;
                output.push(mixed as i16);
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.last = *sample;
        }
        output
    }
}

/// The audio thread's owner: dropping it signals shutdown and joins.
pub(crate) struct AudioThread {
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What the audio layer wants the session surface to say: empty when both
    /// devices opened, otherwise a short "mic unavailable"-class note.
    pub(crate) ready: Option<tokio::sync::oneshot::Receiver<String>>,
}

impl AudioThread {
    pub(crate) fn start(
        muted: Arc<AtomicBool>,
        mic: tokio::sync::mpsc::Sender<Vec<i16>>,
        playout: Arc<Mutex<PlayoutRing>>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let (ready, answer) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("huddle-audio".into())
            .spawn(move || audio_thread(muted, mic, playout, shutdown_rx, ready))
            .ok();
        Self {
            shutdown: Some(shutdown_tx),
            thread,
            ready: Some(answer),
        }
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.shutdown.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn audio_thread(
    muted: Arc<AtomicBool>,
    mic: tokio::sync::mpsc::Sender<Vec<i16>>,
    playout: Arc<Mutex<PlayoutRing>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    ready: tokio::sync::oneshot::Sender<String>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let mut notes = Vec::new();

    // The pump's mic arm must stay pending — never closed — in listen-only
    // sessions: this keepalive holds the channel open even when no input
    // device exists, so a missing microphone degrades instead of ending the
    // session.
    let mic_keepalive = mic.clone();

    let input_stream = host.default_input_device().and_then(|device| {
        let mic = mic.clone();
        let config = device.default_input_config().ok()?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate().0;
        let mut accumulator = FrameAccumulator::default();
        let mut resampler = Resampler::new(rate);
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    if muted.load(Ordering::Relaxed) {
                        return;
                    }
                    let ints: Vec<i16> = data.iter().copied().map(f32_to_i16).collect();
                    let mono = interleaved_to_mono(&ints, channels);
                    for frame in accumulator.push(resampler.push(&mono)) {
                        let _ = mic.try_send(frame);
                    }
                },
                |_| {},
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    if muted.load(Ordering::Relaxed) {
                        return;
                    }
                    let mono = interleaved_to_mono(data, channels);
                    for frame in accumulator.push(resampler.push(&mono)) {
                        let _ = mic.try_send(frame);
                    }
                },
                |_| {},
                None,
            ),
            _ => return None,
        };
        let stream = stream.ok()?;
        stream.play().ok()?;
        Some(stream)
    });
    if input_stream.is_none() {
        notes.push("no microphone");
    }

    let output_stream = host.default_output_device().and_then(|device| {
        let config = device.default_output_config().ok()?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate().0;
        // The hub mixes at 48 kHz; a device at another rate gets the nearest
        // sample (playout quality follows the ponytail note on Resampler).
        let step = 48_000.0 / f64::from(rate);
        let ring = playout;
        let mut phase = 0.0_f64;
        let mut current = 0i16;
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config.into(),
                move |data: &mut [f32], _| {
                    let ticks = data.len() / channels.max(1);
                    let mut mono = vec![0i16; ((ticks as f64) * step).ceil() as usize];
                    ring.lock().expect("playout ring").drain_into(&mut mono);
                    let mut source = mono.into_iter();
                    for tick in data.chunks_exact_mut(channels.max(1)) {
                        phase += step;
                        while phase >= 1.0 {
                            current = source.next().unwrap_or(0);
                            phase -= 1.0;
                        }
                        let value = f32::from(current) / 32768.0;
                        for slot in tick {
                            *slot = value;
                        }
                    }
                },
                |_| {},
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config.into(),
                move |data: &mut [i16], _| {
                    let ticks = data.len() / channels.max(1);
                    let mut mono = vec![0i16; ((ticks as f64) * step).ceil() as usize];
                    ring.lock().expect("playout ring").drain_into(&mut mono);
                    let mut source = mono.into_iter();
                    for tick in data.chunks_exact_mut(channels.max(1)) {
                        phase += step;
                        while phase >= 1.0 {
                            current = source.next().unwrap_or(0);
                            phase -= 1.0;
                        }
                        for slot in tick {
                            *slot = current;
                        }
                    }
                },
                |_| {},
                None,
            ),
            _ => return None,
        };
        let stream = stream.ok()?;
        stream.play().ok()?;
        Some(stream)
    });
    if output_stream.is_none() {
        notes.push("no speaker");
    }

    let _ = ready.send(notes.join(" · "));

    // Park until the session drops the sender; the streams die with the frame.
    let _ = shutdown.recv();
    drop(mic_keepalive);
    drop(input_stream);
    drop(output_stream);
}

// ============================================================================
// state folds — the flat handlers' arms live here
// ============================================================================

/// This side's voice gate after a session event: a `self` event carries the
/// flip, a session start or end clears it, and every other event keeps it.
pub fn call_speaking_after(current: bool, event: &CallEvent) -> bool {
    match event.kind.as_str() {
        "self" => event.speaking,
        "connecting" | "closed" | "refused" | "error" => false,
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verdict the call guest is handed instead of the samples. Silence
    /// and a room's noise floor are not speech; an ordinary speaking level
    /// is. The guest cannot compute this — it never sees a sample — so the
    /// threshold has to hold here.
    #[test]
    fn sound_is_measured_at_the_microphone() {
        assert!(!carries_sound(&[]));
        assert!(!carries_sound(&[0; FRAME_SAMPLES]));
        assert!(!carries_sound(&[399; FRAME_SAMPLES]));
        assert!(carries_sound(&[400; FRAME_SAMPLES]));
        assert!(
            carries_sound(&[-8000; FRAME_SAMPLES]),
            "sign does not matter"
        );
        // It is mean square over the frame, so energy can arrive concentrated:
        // one full-scale click in an otherwise silent frame is over the floor.
        // That is the threshold this moved unchanged, not a new judgement.
        let mut quiet_with_a_click = [0i16; FRAME_SAMPLES];
        quiet_with_a_click[0] = i16::MAX;
        assert!(carries_sound(&quiet_with_a_click));
    }

    #[test]
    fn frames_accumulate_to_exact_voice_frames() {
        let mut accumulator = FrameAccumulator::default();
        assert!(accumulator.push(vec![1i16; FRAME_SAMPLES - 1]).is_empty());
        let frames = accumulator.push(vec![2i16; FRAME_SAMPLES + 1]);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_SAMPLES));
        // The tail stays buffered for the next callback.
        let frames = accumulator.push(vec![3i16; FRAME_SAMPLES]);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn interleaved_folds_to_mono_by_average() {
        assert_eq!(interleaved_to_mono(&[10, 20, 30, 50], 2), vec![15, 40]);
        assert_eq!(interleaved_to_mono(&[7, 8, 9], 1), vec![7, 8, 9]);
    }

    #[test]
    fn resampler_identity_and_ratio() {
        let mut same = Resampler::new(48_000);
        assert_eq!(same.push(&[1, 2, 3]), vec![1, 2, 3]);

        let mut up = Resampler::new(24_000);
        let out = up.push(&[100; 240]);
        // 24k → 48k roughly doubles the sample count.
        assert!((470..=490).contains(&out.len()), "got {}", out.len());

        let mut down = Resampler::new(96_000);
        let out = down.push(&[100; 960]);
        assert!((470..=490).contains(&out.len()), "got {}", out.len());
    }

    /// The gate opens on the first loud frame, ignores a breath inside the
    /// hangover, and closes once the hangover runs out — flips only.

    #[test]
    fn playout_ring_caps_and_zero_fills() {
        let mut ring = PlayoutRing::default();
        ring.push_frame(&[5i16; FRAME_SAMPLES * 12]);
        assert_eq!(ring.samples.len(), PLAYOUT_CAP);
        let mut out = [1i16; 4];
        let mut empty = PlayoutRing::default();
        empty.drain_into(&mut out);
        assert_eq!(out, [0i16; 4]);
    }
}
