//! The video lane: frames cut into shards with FEC, and reassembled in order.
//!
//! **Sending.** A frame's data and metadata ([`codec::FRAME_META_LEN`] bytes, at the end)
//! are cut into `k` equal shards sized to the path MTU, plus `r` recovery shards
//! ([`fec`]). Each shard is one datagram. Recent frames stay in a short history so a NACK
//! can be answered.
//!
//! **Receiving.** Shards are copied into one buffer per frame. With any `k` of them the
//! frame is rebuilt (a plain truncation when no data shard is missing) and handed up, in
//! frame order. A frame that can't complete is given up when, after its last shard
//! arrived, either a later frame has started arriving and [`RxOpts::reorder_slack_us`] has
//! passed, or nothing more came for [`RxOpts::idle_timeout_us`]. If the RTT is short
//! enough, the receiver first NACKs the missing shards and waits one round trip. A frame
//! given up becomes a [`VideoEvent::Gap`]; the frames after it are still handed up, and the
//! decoder resumes at the first whose `refs_from_seq` allows.
//!
//! Chunk bodies: `Video` is `seq: u64, index: u16, k: u16, r: u16, blob_len: u32`, then the
//! shard. `Nack` is `seq: u64, n: u16, n × index: u16`; `n = 0` asks for the whole frame.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use quiver_stream_proto::{EncodedFrame, Micros};
use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

use crate::codec;
use crate::fec::{self, FecPlanner, MAX_FRAME_BYTES};
use crate::wire::{Malformed, Reader, Writer};

/// Bytes of `Video` chunk body in front of the shard.
pub const VIDEO_HEADER: usize = 18;

/// What the client gets from the video lane.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoEvent {
    /// The next frame, whole.
    Frame(EncodedFrame),
    /// Frames `since_seq..=through_seq` are lost for good. Send `Control::LostFrames` with
    /// the same range, and decode on from the next frame whose `refs_from_seq` allows it
    /// (or wait for a keyframe).
    Gap { since_seq: u64, through_seq: u64 },
}

// --- sending ---------------------------------------------------------------------------------------

/// A frame cut into shards: `k` data shards then `r` recovery shards, end to end.
#[derive(Debug)]
pub struct ShardedFrame {
    pub seq: u64,
    pub k: usize,
    pub r: usize,
    pub shard: usize,
    pub blob_len: usize,
    pub keyframe: bool,
    pub created: Micros,
    bytes: Vec<u8>,
}

impl ShardedFrame {
    pub fn total(&self) -> usize {
        self.k + self.r
    }

    pub fn wire_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Writes shard `i`'s `Video` chunk body.
    pub fn write_body(&self, i: usize, out: &mut Vec<u8>) {
        let mut w = Writer::new(out);
        w.u64(self.seq).u16(i as u16).u16(self.k as u16).u16(self.r as u16).u32(self.blob_len as u32);
        w.bytes(&self.bytes[i * self.shard..(i + 1) * self.shard]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTooLarge;

impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "video frame larger than {MAX_FRAME_BYTES} bytes")
    }
}

impl std::error::Error for FrameTooLarge {}

/// Not `Debug`: the codec state isn't.
pub struct VideoSender {
    pub planner: FecPlanner,
    encoder: Option<ReedSolomonEncoder>,
    history: VecDeque<Arc<ShardedFrame>>,
    /// Recovery bytes over all bytes, smoothed over frames.
    pub fec_ratio: f64,
    recovery_avg: f64,
    shards_avg: f64,
}

impl VideoSender {
    /// How long sent frames are kept for NACKs.
    const HISTORY_US: Micros = 200_000;

    pub fn new(planner: FecPlanner) -> Self {
        Self { planner, encoder: None, history: VecDeque::new(), fec_ratio: 0.0, recovery_avg: 0.0, shards_avg: 0.0 }
    }

    /// Cuts a frame into shards of at most `max_shard` bytes.
    pub fn shard(&mut self, frame: EncodedFrame, max_shard: usize, now: Micros) -> Result<Arc<ShardedFrame>, FrameTooLarge> {
        let blob_len = codec::frame_blob_len(&frame);
        if blob_len > MAX_FRAME_BYTES {
            return Err(FrameTooLarge);
        }
        let (k, shard) = fec::layout(blob_len, max_shard);
        let r = self.planner.parity_for(k);
        let (seq, keyframe) = (frame.seq, frame.keyframe);
        let mut bytes = frame.data;
        bytes.reserve((k + r) * shard - bytes.len());
        codec::put_frame_meta(&mut bytes, &EncodedFrame { data: Vec::new(), ..frame });
        bytes.resize(k * shard, 0);
        let parity = fec::encode(&mut self.encoder, &bytes, k, r, shard);
        bytes.extend_from_slice(&parity);
        // Weighted by size: what share of the bytes on the wire is recovery.
        self.recovery_avg = 0.9 * self.recovery_avg + 0.1 * r as f64;
        self.shards_avg = 0.9 * self.shards_avg + 0.1 * (k + r) as f64;
        self.fec_ratio = self.recovery_avg / self.shards_avg;
        let sharded = Arc::new(ShardedFrame { seq, k, r, shard, blob_len, keyframe, created: now, bytes });
        self.history.push_back(sharded.clone());
        while self.history.front().is_some_and(|f| f.created + Self::HISTORY_US < now) || self.history.len() > 64 {
            self.history.pop_front();
        }
        Ok(sharded)
    }

    /// The frame `seq`, if still in the history.
    pub fn find(&self, seq: u64) -> Option<Arc<ShardedFrame>> {
        self.history.iter().rev().find(|f| f.seq == seq).cloned()
    }
}

pub fn put_nack(w: &mut Writer<'_>, seq: u64, indices: &[u16]) {
    w.u64(seq).u16(indices.len() as u16);
    for i in indices {
        w.u16(*i);
    }
}

/// `(seq, indices)`; empty indices ask for the whole frame.
pub fn get_nack(body: &[u8]) -> Result<(u64, Vec<u16>), Malformed> {
    let mut r = Reader::new(body);
    let seq = r.u64()?;
    let n = r.u16()?;
    let idx = (0..n).map(|_| r.u16()).collect::<Result<_, _>>()?;
    Ok((seq, idx))
}

// --- receiving -------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RxOpts {
    /// After a later frame starts arriving, how long to wait for this one's stragglers.
    pub reorder_slack_us: Micros,
    /// How long after a frame's last shard to give up when nothing later arrives.
    pub idle_timeout_us: Micros,
    /// The most a frame is waited for from its first shard, however slowly it comes.
    pub max_wait_us: Micros,
}

impl Default for RxOpts {
    fn default() -> Self {
        Self { reorder_slack_us: 1_500, idle_timeout_us: 20_000, max_wait_us: 500_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    k: usize,
    r: usize,
    shard: usize,
    blob_len: usize,
}

#[derive(Debug)]
struct Asm {
    layout: Option<Layout>,
    buf: Vec<u8>,
    have: Vec<bool>,
    count: usize,
    created: Micros,
    last_rx: Micros,
    overtaken: Option<Micros>,
    nacked: Option<Micros>,
    done: Option<EncodedFrame>,
    lost: bool,
}

impl Asm {
    fn placeholder(now: Micros) -> Self {
        Self {
            layout: None,
            buf: Vec::new(),
            have: Vec::new(),
            count: 0,
            created: now,
            last_rx: now,
            overtaken: Some(now),
            nacked: None,
            done: None,
            lost: false,
        }
    }

    fn pending(&self) -> bool {
        self.done.is_none() && !self.lost
    }

    /// When this frame should be NACKed or given up, whichever comes first.
    fn trigger(&self, o: &RxOpts) -> Micros {
        let mut t = (self.last_rx + o.idle_timeout_us).min(self.created + o.max_wait_us);
        if let Some(ov) = self.overtaken {
            t = t.min(ov.max(self.last_rx) + o.reorder_slack_us);
        }
        t
    }

    fn missing(&self, extra: usize) -> Vec<u16> {
        let Some(l) = self.layout else { return Vec::new() };
        let need = (l.k.saturating_sub(self.count) + extra).min(256);
        (0..l.k + l.r).filter(|&i| !self.have[i]).take(need).map(|i| i as u16).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoRxStats {
    pub frames: u64,
    pub frames_lost: u64,
    pub shards: u64,
    /// Data shards rebuilt from recovery shards.
    pub recovered: u64,
    /// Shards of a frame already given up: the reorder slack was too short for them.
    pub late: u64,
    /// Shards of a frame already handed up (recovery it didn't need).
    pub surplus: u64,
    pub duplicates: u64,
    pub nacks: u64,
    /// Frames that completed only after a NACK.
    pub nack_saves: u64,
}

#[derive(Default)]
pub struct VideoReceiver {
    pub opts: RxOpts,
    next: Option<u64>,
    highest: Option<u64>,
    frames: BTreeMap<u64, Asm>,
    /// Frames recently given up, to tell late shards from surplus ones.
    recent_lost: VecDeque<u64>,
    decoder: Option<ReedSolomonDecoder>,
    events: VecDeque<VideoEvent>,
    nacks: Vec<(u64, Vec<u16>)>,
    /// `(encoded on the host's clock, completed on ours)` for each frame finished.
    pub completions: Vec<(Micros, Micros)>,
    pub stats: VideoRxStats,
}

/// A jump in frame numbers bigger than this restarts the lane instead of making
/// placeholders for every frame in between.
const MAX_JUMP: u64 = 1024;

impl VideoReceiver {
    pub fn new(opts: RxOpts) -> Self {
        Self { opts, ..Default::default() }
    }

    /// Takes a `Video` chunk body.
    pub fn on_shard(&mut self, body: &[u8], now: Micros) -> Result<(), Malformed> {
        let mut r = Reader::new(body);
        let seq = r.u64()?;
        let index = r.u16()? as usize;
        let layout = Layout { k: r.u16()? as usize, r: r.u16()? as usize, blob_len: r.u32()? as usize, shard: r.remaining() };
        let data = r.rest();
        if !valid(layout, index) {
            return Err(Malformed);
        }
        self.stats.shards += 1;
        let next = *self.next.get_or_insert(seq);
        if seq < next {
            if self.recent_lost.contains(&seq) {
                self.stats.late += 1;
            } else {
                self.stats.surplus += 1;
            }
            return Ok(());
        }
        self.note_arrival(seq, now);
        let asm = self.frames.entry(seq).or_insert_with(|| Asm::placeholder(now));
        if asm.layout.is_none() {
            asm.layout = Some(layout);
            asm.buf = vec![0; (layout.k + layout.r) * layout.shard];
            asm.have = vec![false; layout.k + layout.r];
            if self.highest == Some(seq) {
                asm.overtaken = None;
            }
        }
        if asm.layout != Some(layout) {
            return Err(Malformed);
        }
        asm.last_rx = now;
        if !asm.pending() || asm.have[index] {
            self.stats.duplicates += 1;
            return Ok(());
        }
        asm.have[index] = true;
        asm.count += 1;
        asm.buf[index * layout.shard..(index + 1) * layout.shard].copy_from_slice(data);
        if asm.count >= layout.k {
            self.complete(seq, now);
        }
        self.deliver();
        Ok(())
    }

    /// Notes that frame `seq` is arriving: every earlier frame still open is now overtaken,
    /// and frames skipped entirely get placeholders.
    fn note_arrival(&mut self, seq: u64, now: Micros) {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            return;
        };
        if seq <= highest {
            return;
        }
        if seq - highest > MAX_JUMP {
            self.restart_at(seq);
            self.highest = Some(seq);
            return;
        }
        for asm in self.frames.values_mut() {
            if asm.pending() && asm.overtaken.is_none() {
                asm.overtaken = Some(now);
            }
        }
        for s in highest + 1..seq {
            self.frames.entry(s).or_insert_with(|| Asm::placeholder(now));
        }
        self.highest = Some(seq);
    }

    /// Gives up everything open, hands up what is done, and goes on from `seq`.
    fn restart_at(&mut self, seq: u64) {
        for asm in self.frames.values_mut() {
            if asm.pending() {
                asm.lost = true;
            }
        }
        self.deliver();
        let next = self.next.unwrap_or(seq);
        if next < seq {
            self.events.push_back(VideoEvent::Gap { since_seq: next, through_seq: seq - 1 });
            self.stats.frames_lost += seq - next;
        }
        self.frames.clear();
        self.next = Some(seq);
    }

    fn complete(&mut self, seq: u64, now: Micros) {
        let asm = self.frames.get_mut(&seq).expect("frame exists");
        let l = asm.layout.expect("has shards");
        match fec::recover(&mut self.decoder, &mut asm.buf, &asm.have, l.k, l.r, l.shard) {
            Ok(n) => self.stats.recovered += n as u64,
            Err(e) => {
                tracing::warn!(seq, "FEC decode failed: {e}");
                asm.lost = true;
                return;
            }
        }
        let mut blob = std::mem::take(&mut asm.buf);
        blob.truncate(l.blob_len);
        asm.have = Vec::new();
        match codec::frame_from_blob(seq, blob) {
            Ok(frame) => {
                if asm.nacked.is_some() {
                    self.stats.nack_saves += 1;
                }
                self.completions.push((frame.encoded, now));
                asm.done = Some(frame);
            }
            Err(_) => asm.lost = true,
        }
    }

    /// Hands up every frame that is ready in order, and gaps for those lost.
    fn deliver(&mut self) {
        let Some(mut next) = self.next else { return };
        let mut gap: Option<(u64, u64)> = None;
        while let Some(asm) = self.frames.get(&next) {
            if asm.pending() {
                break;
            }
            let asm = self.frames.remove(&next).expect("present");
            match asm.done {
                Some(frame) => {
                    if let Some((a, b)) = gap.take() {
                        self.events.push_back(VideoEvent::Gap { since_seq: a, through_seq: b });
                    }
                    self.stats.frames += 1;
                    self.events.push_back(VideoEvent::Frame(frame));
                }
                None => {
                    self.stats.frames_lost += 1;
                    self.recent_lost.push_back(next);
                    if self.recent_lost.len() > 64 {
                        self.recent_lost.pop_front();
                    }
                    gap = Some(gap.map_or((next, next), |(a, _)| (a, next)));
                }
            }
            next += 1;
        }
        if let Some((a, b)) = gap {
            self.events.push_back(VideoEvent::Gap { since_seq: a, through_seq: b });
        }
        self.next = Some(next);
    }

    /// Runs the deadlines. `nack_wait` is how long a NACK's answer takes (about an RTT),
    /// or None when NACKs aren't worth it on this path.
    pub fn on_timeout(&mut self, now: Micros, nack_wait: Option<Micros>) {
        let opts = self.opts.clone();
        let mut changed = false;
        for (&seq, asm) in self.frames.iter_mut().filter(|(_, a)| a.pending()) {
            let trigger = asm.trigger(&opts);
            if now < trigger {
                continue;
            }
            match (asm.nacked, nack_wait) {
                (None, Some(_)) if now < asm.created + opts.max_wait_us => {
                    let missing = asm.missing(1);
                    if asm.layout.is_none() || !missing.is_empty() {
                        self.nacks.push((seq, missing));
                        self.stats.nacks += 1;
                    }
                    asm.nacked = Some(now);
                }
                (Some(t), Some(wait)) if now < t + wait => {}
                _ => {
                    asm.lost = true;
                    changed = true;
                }
            }
        }
        if changed {
            self.deliver();
        }
    }

    /// The next deadline, if any frame is open.
    pub fn next_timeout(&self, nack_wait: Option<Micros>) -> Option<Micros> {
        self.frames
            .values()
            .filter(|a| a.pending())
            .map(|a| match (a.nacked, nack_wait) {
                (Some(t), Some(w)) => t + w,
                _ => a.trigger(&self.opts),
            })
            .min()
    }

    pub fn poll_event(&mut self) -> Option<VideoEvent> {
        self.events.pop_front()
    }

    /// NACKs to send: `(seq, indices)`.
    pub fn take_nacks(&mut self) -> Vec<(u64, Vec<u16>)> {
        std::mem::take(&mut self.nacks)
    }
}

fn valid(l: Layout, index: usize) -> bool {
    l.k >= 1
        && index < l.k + l.r
        && l.shard >= 2
        && l.shard % 2 == 0
        && l.blob_len <= l.k * l.shard
        && l.blob_len > (l.k - 1) * l.shard
        && l.blob_len >= codec::FRAME_META_LEN
        && l.blob_len <= MAX_FRAME_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, len: usize) -> EncodedFrame {
        EncodedFrame {
            seq,
            captured: seq * 8_333,
            encoded: seq * 8_333 + 2_000,
            keyframe: seq == 0,
            refs_from_seq: 0,
            width: 2560,
            height: 1440,
            data: (0..len).map(|i| (i as u64 * 31 + seq) as u8).collect(),
        }
    }

    fn sender(loss: f64) -> VideoSender {
        let mut p = FecPlanner::new(1e-4, 0.5, 1);
        p.set_loss(loss);
        VideoSender::new(p)
    }

    fn bodies(f: &ShardedFrame) -> Vec<Vec<u8>> {
        (0..f.total())
            .map(|i| {
                let mut b = Vec::new();
                f.write_body(i, &mut b);
                b
            })
            .collect()
    }

    fn frames_of(rx: &mut VideoReceiver) -> Vec<VideoEvent> {
        std::iter::from_fn(|| rx.poll_event()).collect()
    }

    #[test]
    fn a_frame_survives_losing_its_recovery_count() {
        let mut tx = sender(0.05);
        let sharded = tx.shard(frame(0, 50_000), 1184, 0).unwrap();
        assert!(sharded.r >= 3);
        let mut rx = VideoReceiver::default();
        for (i, b) in bodies(&sharded).iter().enumerate() {
            if i % 13 != 0 || i / 13 >= sharded.r {
                rx.on_shard(b, 10).unwrap();
            }
        }
        assert_eq!(frames_of(&mut rx), vec![VideoEvent::Frame(frame(0, 50_000))]);
        assert!(rx.stats.recovered > 0);
    }

    #[test]
    fn frames_come_out_in_order_and_losses_become_gaps() {
        let mut tx = sender(0.0);
        let mut rx = VideoReceiver::default();
        let f: Vec<_> = (10..14).map(|s| tx.shard(frame(s, 3000), 1184, 0).unwrap()).collect();
        // Frame 11 loses two data shards with only one recovery shard.
        let b11 = bodies(&f[1]);
        for b in bodies(&f[0]) {
            rx.on_shard(&b, 0).unwrap();
        }
        rx.on_shard(&b11[2], 1).unwrap();
        for b in bodies(&f[2]).iter().chain(&bodies(&f[3])) {
            rx.on_shard(b, 2).unwrap();
        }
        let ev = frames_of(&mut rx);
        assert_eq!(ev.len(), 1, "12 and 13 wait behind 11");
        assert!(rx.next_timeout(None).unwrap() <= 2 + rx.opts.reorder_slack_us);
        rx.on_timeout(2 + rx.opts.reorder_slack_us, None);
        let ev = frames_of(&mut rx);
        assert_eq!(ev[0], VideoEvent::Gap { since_seq: 11, through_seq: 11 });
        assert!(matches!(&ev[1], VideoEvent::Frame(f) if f.seq == 12));
        assert!(matches!(&ev[2], VideoEvent::Frame(f) if f.seq == 13));
        // A straggler for the lost frame is dropped.
        rx.on_shard(&b11[0], 3).unwrap();
        assert_eq!(rx.stats.late, 1);
    }

    #[test]
    fn a_frame_lost_whole_is_noticed_from_the_next_one() {
        let mut tx = sender(0.0);
        let mut rx = VideoReceiver::default();
        let f: Vec<_> = (0..3).map(|s| tx.shard(frame(s, 100), 1184, 0).unwrap()).collect();
        rx.on_shard(&bodies(&f[0])[0], 0).unwrap();
        rx.on_shard(&bodies(&f[2])[0], 100).unwrap();
        rx.on_timeout(100 + rx.opts.reorder_slack_us, None);
        let ev = frames_of(&mut rx);
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[1], VideoEvent::Gap { since_seq: 1, through_seq: 1 });
    }

    #[test]
    fn a_nack_is_sent_and_answered_before_giving_up() {
        let mut tx = sender(0.0);
        let mut rx = VideoReceiver::default();
        let f0 = tx.shard(frame(0, 5000), 1184, 0).unwrap();
        let f1 = tx.shard(frame(1, 5000), 1184, 0).unwrap();
        let b0 = bodies(&f0);
        for b in &b0[2..] {
            rx.on_shard(b, 0).unwrap();
        }
        rx.on_shard(&bodies(&f1)[0], 10).unwrap();
        let slack = rx.opts.reorder_slack_us;
        rx.on_timeout(10 + slack, Some(2_000));
        let nacks = rx.take_nacks();
        assert_eq!(nacks.len(), 1);
        let (seq, idx) = &nacks[0];
        assert_eq!(*seq, 0);
        assert!(idx.contains(&0) || idx.contains(&1));
        // Not given up while the answer may still come.
        rx.on_timeout(10 + slack + 1_000, Some(2_000));
        assert!(frames_of(&mut rx).is_empty());
        let found = tx.find(0).unwrap();
        for i in idx {
            let mut b = Vec::new();
            found.write_body(*i as usize, &mut b);
            rx.on_shard(&b, 10 + slack + 1_500).unwrap();
        }
        let ev = frames_of(&mut rx);
        assert!(matches!(&ev[0], VideoEvent::Frame(f) if f.seq == 0));
        assert_eq!(rx.stats.nack_saves, 1);
    }

    #[test]
    fn nack_bodies_round_trip() {
        let mut b = Vec::new();
        put_nack(&mut Writer::new(&mut b), 7, &[1, 9]);
        assert_eq!(get_nack(&b).unwrap(), (7, vec![1, 9]));
    }

    #[test]
    fn bad_layouts_are_refused() {
        let mut rx = VideoReceiver::default();
        let mut b = Vec::new();
        Writer::new(&mut b).u64(0).u16(5).u16(2).u16(1).u32(100).bytes(&[0; 60]);
        assert!(rx.on_shard(&b, 0).is_err(), "index beyond k + r");
    }

    #[test]
    fn a_huge_jump_restarts_with_one_gap() {
        let mut tx = sender(0.0);
        let mut rx = VideoReceiver::default();
        rx.on_shard(&bodies(&tx.shard(frame(0, 10), 1184, 0).unwrap())[0], 0).unwrap();
        rx.on_shard(&bodies(&tx.shard(frame(5000, 10), 1184, 0).unwrap())[0], 1).unwrap();
        let ev = frames_of(&mut rx);
        assert_eq!(ev[1], VideoEvent::Gap { since_seq: 1, through_seq: 4999 });
        assert!(matches!(&ev[2], VideoEvent::Frame(f) if f.seq == 5000));
    }
}
