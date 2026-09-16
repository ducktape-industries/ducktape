use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const SAMPLES: usize = 960;
const MAX_PEERS: usize = 32;
const JITTER_FRAMES: usize = 3;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Props {
    pub channel: String,
    pub muted: bool,
    pub source: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Beacon {
    pub muted: bool,
    pub camera_on: bool,
    pub sharing: bool,
    pub speaking: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    Properties(Props),
    LocalAudio(Vec<i16>),
    LocalImage { timestamp_ms: u32, jpeg: Vec<u8> },
    Peer { peer: String, beacon: Beacon },
    Left(String),
    RemoteAudio { peer: String, samples: Vec<i16> },
    RemoteImage { peer: String, jpeg: Vec<u8> },
    Tick,
}

#[derive(Clone, Debug)]
pub enum Effect {
    SendText(serde_json::Value),
    SendBinary(Vec<u8>),
    SelfState(Beacon),
    Capture(String),
    Mute(bool),
    Play(Vec<i16>),
    Image { peer: String, jpeg: Vec<u8> },
    DropImage(String),
}

#[derive(Default)]
pub struct Machine {
    pub props: Props,
    pub peers: BTreeMap<String, Beacon>,
    audio: BTreeMap<String, VecDeque<Vec<i16>>>,
    ticks: u64,
    last_sound: Option<u64>,
    speaking: bool,
}

impl Machine {
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Properties(props) => self.properties(props),
            Event::LocalAudio(samples) => self.local_audio(samples),
            Event::LocalImage { timestamp_ms, jpeg } => self.local_image(timestamp_ms, jpeg),
            Event::Peer { peer, beacon } => self.peer(peer, beacon),
            Event::Left(peer) => self.left(peer),
            Event::RemoteAudio { peer, samples } => self.remote_audio(peer, samples),
            Event::RemoteImage { peer, jpeg } => self.remote_image(peer, jpeg),
            Event::Tick => self.tick(),
        }
    }

    fn state(&self) -> Beacon {
        Beacon {
            muted: self.props.muted,
            camera_on: self.props.source == "camera",
            sharing: self.props.source == "screen",
            speaking: self.speaking,
        }
    }

    fn beacon(&self) -> Vec<Effect> {
        let state = self.state();
        vec![
            Effect::SendText(serde_json::json!({"type": "beacon", "muted": state.muted,
            "camera_on": state.camera_on, "sharing": state.sharing, "speaking": state.speaking})),
            Effect::SelfState(state),
        ]
    }

    fn properties(&mut self, props: Props) -> Vec<Effect> {
        let source_changed = self.props.source != props.source;
        self.props = props;
        if self.props.muted {
            self.speaking = false;
            self.last_sound = None;
        }
        let mut effects = vec![Effect::Mute(self.props.muted)];
        if source_changed {
            effects.push(Effect::Capture(self.props.source.clone()));
        }
        effects.extend(self.beacon());
        effects
    }

    fn local_audio(&mut self, samples: Vec<i16>) -> Vec<Effect> {
        let may_send = !self.props.muted && samples.len() == SAMPLES;
        if !may_send {
            return Vec::new();
        }
        let energy: f64 = samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        let sound = energy / SAMPLES as f64 >= 400.0 * 400.0;
        let mut effects = Vec::new();
        if sound {
            self.last_sound = Some(self.ticks);
            if !self.speaking {
                self.speaking = true;
                effects.extend(self.beacon());
            }
        }
        let mut bytes = vec![1];
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        effects.push(Effect::SendBinary(bytes));
        effects
    }

    fn local_image(&mut self, timestamp_ms: u32, jpeg: Vec<u8>) -> Vec<Effect> {
        let capturing = matches!(self.props.source.as_str(), "camera" | "screen");
        if !capturing || jpeg.len() > 65_530 {
            return Vec::new();
        }
        let mut frame = vec![2, 1];
        frame.extend_from_slice(&timestamp_ms.to_be_bytes());
        frame.extend(jpeg);
        vec![Effect::SendBinary(frame)]
    }

    fn peer(&mut self, peer: String, beacon: Beacon) -> Vec<Effect> {
        let room_full = !self.peers.contains_key(&peer) && self.peers.len() >= MAX_PEERS;
        if room_full {
            return Vec::new();
        }
        self.peers.insert(peer.clone(), beacon.clone());
        let mut effects = Vec::new();
        if !beacon.camera_on && !beacon.sharing {
            effects.push(Effect::DropImage(peer.clone()));
        }
        effects
    }

    fn left(&mut self, peer: String) -> Vec<Effect> {
        self.peers.remove(&peer);
        self.audio.remove(&peer);
        vec![Effect::DropImage(peer)]
    }

    fn remote_audio(&mut self, peer: String, samples: Vec<i16>) -> Vec<Effect> {
        let valid = self.peers.contains_key(&peer) && samples.len() == SAMPLES;
        if !valid {
            return Vec::new();
        }
        let queue = self.audio.entry(peer).or_default();
        if queue.len() == JITTER_FRAMES {
            queue.pop_front();
        }
        queue.push_back(samples);
        Vec::new()
    }

    fn remote_image(&mut self, peer: String, jpeg: Vec<u8>) -> Vec<Effect> {
        let Some(beacon) = self.peers.get(&peer) else {
            return Vec::new();
        };
        let capturing = beacon.camera_on || beacon.sharing;
        if !capturing {
            return Vec::new();
        }
        vec![Effect::Image { peer, jpeg }]
    }

    fn tick(&mut self) -> Vec<Effect> {
        self.ticks += 1;
        let mut effects = Vec::new();
        let expired = self.speaking
            && self
                .last_sound
                .is_none_or(|last| self.ticks.saturating_sub(last) >= 20);
        if expired {
            self.speaking = false;
            effects.extend(self.beacon());
        }
        let mut mixed = vec![0i32; SAMPLES];
        for queue in self.audio.values_mut() {
            if let Some(frame) = queue.pop_front() {
                for (mix, sample) in mixed.iter_mut().zip(frame) {
                    *mix += i32::from(sample);
                }
            }
        }
        effects.push(Effect::Play(
            mixed
                .into_iter()
                .map(|sample| sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect(),
        ));
        effects
    }
}

pub fn binary(bytes: &[u8]) -> Result<Event, String> {
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    match bytes.first() {
        Some(4) if bytes.len() == 41 + SAMPLES * 2 => Ok(Event::RemoteAudio {
            peer: hex(&bytes[9..41]),
            samples: bytes[41..]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect(),
        }),
        Some(3) if bytes.len() > 38 && bytes.len() <= 65_568 => Ok(Event::RemoteImage {
            peer: hex(&bytes[6..38]),
            jpeg: bytes[38..].to_vec(),
        }),
        _ => Err("invalid media frame".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mute_and_source_switch_are_guest_protocol_decisions() {
        let mut machine = Machine::default();
        let effects = machine.step(Event::Properties(Props {
            muted: true,
            source: "screen".into(),
            channel: "r".into(),
        }));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Capture(source) if source == "screen"))
        );
        assert!(
            machine
                .step(Event::LocalAudio(vec![1000; SAMPLES]))
                .is_empty()
        );
        assert!(
            matches!(machine.step(Event::LocalImage { timestamp_ms: 7, jpeg: vec![9] }).as_slice(), [Effect::SendBinary(bytes)] if bytes == &[2,1,0,0,0,7,9])
        );
    }
    #[test]
    fn sound_hangover_and_bounded_jitter_mix_are_owned_by_the_guest() {
        let mut machine = Machine::default();
        machine.step(Event::LocalAudio(vec![1000; SAMPLES]));
        assert!(machine.speaking);
        for _ in 0..20 {
            machine.step(Event::Tick);
        }
        assert!(!machine.speaking);
        for peer in ["a", "b"] {
            machine.step(Event::Peer {
                peer: peer.into(),
                beacon: Beacon::default(),
            });
            for _ in 0..20 {
                machine.step(Event::RemoteAudio {
                    peer: peer.into(),
                    samples: vec![20_000; SAMPLES],
                });
            }
            assert_eq!(machine.audio[peer].len(), JITTER_FRAMES);
        }
        assert!(
            matches!(machine.step(Event::Tick).as_slice(), [Effect::Play(samples)] if samples[0] == i16::MAX)
        );
        machine.step(Event::Left("a".into()));
        assert!(!machine.audio.contains_key("a"));
    }
}
