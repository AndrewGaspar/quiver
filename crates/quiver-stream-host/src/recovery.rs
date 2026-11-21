//! Recovering from lost frames without keyframe storms, and what each frame's
//! [`refs_from_seq`](quiver_stream_proto::EncodedFrame::refs_from_seq) says.
//!
//! The encoder is IPPP with one reference per frame, and its DPB keeps the last
//! [`DPB`](crate::nvenc::DPB) frames. When the client reports `LostFrames { since, through }`,
//! every frame from `since` to the last one encoded depends on what it lost, so all of those
//! still in the DPB are invalidated as references (`nvEncInvalidateRefFrames`). The next frame
//! then refers to the newest frame before `since`, which the client has. If no such frame is
//! left in the DPB, the next frame is a keyframe instead.
//!
//! **`refs_from_seq`** is the seq of the last *clean point*: the last keyframe, or the first
//! frame encoded after an invalidation. A frame doesn't depend on any frame from a lost
//! stretch that ended before its clean point: frames before the clean point that it still
//! (transitively) references are ones the client didn't report lost. So the client's rule is:
//! **after losing frames up to `b`, drop frames until one arrives with `refs_from_seq > b`,
//! then decode from it.** A keyframe always qualifies (its `refs_from_seq` is its own seq).
//!
//! Reports are often late or repeated (the client sends one per loss, and the transport may
//! duplicate it). One whose lost stretch ended before the current clean point changes
//! nothing; one that straddles it invalidates only from the clean point on.

use std::collections::VecDeque;

/// How the stream gets over loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Invalidate the lost frames as references; a keyframe only when nothing older is left.
    RefInvalidation,
    /// Periodic intra refresh waves, and a keyframe on a report. For encoders or decoders
    /// that don't do reference invalidation well.
    IntraRefresh,
    /// A keyframe for every report.
    Keyframe,
}

/// What a loss report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Already recovered from (a late or repeated report).
    Nothing,
    /// Invalidate these frames (by seq, the encoder's input timestamp).
    Invalidate(Vec<u64>),
    /// Nothing usable is left in the DPB: the next frame must be a keyframe.
    Keyframe,
}

/// The frames in the encoder's DPB and the current clean point.
#[derive(Debug, Clone)]
pub struct RefTracker {
    dpb: usize,
    /// Seqs of the frames in the DPB that are still valid references, oldest first.
    refs: VecDeque<u64>,
    /// The last keyframe or recovery frame.
    clean_from: u64,
    /// The next frame starts a new clean point (an invalidation happened).
    recovering: bool,
    last: Option<u64>,
}

impl RefTracker {
    pub fn new(dpb: usize) -> Self {
        RefTracker { dpb, refs: VecDeque::new(), clean_from: 0, recovering: false, last: None }
    }

    /// A frame was encoded. Returns its `refs_from_seq`.
    pub fn encoded(&mut self, seq: u64, keyframe: bool) -> u64 {
        if keyframe {
            self.refs.clear();
            self.clean_from = seq;
        } else if self.recovering {
            self.clean_from = seq;
        }
        self.recovering = false;
        self.refs.push_back(seq);
        // The DPB is a sliding window over the frames encoded, invalidated ones included
        // (assumed, to be safe: NVENC doesn't say whether an invalidated frame keeps its slot).
        let dpb = self.dpb as u64;
        self.refs.retain(|s| s + dpb > seq);
        self.last = Some(seq);
        self.clean_from
    }

    /// The client lost `since..=through`.
    pub fn lost(&mut self, since: u64, through: u64) -> Action {
        let Some(last) = self.last else { return Action::Keyframe };
        if through < self.clean_from || since > last {
            return Action::Nothing;
        }
        // Frames from the clean point on depend on it, so a loss that reaches back past it
        // spoils everything from there.
        let from = since.max(self.clean_from);
        if !self.refs.iter().any(|s| *s < from) {
            return Action::Keyframe;
        }
        let bad: Vec<u64> = self.refs.iter().copied().filter(|s| *s >= from).collect();
        self.refs.retain(|s| *s < from);
        self.recovering = true;
        Action::Invalidate(bad)
    }

    pub fn clean_from(&self) -> u64 {
        self.clean_from
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut RefTracker, from: u64, to: u64) -> Vec<u64> {
        (from..=to).map(|s| t.encoded(s, s == 0)).collect()
    }

    #[test]
    fn normal_frames_carry_the_last_keyframe() {
        let mut t = RefTracker::new(5);
        assert_eq!(run(&mut t, 0, 3), vec![0, 0, 0, 0]);
        assert_eq!(t.encoded(4, true), 4);
        assert_eq!(t.encoded(5, false), 4);
    }

    #[test]
    fn a_loss_invalidates_and_the_next_frame_is_a_clean_point() {
        let mut t = RefTracker::new(5);
        run(&mut t, 0, 10);
        // Lost 9; 10 was sent after it and depends on it.
        assert_eq!(t.lost(9, 9), Action::Invalidate(vec![9, 10]));
        assert_eq!(t.encoded(11, false), 11);
        assert_eq!(t.encoded(12, false), 11);
        // The client's rule: drop until refs_from_seq > 9. Frame 11 qualifies, 10 didn't (0).
        // A repeated report changes nothing.
        assert_eq!(t.lost(9, 9), Action::Nothing);
        assert_eq!(t.encoded(13, false), 11);
    }

    #[test]
    fn a_loss_older_than_the_dpb_needs_a_keyframe() {
        let mut t = RefTracker::new(5);
        run(&mut t, 0, 20);
        // The DPB holds 16..=20; losing 14 leaves no good reference.
        assert_eq!(t.lost(14, 15), Action::Keyframe);
        // So does losing everything in it.
        assert_eq!(t.lost(16, 20), Action::Keyframe);
        // Losing 17 leaves 16.
        assert_eq!(t.lost(17, 17), Action::Invalidate(vec![17, 18, 19, 20]));
    }

    #[test]
    fn a_report_straddling_the_clean_point_invalidates_from_it() {
        let mut t = RefTracker::new(5);
        run(&mut t, 0, 10);
        assert_eq!(t.lost(9, 9), Action::Invalidate(vec![9, 10]));
        assert_eq!(t.encoded(11, false), 11);
        assert_eq!(t.encoded(12, false), 11);
        // The DPB window is 8..=12; valid refs 8, 11, 12. The client lost 10..=11: 10 was
        // already dealt with, 11 (the clean point) is new, so 11 and 12 go and 8 is used.
        assert_eq!(t.lost(10, 11), Action::Invalidate(vec![11, 12]));
        assert_eq!(t.encoded(13, false), 13);
        // Once the window has moved past every good frame, the same loss needs a keyframe.
        for s in 14..=17 {
            t.encoded(s, false);
        }
        assert_eq!(t.lost(13, 13), Action::Keyframe);
    }

    #[test]
    fn two_reports_before_the_next_frame() {
        let mut t = RefTracker::new(5);
        run(&mut t, 0, 10);
        assert_eq!(t.lost(10, 10), Action::Invalidate(vec![10]));
        assert_eq!(t.lost(8, 8), Action::Invalidate(vec![8, 9]));
        assert_eq!(t.encoded(11, false), 11);
    }

    #[test]
    fn nothing_encoded_yet() {
        let mut t = RefTracker::new(5);
        assert_eq!(t.lost(0, 0), Action::Keyframe);
        t.encoded(0, true);
        assert_eq!(t.lost(5, 6), Action::Nothing);
    }
}
