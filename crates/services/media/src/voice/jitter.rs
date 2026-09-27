//! The receive-side jitter buffer: absorbs network jitter and reordering,
//! decides per 20 ms playout tick what the decoder should do.
//!
//! [`MinimalJitter`] is deliberately simple: fixed prefill that grows on
//! underrun (bounded), seq-driven, no time-stretching.

use std::collections::BTreeMap;

/// What the playout tick should do for one speaker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayoutStep {
    /// Still prefilling (or refilling after an underrun): play silence,
    /// there is no stream to render yet.
    Buffering,
    /// The expected frame arrived: decode it.
    Frame(Vec<u8>),
    /// The expected frame is missing. The buffer reports the gap regardless
    /// of whether later frames are already buffered — how to fill it is the
    /// caller's decision (silence today, since the codec offers no FEC/PLC;
    /// a concealment-capable codec would extrapolate here). The stream
    /// position still advances, so alignment is preserved across the gap.
    Gap,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub played: u64,
    /// Playout ticks where the expected frame was missing (rendered per the
    /// caller's concealment policy).
    pub gaps: u64,
    /// Late arrivals (slot already played) plus overflow evictions.
    pub late_dropped: u64,
    pub underruns: u64,
    /// Current prefill target, frames — grows on underrun up to the max.
    pub depth_frames: usize,
}

/// Hard cap on buffered frames — a stalled playout must not accumulate
/// unbounded audio (128 frames ≈ 2.5 s).
const MAX_BUFFERED: usize = 128;

pub struct MinimalJitter {
    /// Buffered frames keyed by unwrapped stream position.
    packets: BTreeMap<u64, Vec<u8>>,
    /// Highest (seq, position) seen — the wrap-unwrapping anchor.
    highest: Option<(u16, u64)>,
    /// Next position to play; `None` until the first fill completes.
    floor: Option<u64>,
    prefill: usize,
    max_depth: usize,
    filling: bool,
    stats: JitterStats,
}

impl MinimalJitter {
    pub fn new(prefill_frames: usize, max_depth_frames: usize) -> Self {
        let prefill = prefill_frames.max(1);
        MinimalJitter {
            packets: BTreeMap::new(),
            highest: None,
            floor: None,
            prefill,
            max_depth: max_depth_frames.max(prefill),
            filling: true,
            stats: JitterStats {
                depth_frames: prefill,
                ..JitterStats::default()
            },
        }
    }

    /// Unwrap a wire seq to a monotonically comparable stream position.
    fn unwrap_seq(&mut self, seq: u16) -> u64 {
        match self.highest {
            None => {
                // Anchor far from zero so early reordering can't underflow.
                let pos = u64::from(u16::MAX) + 1;
                self.highest = Some((seq, pos));
                pos
            }
            Some((high_seq, high_pos)) => {
                let delta = i64::from(seq.wrapping_sub(high_seq) as i16);
                let pos = high_pos.saturating_add_signed(delta);
                if delta > 0 {
                    self.highest = Some((seq, pos));
                }
                pos
            }
        }
    }

    pub fn insert(&mut self, seq: u16, payload: Vec<u8>) {
        let pos = self.unwrap_seq(seq);
        if let Some(floor) = self.floor
            && pos < floor
        {
            self.stats.late_dropped += 1;
            return;
        }
        self.packets.insert(pos, payload);
        if self.packets.len() > MAX_BUFFERED {
            self.packets.pop_first();
            self.stats.late_dropped += 1;
        }
    }

    /// Called exactly once per 20 ms playout tick.
    pub fn tick(&mut self) -> PlayoutStep {
        if self.filling {
            if self.packets.len() < self.prefill {
                return PlayoutStep::Buffering;
            }
            self.filling = false;
            // All buffered positions are >= any previous floor (late guard),
            // so resuming from the earliest buffered frame never replays.
            self.floor = self.packets.keys().next().copied();
        }
        let expected = self.floor.expect("floor set when not filling");
        self.floor = Some(expected + 1);
        if let Some(payload) = self.packets.remove(&expected) {
            self.stats.played += 1;
            return PlayoutStep::Frame(payload);
        }
        // Missing frame. If later frames are buffered we keep advancing past
        // the gap (alignment preserved); only a fully drained buffer is an
        // underrun that rebuilds a deeper cushion.
        if self.packets.is_empty() {
            self.stats.underruns += 1;
            self.prefill = (self.prefill + 1).min(self.max_depth);
            self.stats.depth_frames = self.prefill;
            self.filling = true;
        }
        self.stats.gaps += 1;
        PlayoutStep::Gap
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u16) -> Vec<u8> {
        vec![n as u8]
    }

    #[test]
    fn plays_in_order_after_prefill() {
        let mut jb = MinimalJitter::new(2, 6);
        jb.insert(0, frame(0));
        assert_eq!(jb.tick(), PlayoutStep::Buffering);
        jb.insert(1, frame(1));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(0)));
        jb.insert(2, frame(2));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(1)));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(2)));
        assert_eq!(jb.stats().played, 3);
    }

    #[test]
    fn reordered_arrival_is_absorbed() {
        let mut jb = MinimalJitter::new(2, 6);
        jb.insert(1, frame(1));
        jb.insert(0, frame(0));
        jb.insert(3, frame(3));
        jb.insert(2, frame(2));
        for n in 0..4 {
            assert_eq!(jb.tick(), PlayoutStep::Frame(frame(n)));
        }
    }

    #[test]
    fn isolated_loss_reports_gap_and_stays_aligned() {
        let mut jb = MinimalJitter::new(2, 6);
        jb.insert(0, frame(0));
        jb.insert(1, frame(1));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(0)));
        // seq 2 lost; 3 arrives. The gap is one tick, then 3 plays in slot.
        jb.insert(3, frame(3));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(1)));
        assert_eq!(jb.tick(), PlayoutStep::Gap);
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(3)));
        let stats = jb.stats();
        assert_eq!((stats.gaps, stats.played), (1, 3));
    }

    #[test]
    fn multi_frame_gap_reports_each_missing_tick() {
        let mut jb = MinimalJitter::new(1, 6);
        jb.insert(0, frame(0));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(0)));
        // 1 and 2 lost; 3 buffered — two gaps, then 3 plays aligned.
        jb.insert(3, frame(3));
        assert_eq!(jb.tick(), PlayoutStep::Gap);
        assert_eq!(jb.tick(), PlayoutStep::Gap);
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(3)));
    }

    #[test]
    fn underrun_grows_depth_and_refills() {
        let mut jb = MinimalJitter::new(1, 3);
        jb.insert(0, frame(0));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(0)));
        // Nothing buffered: underrun → gap, then deeper prefill.
        assert_eq!(jb.tick(), PlayoutStep::Gap);
        assert_eq!(jb.stats().underruns, 1);
        assert_eq!(jb.stats().depth_frames, 2);
        jb.insert(5, frame(5));
        assert_eq!(jb.tick(), PlayoutStep::Buffering);
        jb.insert(6, frame(6));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(5)));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(6)));
    }

    #[test]
    fn late_packet_is_dropped_not_replayed() {
        let mut jb = MinimalJitter::new(1, 6);
        jb.insert(0, frame(0));
        jb.insert(1, frame(1));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(0)));
        assert_eq!(jb.tick(), PlayoutStep::Frame(frame(1)));
        jb.insert(0, frame(0));
        assert_eq!(jb.stats().late_dropped, 1);
    }

    #[test]
    fn seq_wraparound_is_seamless() {
        let mut jb = MinimalJitter::new(1, 6);
        let mut expected = Vec::new();
        for i in 0..8u32 {
            let seq = 65_533u16.wrapping_add(i as u16);
            jb.insert(seq, frame(seq));
            expected.push(seq);
        }
        for seq in expected {
            assert_eq!(jb.tick(), PlayoutStep::Frame(frame(seq)));
        }
        assert_eq!(jb.stats().played, 8);
    }
}
