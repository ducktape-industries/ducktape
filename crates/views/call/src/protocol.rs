use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// The largest audio payload this room moves: one encoded 20 ms frame, the
/// same ceiling `media_service::call_wire` checks. The guest never decodes
/// one — the host encodes at the microphone and decodes at the speaker — so
/// this is a bound, not a shape.
pub const MAX_AUDIO_PAYLOAD: usize = 1275;
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
    /// One encoded frame from this device's microphone, with the host's
    /// verdict on whether it carried sound. The energy that decides `sound`
    /// is measured on the samples, which live and die at the device: the
    /// guest owns what SPEAKING means to the room (hangover, beacons), never
    /// the audio itself.
    LocalAudio {
        frame: Vec<u8>,
        sound: bool,
    },
    LocalImage {
        timestamp_ms: u32,
        jpeg: Vec<u8>,
    },
    Peer {
        peer: String,
        beacon: Beacon,
    },
    Left(String),
    RemoteAudio {
        peer: String,
        frame: Vec<u8>,
    },
    RemoteImage {
        peer: String,
        jpeg: Vec<u8>,
    },
    Tick,
}

#[derive(Clone, Debug)]
pub enum Effect {
    SendText(serde_json::Value),
    SendBinary(Vec<u8>),
    SelfState(Beacon),
    Capture(String),
    Mute(bool),
    /// This tick's audio, one encoded frame per peer that had one. The host
    /// decodes each with that peer's decoder and mixes: a codec is stateful
    /// per stream, so mixing cannot happen before decoding and decoding
    /// cannot happen without knowing whose frame it is.
    Play(Vec<(String, Vec<u8>)>),
    Image {
        peer: String,
        jpeg: Vec<u8>,
    },
    DropImage(String),
}

#[derive(Default)]
pub struct Machine {
    pub props: Props,
    pub peers: BTreeMap<String, Beacon>,
    audio: BTreeMap<String, VecDeque<Vec<u8>>>,
    ticks: u64,
    last_sound: Option<u64>,
    speaking: bool,
}

impl Machine {
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Properties(props) => self.properties(props),
            Event::LocalAudio { frame, sound } => self.local_audio(frame, sound),
            Event::LocalImage { timestamp_ms, jpeg } => self.local_image(timestamp_ms, jpeg),
            Event::Peer { peer, beacon } => self.peer(peer, beacon),
            Event::Left(peer) => self.left(peer),
            Event::RemoteAudio { peer, frame } => self.remote_audio(peer, frame),
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

    fn local_audio(&mut self, frame: Vec<u8>, sound: bool) -> Vec<Effect> {
        let carries_one_frame = !frame.is_empty() && frame.len() <= MAX_AUDIO_PAYLOAD;
        let may_send = !self.props.muted && carries_one_frame;
        if !may_send {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if sound {
            self.last_sound = Some(self.ticks);
            if !self.speaking {
                self.speaking = true;
                effects.extend(self.beacon());
            }
        }
        let mut bytes = vec![1];
        bytes.extend(frame);
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

    fn remote_audio(&mut self, peer: String, frame: Vec<u8>) -> Vec<Effect> {
        let carries_one_frame = !frame.is_empty() && frame.len() <= MAX_AUDIO_PAYLOAD;
        let valid = self.peers.contains_key(&peer) && carries_one_frame;
        if !valid {
            return Vec::new();
        }
        let queue = self.audio.entry(peer).or_default();
        if queue.len() == JITTER_FRAMES {
            queue.pop_front();
        }
        queue.push_back(frame);
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
        // one frame per talking peer, in roster order; the host decodes each
        // with that peer's decoder and mixes. An empty list is the silence a
        // playout tick still has to hear about.
        let due: Vec<(String, Vec<u8>)> = self
            .audio
            .iter_mut()
            .filter_map(|(peer, queue)| Some((peer.clone(), queue.pop_front()?)))
            .collect();
        effects.push(Effect::Play(due));
        effects
    }
}

pub fn binary(bytes: &[u8]) -> Result<Event, String> {
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    match bytes.first() {
        Some(4) if bytes.len() > 41 && bytes.len() <= 41 + MAX_AUDIO_PAYLOAD => {
            Ok(Event::RemoteAudio {
                peer: hex(&bytes[9..41]),
                frame: bytes[41..].to_vec(),
            })
        }
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

    /// one encoded frame, as the host hands it over. The guest never reads
    /// inside one, so a marker byte stands in for a codec here.
    fn voice(marker: u8) -> Vec<u8> {
        vec![marker; 80]
    }

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
                .step(Event::LocalAudio {
                    frame: voice(7),
                    sound: true
                })
                .is_empty(),
            "a muted device sends nothing, however loud the room"
        );
        assert!(
            matches!(machine.step(Event::LocalImage { timestamp_ms: 7, jpeg: vec![9] }).as_slice(), [Effect::SendBinary(bytes)] if bytes == &[2,1,0,0,0,7,9])
        );
    }
    /// The view-framing stage of #2237's latency decomposition. Every event a
    /// live call turns on runs through `step`, so this is the whole cost the
    /// guest adds between a captured frame and the byte handed to the host.
    /// The payload is rebuilt each round, so the same construction is timed
    /// alone and subtracted: what is left is the step.
    ///
    /// Program output: the harness is run by hand and the numbers ARE the
    /// result.
    #[test]
    #[ignore = "measurement harness"]
    fn framing_cost_per_event() {
        const ROUNDS: u32 = 20_000;
        let mut machine = Machine::default();
        machine.step(Event::Properties(Props {
            channel: "room".into(),
            muted: false,
            source: "camera".into(),
        }));
        for index in 0..4u32 {
            machine.step(Event::Peer {
                peer: format!("{index:064x}"),
                beacon: Beacon {
                    camera_on: true,
                    ..Beacon::default()
                },
            });
        }
        let speaker = machine.peers.keys().next().expect("a seated peer").clone();
        let jpeg = vec![7u8; 16 * 1024];
        let sources: [(&str, &dyn Fn() -> Event); 5] = [
            ("local_audio", &|| Event::LocalAudio {
                frame: vec![7u8; 80],
                sound: true,
            }),
            ("local_image", &|| Event::LocalImage {
                timestamp_ms: 7,
                jpeg: vec![7u8; 16 * 1024],
            }),
            ("remote_audio", &|| Event::RemoteAudio {
                peer: format!("{:064x}", 0),
                frame: vec![9u8; 80],
            }),
            ("remote_image", &|| Event::RemoteImage {
                peer: format!("{:064x}", 0),
                jpeg: vec![7u8; 16 * 1024],
            }),
            ("tick", &|| Event::Tick),
        ];
        assert_eq!(speaker, format!("{:064x}", 0), "the peer the sources name");
        assert_eq!(jpeg.len(), 16 * 1024);
        println!("== view framing, {ROUNDS} rounds, 4 seated peers ==");
        println!("event\tstep_ns\tbuild_ns");
        for (name, build) in sources {
            let idle = std::time::Instant::now();
            for _ in 0..ROUNDS {
                std::hint::black_box(build());
            }
            let idle = idle.elapsed().as_nanos() / u128::from(ROUNDS);
            let busy = std::time::Instant::now();
            for _ in 0..ROUNDS {
                machine.step(build());
            }
            let busy = busy.elapsed().as_nanos() / u128::from(ROUNDS);
            println!("{name}\t{}\t{idle}", busy.saturating_sub(idle));
        }
    }

    #[test]
    fn sound_hangover_and_bounded_jitter_are_owned_by_the_guest() {
        let mut machine = Machine::default();
        machine.step(Event::LocalAudio {
            frame: voice(1),
            sound: true,
        });
        assert!(machine.speaking);
        // silence does not end the turn — the hangover does
        machine.step(Event::LocalAudio {
            frame: voice(1),
            sound: false,
        });
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
                    frame: voice(9),
                });
            }
            assert_eq!(machine.audio[peer].len(), JITTER_FRAMES);
        }
        // the tick hands the host one frame per talking peer, named, for it
        // to decode and mix — the guest moves audio it cannot read.
        assert!(
            matches!(machine.step(Event::Tick).as_slice(), [Effect::Play(due)]
                if due.len() == 2 && due[0].0 == "a" && due[0].1 == voice(9))
        );
        machine.step(Event::Left("a".into()));
        assert!(!machine.audio.contains_key("a"));
        assert!(
            matches!(machine.step(Event::Tick).as_slice(), [Effect::Play(due)] if due.len() == 1)
        );
    }

    /// A frame the host could not have produced is refused at the seam it
    /// arrives on, in both directions: these bytes reach the guest from an
    /// untrusted peer through the hub, and from its own host.
    #[test]
    fn an_audio_frame_outside_the_bound_is_dropped() {
        let mut machine = Machine::default();
        machine.step(Event::Peer {
            peer: "a".into(),
            beacon: Beacon::default(),
        });
        for frame in [Vec::new(), vec![3; MAX_AUDIO_PAYLOAD + 1]] {
            assert!(
                machine
                    .step(Event::LocalAudio {
                        frame: frame.clone(),
                        sound: true
                    })
                    .is_empty()
            );
            machine.step(Event::RemoteAudio {
                peer: "a".into(),
                frame,
            });
            assert!(!machine.audio.contains_key("a"));
        }
        assert!(!machine.speaking, "a dropped frame is not a spoken one");
    }
}
