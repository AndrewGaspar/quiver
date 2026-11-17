//! Reading the Sight layer's rings (SIGHT-HUD §3.6–3.8, §3.16) on a dedicated thread.
//!
//! Per session dir: discovery by inotify plus a periodic `list()` (Wine-made mod rings are
//! created in place, so a ring can be seen before its magic is written: `NotReady` means
//! retry). Per ring: the writer's pid (gone: the producer disconnects), `size_now` before
//! each drain, a SIGBUS guard over the mapping ([`super::sigbus`]), `Record::check` on every
//! record, a records-per-second cap and an invalid-record budget. Records are decoded here
//! and handed to the session's store under the hub's lock, briefly.
//!
//! Mod rings (`mod-*.ring`) are phase 2: they're seen and listed, not read.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ubi_perf::layout::{LAYER_METRIC_PRESENT, LayerStatus, PRODUCER_LAYER, RING_HEADER_BYTES, SLOT_BYTES};
use ubi_perf::record::LayerPresent;
use ubi_perf::ring::RingReader;
use ubi_perf::session::{FileKind, SessionDir};
use ubi_perf::shm::{Mapping, ShmRegion};

use super::hud::ImageView;
use super::sigbus::Guard;

pub const DRAIN_EVERY: Duration = Duration::from_millis(50);
/// Records read per ring per drain (20 000/s at 50 ms is 1000; leave headroom to catch up).
const MAX_PER_DRAIN: usize = 4096;
/// §3.16: above this a ring is disabled for the session.
const MAX_RECORDS_PER_SEC: u64 = ubi_perf::descriptor::limits::MAX_RING_RECORDS_PER_SEC as u64;
const LIST_EVERY: Duration = Duration::from_secs(1);

/// What a drain of one session found.
#[derive(Debug, Default)]
pub struct Drained {
    /// (swapchain, entry ns) per present, in order.
    pub presents: Vec<(u32, i64)>,
    /// One per layer ring.
    pub layers: Vec<LayerRing>,
}

/// The state of one layer ring, for the registry, the presenter choice and `hud status`.
#[derive(Debug, Clone, Default)]
pub struct LayerRing {
    pub file: String,
    pub pid: u32,
    pub alive: bool,
    /// The writer's heartbeat (its CLOCK_MONOTONIC_RAW ns).
    pub heartbeat: i64,
    pub status: Option<LayerStatus>,
    pub records: u64,
    pub lost: u64,
    pub rejected: u64,
    /// `ok`, `waiting`, or `invalid: <reason>`.
    pub ring: String,
}

struct Ring {
    // Field order matters: the guard unregisters before the mapping is unmapped.
    guard: Option<Guard>,
    reader: Option<RingReader<Mapping>>,
    path: PathBuf,
    state: LayerRing,
    retry_at: Instant,
    attempts: u32,
    invalid: Option<String>,
    /// Records in the current second, and invalid ones over the last 10 s.
    sec_start: Instant,
    sec_records: u64,
    bad_window: (Instant, u64, u64),
}

impl Ring {
    fn new(file: String, path: PathBuf, pid: u32) -> Ring {
        let now = Instant::now();
        Ring {
            guard: None,
            reader: None,
            path,
            state: LayerRing { file, pid, ring: "waiting".into(), ..Default::default() },
            retry_at: now,
            attempts: 0,
            invalid: None,
            sec_start: now,
            sec_records: 0,
            bad_window: (now, 0, 0),
        }
    }

    fn close(&mut self, why: Option<String>) {
        self.guard = None;
        self.reader = None;
        if let Some(w) = why {
            tracing::warn!(ring = %self.path.display(), "perf ring disabled: {w}");
            self.state.ring = format!("invalid: {w}");
            self.invalid = Some(w);
        }
    }

    fn try_open(&mut self) {
        let now = Instant::now();
        if self.invalid.is_some() || self.reader.is_some() || now < self.retry_at {
            return;
        }
        match ubi_perf::session::open_ring_reader(&self.path) {
            Ok(r) => {
                if r.info().producer_kind != PRODUCER_LAYER {
                    self.close(Some("not a layer ring".into()));
                    return;
                }
                let guard = Guard::register(r.region().base(), r.region().len());
                if !guard.is_guarded() {
                    // No guard slot left: don't read it at all rather than risk quiverd.
                    self.retry_at = now + Duration::from_secs(5);
                    return;
                }
                self.state.pid = r.info().writer_pid;
                self.guard = Some(guard);
                self.reader = Some(r);
                self.state.ring = "ok".into();
            }
            // Not initialized yet (in-place creation): retry soon.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.retry_at = now + Duration::from_millis(100),
            Err(e) => {
                self.attempts += 1;
                if self.attempts > 20 {
                    self.close(Some(format!("can't open: {e}")));
                } else {
                    self.retry_at = now + Duration::from_millis(250);
                }
            }
        }
    }

    fn drain(&mut self, out: &mut Drained) {
        self.try_open();
        let Some(reader) = self.reader.as_mut() else { return };
        let info = *reader.info();
        let expected = RING_HEADER_BYTES as u64 + info.slot_count as u64 * SLOT_BYTES as u64;
        // A replaced file (a new process reusing the pid's name) or a removed one.
        if !reader.region().is_file_at(&self.path) {
            self.close(None);
            self.state = LayerRing { file: self.state.file.clone(), pid: self.state.pid, ring: "waiting".into(), ..Default::default() };
            self.retry_at = Instant::now();
            return;
        }
        match reader.region().size_now() {
            Ok(n) if n >= expected => {}
            Ok(n) => return self.close(Some(format!("truncated to {n} bytes"))),
            Err(e) => return self.close(Some(format!("stat failed: {e}"))),
        }
        self.state.alive = pid_alive(info.writer_pid);
        self.state.heartbeat = reader.heartbeat();
        let mut bad = 0u64;
        let mut presents = Vec::new();
        let poll = reader.poll(MAX_PER_DRAIN, |_, rec| {
            if rec.check(LAYER_METRIC_PRESENT).is_err() {
                bad += 1;
                return;
            }
            match LayerPresent::from_record(rec) {
                Some(p) => presents.push((p.swapchain, p.entry_ns)),
                None => bad += 1,
            }
        });
        let status = reader.layer_status();
        let faulted = self.guard.as_ref().is_some_and(|g| g.faulted() > 0);
        if faulted {
            // The pages read as zeros after the fault; nothing from this drain is trusted.
            return self.close(Some("truncated while mapped".into()));
        }
        if let Some(s) = status {
            self.state.status = Some(s);
        }
        let n = poll.read;
        self.state.records += n;
        self.state.lost += poll.lost + poll.torn;
        self.state.rejected += bad;
        // Rate cap.
        let now = Instant::now();
        if now.duration_since(self.sec_start) >= Duration::from_secs(1) {
            self.sec_start = now;
            self.sec_records = 0;
        }
        self.sec_records += n;
        if self.sec_records > MAX_RECORDS_PER_SEC {
            return self.close(Some(format!("more than {MAX_RECORDS_PER_SEC} records/s")));
        }
        // Invalid budget: > 1% over 10 s (with enough records to judge).
        let (start, total, invalid) = &mut self.bad_window;
        if now.duration_since(*start) >= Duration::from_secs(10) {
            *start = now;
            *total = 0;
            *invalid = 0;
        }
        *total += n + bad;
        *invalid += bad;
        let (total, invalid) = (*total, *invalid);
        if total >= 100 && invalid * 100 > total {
            return self.close(Some(format!("{invalid} of {total} records invalid")));
        }
        out.presents.extend(presents);
    }
}

fn pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// inotify on a session dir: "something changed, list again".
struct Inotify {
    fd: libc::c_int,
}

impl Inotify {
    fn watch(dir: &Path) -> Option<Inotify> {
        // SAFETY: plain syscalls; the fd is closed on drop.
        unsafe {
            let fd = libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
            let mask = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_CLOSE_WRITE;
            if libc::inotify_add_watch(fd, c.as_ptr(), mask) < 0 {
                libc::close(fd);
                return None;
            }
            Some(Inotify { fd })
        }
    }

    /// Whether any event arrived since the last call.
    fn changed(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut any = false;
        loop {
            // SAFETY: reading into a buffer we own.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return any;
            }
            any = true;
        }
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        // SAFETY: our fd.
        unsafe { libc::close(self.fd) };
    }
}

/// The rings of one session dir.
pub struct SessionRings {
    dir: SessionDir,
    inotify: Option<Inotify>,
    rings: BTreeMap<String, Ring>,
    last_list: Option<Instant>,
    /// Mod rings seen (phase 2: not read).
    pub mod_rings: Vec<String>,
    /// `hud.image`, read-only, for its publish counter and quiver-sight's heartbeat.
    image: Option<(Guard, ubi_perf::hud::HudReader<Mapping>, (u64, u64))>,
    image_view: Option<ImageView>,
    image_checked: Option<Instant>,
}

impl SessionRings {
    pub fn new(dir: SessionDir) -> SessionRings {
        let inotify = Inotify::watch(dir.path());
        SessionRings { dir, inotify, rings: BTreeMap::new(), last_list: None, mod_rings: Vec::new(), image: None, image_view: None, image_checked: None }
    }

    /// `hud.image`'s state, re-read at most every 250 ms.
    pub fn image(&mut self, now_ns: i64) -> Option<ImageView> {
        if self.image_checked.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
            return self.image_view.clone();
        }
        self.image_checked = Some(Instant::now());
        use std::os::unix::fs::MetadataExt;
        let path = self.dir.file(&FileKind::HudImage);
        let id_now = std::fs::symlink_metadata(&path).ok().map(|m| (m.dev(), m.ino()));
        if self.image.as_ref().is_some_and(|(g, _, id)| g.faulted() > 0 || id_now != Some(*id)) {
            self.image = None;
        }
        if self.image.is_none() {
            use ubi_perf::hud::hud_file_bytes;
            let m = Mapping::open_read(&path, hud_file_bytes(4096), hud_file_bytes(ubi_perf::layout::HUD_MAX_BUFFER_BYTES)).ok()?;
            let md = m.file().metadata().ok()?;
            // Guard first: reading the header can fault too.
            let g = Guard::register(m.base(), m.len());
            if !g.is_guarded() {
                return None;
            }
            let r = ubi_perf::hud::HudReader::new(m).ok()?;
            self.image = Some((g, r, (md.dev(), md.ino())));
        }
        let (_, r, _) = self.image.as_ref()?;
        let published = r.published();
        let heartbeat = r.heartbeat();
        let writer_pid = r.writer_pid();
        if self.image.as_ref().is_some_and(|(g, _, _)| g.faulted() > 0) {
            self.image = None;
            return None;
        }
        let changed_at = match &self.image_view {
            Some(p) if p.published == published => p.changed_at,
            _ => now_ns,
        };
        self.image_view = Some(ImageView { path, buffer_bytes: r.info().buffer_bytes, published, heartbeat, writer_pid, changed_at });
        self.image_view.clone()
    }

    fn discover(&mut self) {
        let changed = self.inotify.as_ref().is_some_and(|i| i.changed());
        if !changed && self.last_list.is_some_and(|t| t.elapsed() < LIST_EVERY) {
            return;
        }
        self.last_list = Some(Instant::now());
        let Ok(files) = self.dir.list() else { return };
        self.mod_rings.clear();
        for (kind, path) in files {
            match kind {
                FileKind::Layer { pid } => {
                    let name = kind.file_name();
                    self.rings.entry(name.clone()).or_insert_with(|| Ring::new(name, path, pid));
                }
                FileKind::Mod { .. } => self.mod_rings.push(kind.file_name()),
                FileKind::HudImage => {}
            }
        }
        // Files that went away (a ring the writer removed): drop their readers.
        let present: Vec<String> = self.dir.list().map(|l| l.into_iter().map(|(k, _)| k.file_name()).collect()).unwrap_or_default();
        for (name, r) in &mut self.rings {
            if !present.contains(name) && r.reader.is_some() {
                r.close(None);
                r.state.alive = false;
                r.state.ring = "removed".into();
            }
        }
    }

    pub fn drain(&mut self) -> Drained {
        self.discover();
        let mut out = Drained::default();
        for r in self.rings.values_mut() {
            r.drain(&mut out);
            out.layers.push(r.state.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubi_perf::record::Record;
    use ubi_perf::session::{SessionId, SessionDir};

    fn shm() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("quiverd-perf-test-").tempdir_in("/dev/shm").unwrap()
    }

    fn present(seq: u64, t: i64) -> Record {
        LayerPresent { swapchain: 3, seq, entry_ns: t, return_ns: t + 50_000, ..Default::default() }.to_record()
    }

    #[test]
    fn discovers_reads_and_validates_a_layer_ring() {
        let root = shm();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-rings").unwrap()).unwrap();
        let mut rings = SessionRings::new(dir.clone());
        assert!(rings.drain().layers.is_empty());
        // Our own pid: alive.
        let pid = std::process::id();
        let w = dir.create_layer_ring(pid, [9; 16]).unwrap();
        for i in 1..=50 {
            w.push(&present(i, 1_000_000 * i as i64));
        }
        // A record with a bad metric index, and one that isn't a present.
        let mut bad = present(51, 51_000_000);
        bad.metric = 9;
        w.push(&bad);
        w.push(&Record::series(1, 5, 1.0));
        let mut s = LayerStatus::ZERO;
        s.extent_w = 2560;
        s.extent_h = 1600;
        s.swapchain = 3;
        w.set_layer_status(&s);
        w.heartbeat(77);
        let d = rings.drain();
        assert_eq!(d.presents.len(), 50);
        assert_eq!(d.presents[0], (3, 1_000_000));
        let l = &d.layers[0];
        assert_eq!((l.pid, l.alive, l.heartbeat, l.ring.as_str()), (pid, true, 77, "ok"));
        assert_eq!(l.rejected, 2);
        assert_eq!(l.status.unwrap().extent_w, 2560);
        // Nothing new: nothing delivered.
        assert!(rings.drain().presents.is_empty());
    }

    #[test]
    fn dead_writer_and_truncated_ring() {
        let root = shm();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-dead").unwrap()).unwrap();
        // A pid that can't exist.
        let w = dir.create_layer_ring(4_000_000_000, [1; 16]).unwrap();
        w.push(&present(1, 1));
        let mut rings = SessionRings::new(dir.clone());
        let d = rings.drain();
        assert!(!d.layers[0].alive);
        // The game truncates its mapped ring: quiverd notices instead of crashing.
        let path = dir.file(&FileKind::Layer { pid: 4_000_000_000 });
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(100).unwrap();
        let d = rings.drain();
        assert!(d.layers[0].ring.starts_with("invalid: truncated"), "{}", d.layers[0].ring);
        drop(w);
    }

    #[test]
    fn garbage_ring_is_disabled() {
        let root = shm();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-junk").unwrap()).unwrap();
        let w = dir.create_layer_ring(std::process::id(), [2; 16]).unwrap();
        let mut rings = SessionRings::new(dir.clone());
        // 200 records of which most fail checks.
        for i in 0..200 {
            let mut r = present(i, i as i64);
            if i % 2 == 0 {
                r.nfields = 40;
            }
            w.push(&r);
        }
        let d = rings.drain();
        assert!(d.layers[0].ring.starts_with("invalid:"), "{}", d.layers[0].ring);
    }
}
