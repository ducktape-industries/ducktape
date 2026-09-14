//! Call control datagrams — on a separate flow over `Service::Voice`, so
//! control keeps working in an audio-only build. One tiny tagged
//! frame per message: `[tag][fields…]`, all integers BE. no version byte:
//! the tag is the discriminant, and the frames are fixed shapes (flag-day
//! rule — no in-band version).

const TAG_KEYFRAME_REQUEST: u8 = 1;
const TAG_BEACON: u8 = 2;
const TAG_RATE_HINT: u8 = 3;

/// The sender-side bitrate ladder (kbps): receivers hint, the sender takes
/// the min across receivers.
pub const RATE_LADDER_KBPS: [u32; 4] = [1200, 800, 500, 300];

/// The next rung below `current` (saturates at the bottom).
pub fn step_down(current: u32) -> u32 {
    RATE_LADDER_KBPS
        .iter()
        .copied()
        .filter(|&r| r < current)
        .max()
        .unwrap_or(*RATE_LADDER_KBPS.last().expect("non-empty ladder"))
}

/// The next rung above `current` (saturates at the top).
pub fn step_up(current: u32) -> u32 {
    RATE_LADDER_KBPS
        .iter()
        .copied()
        .filter(|&r| r > current)
        .min()
        .unwrap_or(RATE_LADDER_KBPS[0])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallControl {
    /// the receiver lost a frame and needs a decoder sync point. Senders
    /// rate-limit honoring this to one keyframe per second.
    KeyframeRequest,
    /// 1 Hz presence + ephemeral state (drives tiles, NOT consensus). `sharing`
    /// marks the video lane as a screen share (vs the camera) so peers render it
    /// letterboxed + labelled. `speaking` is the sender's own voice gate: the
    /// hub mixes audio, so a receiver cannot tell whose sound it hears.
    Beacon {
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
    /// receiver loss report: send to me at no more than `max_kbps`.
    RateHint { max_kbps: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("control frame truncated")]
    Truncated,
    #[error("unknown control tag {0}")]
    UnknownTag(u8),
}

impl CallControl {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            CallControl::KeyframeRequest => vec![TAG_KEYFRAME_REQUEST],
            CallControl::Beacon {
                muted,
                camera_on,
                sharing,
                speaking,
            } => vec![
                TAG_BEACON,
                *muted as u8,
                *camera_on as u8,
                *sharing as u8,
                *speaking as u8,
            ],
            CallControl::RateHint { max_kbps } => {
                let mut frame = vec![TAG_RATE_HINT];
                frame.extend_from_slice(&max_kbps.to_be_bytes());
                frame
            }
        }
    }

    pub fn decode(frame: &[u8]) -> Result<CallControl, ControlError> {
        if frame.is_empty() {
            return Err(ControlError::Truncated);
        }
        match frame[0] {
            TAG_KEYFRAME_REQUEST => Ok(CallControl::KeyframeRequest),
            TAG_BEACON if frame.len() >= 5 => Ok(CallControl::Beacon {
                muted: frame[1] != 0,
                camera_on: frame[2] != 0,
                sharing: frame[3] != 0,
                speaking: frame[4] != 0,
            }),
            TAG_RATE_HINT if frame.len() >= 5 => Ok(CallControl::RateHint {
                max_kbps: u32::from_be_bytes(frame[1..5].try_into().expect("4 bytes")),
            }),
            TAG_BEACON | TAG_RATE_HINT => Err(ControlError::Truncated),
            other => Err(ControlError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_request_round_trips() {
        let frame = CallControl::KeyframeRequest.encode();
        assert_eq!(
            CallControl::decode(&frame).unwrap(),
            CallControl::KeyframeRequest
        );
    }

    #[test]
    fn beacon_round_trips() {
        for muted in [false, true] {
            for camera_on in [false, true] {
                for sharing in [false, true] {
                    for speaking in [false, true] {
                        let control = CallControl::Beacon {
                            muted,
                            camera_on,
                            sharing,
                            speaking,
                        };
                        let frame = control.encode();
                        assert_eq!(CallControl::decode(&frame).unwrap(), control);
                    }
                }
            }
        }
    }

    #[test]
    fn rate_hint_round_trips() {
        let control = CallControl::RateHint { max_kbps: 500 };
        let frame = control.encode();
        assert_eq!(CallControl::decode(&frame).unwrap(), control);
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(matches!(
            CallControl::decode(&[]),
            Err(ControlError::Truncated)
        ));
        assert!(matches!(
            CallControl::decode(&[TAG_BEACON, 1]),
            Err(ControlError::Truncated)
        ));
        // Shorter beacons (pre-share, pre-speaking wires) are retired: short =
        // truncated.
        assert!(matches!(
            CallControl::decode(&[TAG_BEACON, 1, 1]),
            Err(ControlError::Truncated)
        ));
        assert!(matches!(
            CallControl::decode(&[TAG_BEACON, 1, 1, 1]),
            Err(ControlError::Truncated)
        ));
        assert!(matches!(
            CallControl::decode(&[TAG_RATE_HINT, 0, 0, 0]),
            Err(ControlError::Truncated)
        ));
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        assert!(matches!(
            CallControl::decode(&[99]),
            Err(ControlError::UnknownTag(99))
        ));
    }

    #[test]
    fn ladder_steps_down_and_saturates() {
        assert_eq!(step_down(1200), 800);
        assert_eq!(step_down(800), 500);
        assert_eq!(step_down(500), 300);
        assert_eq!(step_down(300), 300); // already at the bottom
        assert_eq!(step_down(700), 500); // between rungs takes the next lower
    }

    #[test]
    fn ladder_steps_up_and_saturates() {
        assert_eq!(step_up(300), 500);
        assert_eq!(step_up(500), 800);
        assert_eq!(step_up(800), 1200);
        assert_eq!(step_up(1200), 1200); // already at the top
        assert_eq!(step_up(700), 800); // between rungs takes the next higher
    }
}
