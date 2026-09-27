//! Frame reassembly for one sender: fragments arrive unordered and lossy;
//! a frame completes when all fragments land. A NEWER frame starting while
//! one is in progress abandons the old one (any missing fragment drops the
//! whole frame — that is the contract); frames at-or-below the last emitted
//! frame_no are stale and ignored. `dropped_frames` feeds the keyframe-
//! request path.

use super::frame::{MAX_FRAGMENT_PAYLOAD, VideoHeader, frame_newer};

pub struct CompleteFrame {
    pub frame_no: u32,
    pub keyframe: bool,
    pub ts_ms: u32,
    pub data: Vec<u8>,
}

pub enum Assembly {
    /// fragment stored, frame not yet complete.
    Progress,
    /// stale or duplicate fragment — ignored.
    Stale,
    Complete(CompleteFrame),
}

#[derive(Default)]
pub struct Reassembler {
    current: Option<InProgress>,
    last_emitted: Option<u32>,
    dropped: u64,
}

struct InProgress {
    frame_no: u32,
    keyframe: bool,
    ts_ms: u32,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl Reassembler {
    pub fn insert(&mut self, header: VideoHeader, payload: &[u8]) -> Assembly {
        if payload.len() > MAX_FRAGMENT_PAYLOAD {
            return Assembly::Stale;
        }
        if let Some(last) = self.last_emitted
            && !frame_newer(header.frame_no, last)
        {
            return Assembly::Stale;
        }
        match &self.current {
            Some(current) if current.frame_no == header.frame_no => {}
            Some(current) if frame_newer(header.frame_no, current.frame_no) => {
                // a newer frame started before this one completed: the old
                // frame is dead (missing fragments never retransmit).
                self.dropped += 1;
                self.current = Some(InProgress::start(header));
            }
            Some(_) => return Assembly::Stale, // older than in-progress
            None => self.current = Some(InProgress::start(header)),
        }
        let current = self.current.as_mut().expect("just ensured");
        // a fragment disagreeing on the frame's shape poisons the frame —
        // drop it wholesale rather than assemble a chimera.
        if current.parts.len() != header.frag_count as usize || current.keyframe != header.keyframe
        {
            self.current = None;
            self.dropped += 1;
            return Assembly::Stale;
        }
        let slot = &mut current.parts[header.frag_index as usize];
        if slot.is_some() {
            return Assembly::Stale; // duplicate
        }
        *slot = Some(payload.to_vec());
        current.received += 1;
        if current.received < current.parts.len() {
            return Assembly::Progress;
        }
        let done = self.current.take().expect("complete frame");
        self.last_emitted = Some(done.frame_no);
        Assembly::Complete(CompleteFrame {
            frame_no: done.frame_no,
            keyframe: done.keyframe,
            ts_ms: done.ts_ms,
            data: done.parts.into_iter().flatten().flatten().collect(),
        })
    }

    /// frames abandoned incomplete since construction (drives keyframe requests).
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }
}

impl InProgress {
    fn start(header: VideoHeader) -> Self {
        InProgress {
            frame_no: header.frame_no,
            keyframe: header.keyframe,
            ts_ms: header.ts_ms,
            parts: vec![None; header.frag_count as usize],
            received: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(frame_no: u32, frag_index: u16, frag_count: u16, keyframe: bool) -> VideoHeader {
        VideoHeader {
            keyframe,
            frame_no,
            frag_index,
            frag_count,
            ts_ms: frame_no,
            // the reassembler is per (peer, epoch) lane state: its owner
            // rebuilds it on an epoch change, so one value throughout here.
            epoch: 1,
        }
    }

    fn expect_complete(assembly: Assembly) -> CompleteFrame {
        match assembly {
            Assembly::Complete(frame) => frame,
            Assembly::Progress => panic!("expected Complete, got Progress"),
            Assembly::Stale => panic!("expected Complete, got Stale"),
        }
    }

    #[test]
    fn out_of_order_fragments_complete() {
        let mut reassembler = Reassembler::default();
        assert!(matches!(
            reassembler.insert(header(1, 2, 3, true), b"c"),
            Assembly::Progress
        ));
        assert!(matches!(
            reassembler.insert(header(1, 0, 3, true), b"a"),
            Assembly::Progress
        ));
        let complete = expect_complete(reassembler.insert(header(1, 1, 3, true), b"b"));
        assert_eq!(complete.frame_no, 1);
        assert!(complete.keyframe);
        assert_eq!(complete.data, b"abc");
        assert_eq!(reassembler.dropped_frames(), 0);
    }

    #[test]
    fn single_fragment_frame_completes_immediately() {
        let mut reassembler = Reassembler::default();
        let complete = expect_complete(reassembler.insert(header(4, 0, 1, false), b"only"));
        assert_eq!(complete.frame_no, 4);
        assert_eq!(complete.data, b"only");
    }

    #[test]
    fn incomplete_frame_replaced_by_newer_counts_as_dropped() {
        let mut reassembler = Reassembler::default();
        // frame 1 starts but never completes (missing fragment 1 of 2).
        assert!(matches!(
            reassembler.insert(header(1, 0, 2, false), b"x"),
            Assembly::Progress
        ));
        // frame 2 arrives before frame 1 completes: frame 1 is abandoned.
        let complete = expect_complete(reassembler.insert(header(2, 0, 1, false), b"y"));
        assert_eq!(complete.frame_no, 2);
        assert_eq!(reassembler.dropped_frames(), 1);

        // the abandoned frame 1's remaining fragment must not resurrect it,
        // or complete out of order.
        assert!(matches!(
            reassembler.insert(header(1, 1, 2, false), b"z"),
            Assembly::Stale
        ));
        assert_eq!(reassembler.dropped_frames(), 1);
    }

    #[test]
    fn stale_older_or_duplicate_fragments_are_ignored() {
        let mut reassembler = Reassembler::default();
        let complete = expect_complete(reassembler.insert(header(5, 0, 1, false), b"a"));
        assert_eq!(complete.frame_no, 5);

        // an older frame_no after emission is stale.
        assert!(matches!(
            reassembler.insert(header(4, 0, 1, false), b"old"),
            Assembly::Stale
        ));
        // the same frame_no again (duplicate / at-or-below last emitted) is stale.
        assert!(matches!(
            reassembler.insert(header(5, 0, 1, false), b"dup"),
            Assembly::Stale
        ));

        // a duplicate fragment within an in-progress frame is also stale.
        assert!(matches!(
            reassembler.insert(header(6, 0, 2, false), b"first"),
            Assembly::Progress
        ));
        assert!(matches!(
            reassembler.insert(header(6, 0, 2, false), b"again"),
            Assembly::Stale
        ));
    }

    #[test]
    fn completed_frame_nos_are_monotonic() {
        let mut reassembler = Reassembler::default();
        let mut emitted = Vec::new();
        for frame_no in [1u32, 2, 3, 5, 4, 6] {
            match reassembler.insert(header(frame_no, 0, 1, false), b"x") {
                Assembly::Complete(frame) => emitted.push(frame.frame_no),
                Assembly::Progress | Assembly::Stale => {}
            }
        }
        let mut sorted = emitted.clone();
        sorted.sort_unstable();
        assert_eq!(emitted, sorted, "emitted frame_nos must be monotonic");
        // frame_no 4 arrived after 5 was already emitted, so it must have
        // been dropped as stale rather than emitted out of order.
        assert!(!emitted.contains(&4));
    }
}
