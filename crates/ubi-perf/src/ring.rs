//! Shared-memory rings (SIGHT-HUD §3.8): fixed 128-byte slots, one writer process (any number
//! of its threads), any number of read-only readers, no locks.
//!
//! **Writer**: `i = fetch_add(reserve, 1)`; slot `i & (n-1)`; claim the slot by a CAS of
//! `commit` from a value below `i + 1` to `(i + 1) | BUSY`; release fence; the record's other
//! fifteen words; then `commit = i + 1` with release ordering. If the slot is busy (another
//! thread of this writer, a full lap behind, is still writing it) or already holds a later
//! lap, the record is dropped and counted in `dropped`, so two writers never write one slot
//! at once and a committed record is always one writer's bytes.
//!
//! **Reader**: own cursor; load `reserve` (acquire); per index, `commit == index + 1` means
//! committed: copy the words, acquire fence, re-check `commit` (a writer claiming it for a
//! later lap mid-copy changes it: discarded and counted as torn). A later lap's value means
//! overwritten (lost). Not claimed yet, or busy for this index, means not committed yet: the
//! reader stops there and retries next poll, and skips the slot as lost once it has stayed
//! that way for [`DEFAULT_STUCK_POLLS`] polls while later slots exist (a writer thread that
//! died mid-write, or a writer process restarted on the same ring).
//!
//! Every access to shared memory is an atomic, so the protocol is sound under the Rust memory
//! model as well as against another process.

use core::mem::offset_of;
use core::sync::atomic::{Ordering, fence};

use crate::error::Error;
use crate::layout::*;
use crate::shm::{Atomics, ShmRegion};

const OFF_MAGIC: usize = offset_of!(RingHeader, magic);
const OFF_VERSION: usize = offset_of!(RingHeader, version);
const OFF_HEADER_BYTES: usize = offset_of!(RingHeader, header_bytes);
const OFF_SLOT_BYTES: usize = offset_of!(RingHeader, slot_bytes);
const OFF_CLOCK: usize = offset_of!(RingHeader, clock);
const OFF_SLOT_COUNT: usize = offset_of!(RingHeader, slot_count);
const OFF_PRODUCER: usize = offset_of!(RingHeader, producer_kind);
const OFF_CLOCK_FREQ: usize = offset_of!(RingHeader, clock_freq);
const OFF_NONCE: usize = offset_of!(RingHeader, nonce);
const OFF_PID: usize = offset_of!(RingHeader, writer_pid);
const OFF_SCHEMA: usize = offset_of!(RingHeader, schema_rev);
const OFF_RESERVE: usize = offset_of!(RingHeader, reserve);
const OFF_HEARTBEAT: usize = offset_of!(RingHeader, heartbeat);
const OFF_DROPPED: usize = offset_of!(RingHeader, dropped);
const OFF_STATUS: usize = offset_of!(RingHeader, status);
const OFF_STATUS_SEQ: usize = OFF_STATUS + offset_of!(StatusBlock, seq);
const OFF_STATUS_LEN: usize = OFF_STATUS + offset_of!(StatusBlock, len);
const OFF_STATUS_FORMAT: usize = OFF_STATUS + offset_of!(StatusBlock, format);
const OFF_STATUS_DATA: usize = OFF_STATUS + offset_of!(StatusBlock, data);
const STATUS_WORDS: usize = RING_STATUS_DATA_BYTES / 8;
/// High bit of `commit`: the slot is being written for the index in the low bits.
pub const COMMIT_BUSY: u64 = 1 << 63;

/// Polls a slot may stay uncommitted, with later slots reserved, before a reader gives up on
/// it. At quiverd's 50 ms drain that is 200 ms, far longer than any 128-byte write.
pub const DEFAULT_STUCK_POLLS: u32 = 4;

/// Bytes of a ring file with `slot_count` slots.
pub const fn ring_bytes(slot_count: u32) -> usize {
    RING_HEADER_BYTES + slot_count as usize * SLOT_BYTES
}

/// What a ring's creator writes into its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingParams {
    pub slot_count: u32,
    /// [`CLOCK_QPC`] or [`CLOCK_MONO_RAW_NS`].
    pub clock: u16,
    pub clock_freq: u64,
    pub producer_kind: u32,
    pub nonce: [u8; 16],
    pub writer_pid: u32,
    pub schema_rev: u32,
}

impl RingParams {
    /// The Sight layer's ring: 4096 slots, CLOCK_MONOTONIC_RAW ns.
    pub fn layer(writer_pid: u32, nonce: [u8; 16]) -> RingParams {
        RingParams {
            slot_count: RING_DEFAULT_SLOTS,
            clock: CLOCK_MONO_RAW_NS,
            clock_freq: 1_000_000_000,
            producer_kind: PRODUCER_LAYER,
            nonce,
            writer_pid,
            schema_rev: 0,
        }
    }

    /// A mod's ring (created by the bridge): QPC ticks at `qpc_freq` (10 MHz under Wine).
    pub fn wine_mod(slot_count: u32, qpc_freq: u64, writer_pid: u32, nonce: [u8; 16]) -> RingParams {
        RingParams { slot_count, clock: CLOCK_QPC, clock_freq: qpc_freq, producer_kind: PRODUCER_MOD, nonce, writer_pid, schema_rev: 0 }
    }

    /// The largest power-of-two slot count whose ring fits in `bytes` (a `ring_req`), within
    /// [`RING_MIN_SLOTS`]..=[`RING_MAX_SLOTS`]; `None` if not even the minimum fits.
    pub fn slots_for_bytes(bytes: u64) -> Option<u32> {
        let room = bytes.checked_sub(RING_HEADER_BYTES as u64)? / SLOT_BYTES as u64;
        if room < RING_MIN_SLOTS as u64 {
            return None;
        }
        let n = 1u64 << (63 - room.leading_zeros());
        Some(n.min(RING_MAX_SLOTS as u64) as u32)
    }

    pub fn file_bytes(&self) -> usize {
        ring_bytes(self.slot_count)
    }

    fn check(&self) -> Result<(), Error> {
        if !self.slot_count.is_power_of_two() || !(RING_MIN_SLOTS..=RING_MAX_SLOTS).contains(&self.slot_count) {
            return Err(Error::Layout("slot_count"));
        }
        if self.clock != CLOCK_QPC && self.clock != CLOCK_MONO_RAW_NS {
            return Err(Error::Layout("clock"));
        }
        if self.clock_freq == 0 {
            return Err(Error::Layout("clock_freq"));
        }
        if self.producer_kind != PRODUCER_LAYER && self.producer_kind != PRODUCER_MOD {
            return Err(Error::Layout("producer_kind"));
        }
        Ok(())
    }
}

/// A ring header as validated when attaching. These fields are fixed at creation; the
/// reader and writer use this copy, never the live (untrusted) values, for bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingInfo {
    pub slot_count: u32,
    pub clock: u16,
    pub clock_freq: u64,
    pub producer_kind: u32,
    pub nonce: [u8; 16],
    pub writer_pid: u32,
}

/// Initializes `region` as an empty ring: zeroes it, writes the header, stores the magic last
/// (release), so a reader that sees the magic sees the rest.
pub fn init_ring<R: ShmRegion>(region: &R, p: &RingParams) -> Result<RingInfo, Error> {
    p.check()?;
    if !region.writable() {
        return Err(Error::ReadOnly);
    }
    let need = p.file_bytes();
    if region.len() < need {
        return Err(Error::TooSmall { need, have: region.len() });
    }
    region.u64_at(OFF_MAGIC).store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    for off in (8..need).step_by(8) {
        region.u64_at(off).store(0, Ordering::Relaxed);
    }
    region.u16_at(OFF_VERSION).store(RING_VERSION, Ordering::Relaxed);
    region.u16_at(OFF_HEADER_BYTES).store(RING_HEADER_BYTES as u16, Ordering::Relaxed);
    region.u16_at(OFF_SLOT_BYTES).store(SLOT_BYTES as u16, Ordering::Relaxed);
    region.u16_at(OFF_CLOCK).store(p.clock, Ordering::Relaxed);
    region.u32_at(OFF_SLOT_COUNT).store(p.slot_count, Ordering::Relaxed);
    region.u32_at(OFF_PRODUCER).store(p.producer_kind, Ordering::Relaxed);
    region.u64_at(OFF_CLOCK_FREQ).store(p.clock_freq, Ordering::Relaxed);
    region.u64_at(OFF_NONCE).store(u64::from_le_bytes(p.nonce[..8].try_into().unwrap()), Ordering::Relaxed);
    region.u64_at(OFF_NONCE + 8).store(u64::from_le_bytes(p.nonce[8..].try_into().unwrap()), Ordering::Relaxed);
    region.u32_at(OFF_PID).store(p.writer_pid, Ordering::Relaxed);
    region.u32_at(OFF_SCHEMA).store(p.schema_rev, Ordering::Relaxed);
    region.u64_at(OFF_MAGIC).store(RING_MAGIC, Ordering::Release);
    Ok(RingInfo {
        slot_count: p.slot_count,
        clock: p.clock,
        clock_freq: p.clock_freq,
        producer_kind: p.producer_kind,
        nonce: p.nonce,
        writer_pid: p.writer_pid,
    })
}

/// Validates a ring header in (untrusted) memory.
pub fn read_ring_header<R: ShmRegion>(region: &R) -> Result<RingInfo, Error> {
    if region.len() < RING_HEADER_BYTES {
        return Err(Error::TooSmall { need: RING_HEADER_BYTES, have: region.len() });
    }
    let magic = region.u64_at(OFF_MAGIC).load(Ordering::Acquire);
    if magic == 0 {
        return Err(Error::NotReady);
    }
    if magic != RING_MAGIC {
        return Err(Error::BadMagic(magic));
    }
    let version = region.u16_at(OFF_VERSION).load(Ordering::Relaxed);
    if version != RING_VERSION {
        return Err(Error::Version(version));
    }
    if region.u16_at(OFF_HEADER_BYTES).load(Ordering::Relaxed) as usize != RING_HEADER_BYTES {
        return Err(Error::Layout("header_bytes"));
    }
    if region.u16_at(OFF_SLOT_BYTES).load(Ordering::Relaxed) as usize != SLOT_BYTES {
        return Err(Error::Layout("slot_bytes"));
    }
    let mut nonce = [0u8; 16];
    nonce[..8].copy_from_slice(&region.u64_at(OFF_NONCE).load(Ordering::Relaxed).to_le_bytes());
    nonce[8..].copy_from_slice(&region.u64_at(OFF_NONCE + 8).load(Ordering::Relaxed).to_le_bytes());
    let p = RingParams {
        slot_count: region.u32_at(OFF_SLOT_COUNT).load(Ordering::Relaxed),
        clock: region.u16_at(OFF_CLOCK).load(Ordering::Relaxed),
        clock_freq: region.u64_at(OFF_CLOCK_FREQ).load(Ordering::Relaxed),
        producer_kind: region.u32_at(OFF_PRODUCER).load(Ordering::Relaxed),
        nonce,
        writer_pid: region.u32_at(OFF_PID).load(Ordering::Relaxed),
        schema_rev: 0,
    };
    p.check()?;
    let need = p.file_bytes();
    if region.len() < need {
        return Err(Error::TooSmall { need, have: region.len() });
    }
    Ok(RingInfo {
        slot_count: p.slot_count,
        clock: p.clock,
        clock_freq: p.clock_freq,
        producer_kind: p.producer_kind,
        nonce: p.nonce,
        writer_pid: p.writer_pid,
    })
}

fn slot_off(info: &RingInfo, index: u64) -> usize {
    RING_HEADER_BYTES + (index & (info.slot_count as u64 - 1)) as usize * SLOT_BYTES
}

/// The writing side. `push` takes `&self`: share it between the writer process's threads.
pub struct RingWriter<R: ShmRegion> {
    region: R,
    info: RingInfo,
}

impl<R: ShmRegion> RingWriter<R> {
    /// Attaches to an initialized ring (one this process created, or one the bridge created
    /// for this mod). After a writer restart this continues at `reserve`; slots and a status
    /// block left mid-write by a dead writer are released. Attach only while no other writer
    /// is live on the ring.
    pub fn new(region: R) -> Result<Self, Error> {
        if !region.writable() {
            return Err(Error::ReadOnly);
        }
        let info = read_ring_header(&region)?;
        let seq = region.u64_at(OFF_STATUS_SEQ);
        let s = seq.load(Ordering::Relaxed);
        if s & 1 == 1 {
            seq.store(s + 1, Ordering::Release);
        }
        for k in 0..info.slot_count as u64 {
            let commit = region.u64_at(slot_off(&info, k));
            if commit.load(Ordering::Relaxed) & COMMIT_BUSY != 0 {
                commit.store(0, Ordering::Release);
            }
        }
        Ok(RingWriter { region, info })
    }

    /// Initializes `region` with `p` and attaches.
    pub fn create(region: R, p: &RingParams) -> Result<Self, Error> {
        let info = init_ring(&region, p)?;
        Ok(RingWriter { region, info })
    }

    pub fn info(&self) -> &RingInfo {
        &self.info
    }

    pub fn region(&self) -> &R {
        &self.region
    }

    /// Appends a record (its `commit` is ignored) and returns its index, or `None` if the
    /// slot was busy and the record was dropped (counted). Never blocks, allocates or makes a
    /// syscall.
    #[inline]
    pub fn push(&self, rec: &Record) -> Option<u64> {
        let i = self.region.u64_at(OFF_RESERVE).fetch_add(1, Ordering::Relaxed);
        if self.write_slot(i, rec) {
            Some(i)
        } else {
            self.add_dropped(1);
            None
        }
    }

    #[inline]
    fn write_slot(&self, i: u64, rec: &Record) -> bool {
        let base = slot_off(&self.info, i);
        let want = i.wrapping_add(1) & !COMMIT_BUSY;
        let commit = self.region.u64_at(base);
        let mut c = commit.load(Ordering::Relaxed);
        loop {
            if c & COMMIT_BUSY != 0 || c >= want {
                return false;
            }
            match commit.compare_exchange_weak(c, want | COMMIT_BUSY, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => c = now,
            }
        }
        fence(Ordering::Release);
        let words = rec.to_words();
        for (k, w) in words.iter().enumerate().skip(1) {
            self.region.u64_at(base + 8 * k).store(*w, Ordering::Relaxed);
        }
        commit.store(want, Ordering::Release);
        true
    }

    /// Reserves and claims a slot and never commits it: what a writer thread dying mid-write
    /// leaves behind.
    #[doc(hidden)]
    pub fn claim_without_commit_for_test(&self) -> u64 {
        let i = self.region.u64_at(OFF_RESERVE).fetch_add(1, Ordering::Relaxed);
        let base = slot_off(&self.info, i);
        self.region.u64_at(base).store((i + 1) | COMMIT_BUSY, Ordering::Relaxed);
        i
    }

    /// The writer's clock, at least once a second even when idle.
    pub fn heartbeat(&self, now: i64) {
        self.region.u64_at(OFF_HEARTBEAT).store(now as u64, Ordering::Release);
    }

    /// Counts records the writer chose not to write.
    pub fn add_dropped(&self, n: u64) {
        self.region.u64_at(OFF_DROPPED).fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_schema_rev(&self, rev: u32) {
        self.region.u32_at(OFF_SCHEMA).store(rev, Ordering::Release);
    }

    /// Replaces the status block (seqlock). Returns false, writing nothing, if another thread
    /// is updating it right now. `data` beyond 2032 bytes is cut.
    pub fn set_status(&self, format: u32, data: &[u8]) -> bool {
        let seq = self.region.u64_at(OFF_STATUS_SEQ);
        let s = seq.load(Ordering::Relaxed);
        if s & 1 == 1 || seq.compare_exchange(s, s + 1, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return false;
        }
        fence(Ordering::Release);
        let data = &data[..data.len().min(RING_STATUS_DATA_BYTES)];
        self.region.u32_at(OFF_STATUS_LEN).store(data.len() as u32, Ordering::Relaxed);
        self.region.u32_at(OFF_STATUS_FORMAT).store(format, Ordering::Relaxed);
        for (k, chunk) in data.chunks(8).enumerate() {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            self.region.u64_at(OFF_STATUS_DATA + 8 * k).store(u64::from_le_bytes(w), Ordering::Relaxed);
        }
        seq.store(s + 2, Ordering::Release);
        true
    }

    /// The layer's status block.
    pub fn set_layer_status(&self, s: &LayerStatus) -> bool {
        self.set_status(STATUS_FORMAT_LAYER, s.as_bytes())
    }
}

/// What one [`RingReader::poll`] saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// Records delivered.
    pub read: u64,
    /// Records overwritten before this reader got to them (overrun), or skipped as stuck.
    pub lost: u64,
    /// Records overwritten while being copied (discarded).
    pub torn: u64,
    /// Stopped at a reserved but uncommitted slot; the next poll retries it.
    pub pending: bool,
    /// `reserve` went backwards (the ring was re-initialized); the cursor was reset.
    pub reset: bool,
}

/// The reading side; quiverd's drain, the bridge in relay mode. Never writes.
pub struct RingReader<R: ShmRegion> {
    region: R,
    info: RingInfo,
    cursor: u64,
    stuck: Option<(u64, u32)>,
    stuck_polls: u32,
}

impl<R: ShmRegion> RingReader<R> {
    /// Attaches with the cursor at the oldest record still in the ring.
    pub fn new(region: R) -> Result<Self, Error> {
        let info = read_ring_header(&region)?;
        let mut r = RingReader { region, info, cursor: 0, stuck: None, stuck_polls: DEFAULT_STUCK_POLLS };
        r.seek_oldest();
        Ok(r)
    }

    pub fn info(&self) -> &RingInfo {
        &self.info
    }

    pub fn region(&self) -> &R {
        &self.region
    }

    pub fn set_stuck_polls(&mut self, polls: u32) {
        self.stuck_polls = polls.max(1);
    }

    /// The next index this reader will read.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Records reserved so far (untrusted).
    pub fn reserve(&self) -> u64 {
        self.region.u64_at(OFF_RESERVE).load(Ordering::Acquire)
    }

    /// Skip to the newest records only.
    pub fn seek_end(&mut self) {
        self.cursor = self.reserve();
        self.stuck = None;
    }

    pub fn seek_oldest(&mut self) {
        self.cursor = self.reserve().saturating_sub(self.info.slot_count as u64);
        self.stuck = None;
    }

    pub fn heartbeat(&self) -> i64 {
        self.region.u64_at(OFF_HEARTBEAT).load(Ordering::Acquire) as i64
    }

    pub fn dropped(&self) -> u64 {
        self.region.u64_at(OFF_DROPPED).load(Ordering::Relaxed)
    }

    pub fn schema_rev(&self) -> u32 {
        self.region.u32_at(OFF_SCHEMA).load(Ordering::Acquire)
    }

    /// Delivers up to `max` new records to `f(index, record)`. Records are copied out and
    /// sequence-checked but otherwise unvalidated: apply [`Record::check`].
    pub fn poll(&mut self, max: usize, mut f: impl FnMut(u64, &Record)) -> Poll {
        let n = self.info.slot_count as u64;
        let mut out = Poll::default();
        let reserve = self.reserve();
        if reserve < self.cursor {
            out.reset = true;
            self.cursor = reserve.saturating_sub(n);
            self.stuck = None;
        }
        if reserve - self.cursor > n {
            out.lost += reserve - n - self.cursor;
            self.cursor = reserve - n;
        }
        while self.cursor < reserve && (out.read as usize) < max {
            let idx = self.cursor;
            let base = slot_off(&self.info, idx);
            let commit = self.region.u64_at(base);
            let c1 = commit.load(Ordering::Acquire);
            let want = idx + 1;
            if c1 == want {
                let mut w = [0u64; 16];
                for (k, word) in w.iter_mut().enumerate().skip(1) {
                    *word = self.region.u64_at(base + 8 * k).load(Ordering::Relaxed);
                }
                fence(Ordering::Acquire);
                if commit.load(Ordering::Relaxed) != c1 {
                    out.torn += 1;
                } else {
                    w[0] = c1;
                    f(idx, &Record::from_words(w));
                    out.read += 1;
                }
                self.cursor += 1;
                self.stuck = None;
                continue;
            }
            let v = c1 & !COMMIT_BUSY;
            let pending = if c1 & COMMIT_BUSY != 0 {
                // Busy for this index: being written. Busy for another lap: this index's
                // record was dropped by its writer, or is being overwritten.
                v == want
            } else {
                v < want
            };
            if !pending {
                out.lost += 1;
                self.cursor += 1;
                self.stuck = None;
                continue;
            }
            let polls = match self.stuck {
                Some((i, k)) if i == idx => k + 1,
                _ => 1,
            };
            if polls > self.stuck_polls && reserve > want {
                out.lost += 1;
                self.cursor += 1;
                self.stuck = None;
                continue;
            }
            self.stuck = Some((idx, polls));
            out.pending = true;
            break;
        }
        out
    }

    /// Copies the status block (seqlock, a few retries). `None` if the writer kept it busy or
    /// never wrote one. Returns (format, valid length).
    pub fn status(&self, buf: &mut [u8; RING_STATUS_DATA_BYTES]) -> Option<(u32, usize)> {
        let seq = self.region.u64_at(OFF_STATUS_SEQ);
        for _ in 0..16 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let len = (self.region.u32_at(OFF_STATUS_LEN).load(Ordering::Relaxed) as usize).min(RING_STATUS_DATA_BYTES);
            let format = self.region.u32_at(OFF_STATUS_FORMAT).load(Ordering::Relaxed);
            for k in 0..STATUS_WORDS {
                let w = self.region.u64_at(OFF_STATUS_DATA + 8 * k).load(Ordering::Relaxed);
                buf[8 * k..8 * k + 8].copy_from_slice(&w.to_le_bytes());
            }
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == s1 {
                return (s1 != 0).then_some((format, len));
            }
        }
        None
    }

    /// The layer's status block, if the ring holds one.
    pub fn layer_status(&self) -> Option<LayerStatus> {
        let mut buf = [0u8; RING_STATUS_DATA_BYTES];
        match self.status(&mut buf)? {
            (STATUS_FORMAT_LAYER, len) => Some(LayerStatus::from_bytes(&buf[..len])),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::HeapRegion;
    use std::sync::Arc;

    fn ring(slots: u32) -> (RingWriter<Arc<HeapRegion>>, RingReader<Arc<HeapRegion>>) {
        let mem = Arc::new(HeapRegion::new(ring_bytes(slots)));
        let w = RingWriter::create(mem.clone(), &RingParams::layer(42, [7; 16])).map(|_| ()).err();
        assert!(w.is_some(), "layer params need 4096 slots");
        let p = RingParams { slot_count: slots, ..RingParams::layer(42, [7; 16]) };
        let w = RingWriter::create(mem.clone(), &p).unwrap();
        let r = RingReader::new(mem).unwrap();
        (w, r)
    }

    fn drain(r: &mut RingReader<Arc<HeapRegion>>) -> (Vec<Record>, Poll) {
        let mut v = Vec::new();
        let p = r.poll(usize::MAX, |_, rec| v.push(*rec));
        (v, p)
    }

    #[test]
    fn header_roundtrip_and_validation() {
        let mem = HeapRegion::new(ring_bytes(64));
        assert_eq!(read_ring_header(&mem), Err(Error::NotReady));
        let p = RingParams::wine_mod(64, 10_000_000, 1234, [9; 16]);
        let info = init_ring(&mem, &p).unwrap();
        assert_eq!(read_ring_header(&mem), Ok(info));
        assert_eq!(info.clock, CLOCK_QPC);
        // Too small a region for the declared slots.
        let small = HeapRegion::new(ring_bytes(32));
        assert!(matches!(init_ring(&small, &p), Err(Error::TooSmall { .. })));
        for bad in [0, 3, 8, 16384] {
            assert!(init_ring(&mem, &RingParams { slot_count: bad, ..p }).is_err());
        }
        // Corrupt layout fields are refused.
        mem.u16_at(OFF_SLOT_BYTES).store(64, Ordering::Relaxed);
        assert_eq!(read_ring_header(&mem), Err(Error::Layout("slot_bytes")));
        mem.u64_at(OFF_MAGIC).store(1, Ordering::Relaxed);
        assert_eq!(read_ring_header(&mem), Err(Error::BadMagic(1)));
    }

    #[test]
    fn slots_for_bytes() {
        assert_eq!(RingParams::slots_for_bytes(1 << 20), Some(4096));
        assert_eq!(RingParams::slots_for_bytes((1 << 20) + 4096), Some(8192));
        assert_eq!(RingParams::slots_for_bytes(64 << 20), Some(8192));
        assert_eq!(RingParams::slots_for_bytes(4096 + 16 * 128), Some(16));
        assert_eq!(RingParams::slots_for_bytes(4096 + 15 * 128), None);
        assert_eq!(RingParams::slots_for_bytes(10), None);
    }

    #[test]
    fn push_and_poll() {
        let (w, mut r) = ring(16);
        for i in 0..10 {
            w.push(&Record::series(1, i, i as f64));
        }
        let (v, p) = drain(&mut r);
        assert_eq!(p, Poll { read: 10, ..Poll::default() });
        assert_eq!(v.iter().map(|r| r.t).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
        assert_eq!(v[3].commit, 4);
        assert_eq!(drain(&mut r).1, Poll::default());
        // max bounds a poll.
        for i in 0..5 {
            w.push(&Record::series(1, i, 0.0));
        }
        assert_eq!(r.poll(2, |_, _| {}).read, 2);
        assert_eq!(r.poll(10, |_, _| {}).read, 3);
    }

    #[test]
    fn overrun_counts_lost() {
        let (w, mut r) = ring(16);
        for i in 0..50 {
            w.push(&Record::series(1, i, 0.0));
        }
        let (v, p) = drain(&mut r);
        assert_eq!(p.read, 16);
        assert_eq!(p.lost, 34);
        assert_eq!(v[0].t, 34);
        assert_eq!(r.cursor(), 50);
    }

    #[test]
    fn stuck_slot_is_skipped_after_polls() {
        let (w, mut r) = ring(16);
        w.push(&Record::series(1, 0, 0.0));
        w.claim_without_commit_for_test();
        let mut seen = vec![];
        let mut lost = 0;
        // Nothing after the stuck slot: wait however long it takes.
        for _ in 0..10 {
            let (v, p) = drain(&mut r);
            seen.extend(v.iter().map(|r| r.t));
            assert!(p.pending && p.lost == 0);
        }
        w.push(&Record::series(1, 2, 0.0));
        for _ in 0..=DEFAULT_STUCK_POLLS + 1 {
            let (v, p) = drain(&mut r);
            seen.extend(v.iter().map(|r| r.t));
            lost += p.lost;
        }
        assert_eq!(seen, vec![0, 2]);
        assert_eq!(lost, 1);
    }

    #[test]
    fn reset_when_reserve_goes_back() {
        let (w, mut r) = ring(16);
        for i in 0..5 {
            w.push(&Record::series(1, i, 0.0));
        }
        drain(&mut r);
        let p = RingParams { slot_count: 16, ..RingParams::layer(43, [1; 16]) };
        let w2 = RingWriter::create(w.region().clone(), &p).unwrap();
        w2.push(&Record::series(1, 100, 0.0));
        let (v, p) = drain(&mut r);
        assert!(p.reset);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].t, 100);
    }

    #[test]
    fn writer_restart_continues() {
        let (w, mut r) = ring(16);
        w.push(&Record::series(1, 0, 0.0));
        let region = w.region().clone();
        drop(w);
        let w = RingWriter::new(region).unwrap();
        w.push(&Record::series(1, 1, 0.0));
        let (v, p) = drain(&mut r);
        assert_eq!(p.read, 2);
        assert_eq!(v[1].commit, 2);
    }

    #[test]
    fn status_block() {
        let (w, r) = ring(16);
        let mut buf = [0u8; RING_STATUS_DATA_BYTES];
        assert_eq!(r.status(&mut buf), None);
        assert_eq!(r.layer_status(), None);
        let mut s = LayerStatus::ZERO;
        s.extent_w = 1920;
        s.drawn = 77;
        assert!(w.set_layer_status(&s));
        assert_eq!(r.layer_status(), Some(s));
        assert!(w.set_status(9, b"hello"));
        assert_eq!(r.status(&mut buf), Some((9, 5)));
        assert_eq!(&buf[..5], b"hello");
        // A status block a crashed writer left odd is released on attach.
        let seq = w.region().u64_at(OFF_STATUS_SEQ);
        seq.store(seq.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        assert_eq!(r.status(&mut buf), None);
        assert!(!w.set_status(1, b"x"));
        let w = RingWriter::new(w.region().clone()).unwrap();
        assert!(w.set_status(1, b"x"));
        assert_eq!(r.status(&mut buf), Some((1, 1)));
    }

    #[test]
    fn hostile_header_values() {
        let (w, mut r) = ring(16);
        w.push(&Record::series(1, 0, 0.0));
        // A writer that sets reserve to garbage can't push the reader out of bounds.
        w.region().u64_at(OFF_RESERVE).store(u64::MAX, Ordering::Relaxed);
        let p = r.poll(usize::MAX, |_, _| {});
        assert!(p.lost > 0);
        // Garbage commits are classified, never read as records.
        for k in 0..16 {
            w.region().u64_at(RING_HEADER_BYTES + k * SLOT_BYTES).store(fastrand::u64(..), Ordering::Relaxed);
        }
        let _ = r.poll(usize::MAX, |_, _| {});
        w.region().u64_at(OFF_RESERVE).store(3, Ordering::Relaxed);
        let p = r.poll(usize::MAX, |_, _| {});
        assert!(p.reset);
    }

    /// Model check: random bursts of pushes (some never committed) and polls of random size.
    /// Delivered records are exactly the pushed ones, in order, minus what the reader was
    /// told it lost; when fully drained, read + lost covers every index.
    #[test]
    fn random_pushes_and_polls() {
        for _ in 0..200 {
            let slots = [16, 32, 64][fastrand::usize(..3)];
            let (w, mut r) = ring(slots);
            r.set_stuck_polls(1);
            let mut pushed: Vec<Option<i64>> = vec![];
            let (mut read, mut lost) = (0u64, 0u64);
            let mut next = 0usize;
            for _ in 0..fastrand::usize(1..60) {
                for _ in 0..fastrand::usize(0..(slots as usize * 2)) {
                    if fastrand::u8(..) < 4 {
                        w.claim_without_commit_for_test();
                        pushed.push(None);
                    } else {
                        let t = fastrand::i64(..);
                        // A slot left busy by a dead writer drops later laps' records.
                        pushed.push(w.push(&Record::series(1, t, 0.0)).map(|_| t));
                    }
                }
                let p = r.poll(fastrand::usize(0..slots as usize * 2), |i, rec| {
                    let i = i as usize;
                    assert!(i >= next);
                    assert_eq!(Some(rec.t), pushed[i]);
                    next = i + 1;
                });
                read += p.read;
                lost += p.lost + p.torn;
            }
            for _ in 0..pushed.len() + 4 {
                let p = r.poll(usize::MAX, |i, rec| assert_eq!(Some(rec.t), pushed[i as usize]));
                read += p.read;
                lost += p.lost + p.torn;
            }
            // A dead slot with nothing after it is waited on, not skipped.
            let waiting = r.reserve() - r.cursor();
            assert!(waiting <= 1 && (waiting == 0 || pushed.last() == Some(&None)));
            assert_eq!(read + lost + waiting, pushed.len() as u64);
            assert!(read <= pushed.iter().filter(|p| p.is_some()).count() as u64);
        }
    }

    /// Small ring, a writer lapping it constantly: every record the reader accepts must be
    /// exactly what was written for that index (no torn copies), and indices increase.
    #[test]
    fn concurrent_lapping_never_tears() {
        let (w, mut r) = ring(16);
        let w = Arc::new(w);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let (w, stop) = (w.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut k = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        // Every field derives from `t`, so a mix of two writes is visible.
                        let t = fastrand::i64(0..i64::MAX / 2);
                        let fields: Vec<i64> = (0..12).map(|j| t ^ (j * 0x9e37_79b9)).collect();
                        w.push(&Record::frame(1, t as u64, FLAG_REAL, t as u32, t, &fields).unwrap());
                        k += 1;
                    }
                    k
                })
            })
            .collect();
        let (mut read, mut torn, mut lost) = (0u64, 0u64, 0u64);
        let mut last = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            let p = r.poll(usize::MAX, |i, rec| {
                assert!(last.is_none_or(|l| i > l));
                last = Some(i);
                assert_eq!(rec.commit, i + 1);
                assert_eq!(rec.key, rec.t as u64);
                assert_eq!(rec.swapchain, rec.t as u32);
                for (j, f) in rec.fields().iter().enumerate() {
                    assert_eq!(*f, rec.t ^ (j as i64 * 0x9e37_79b9));
                }
            });
            read += p.read;
            torn += p.torn;
            lost += p.lost;
        }
        stop.store(true, Ordering::Relaxed);
        let written: u64 = writers.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(read > 0 && lost > 0, "read {read} lost {lost} torn {torn} of {written}");
    }
}
