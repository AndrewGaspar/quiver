//! The video pipeline's threads and what they share.
//!
//! ```text
//! link.video ─► decode thread ──(Converted)──► mailbox ─► present thread ─► swapchain
//!               gap tracker, libav,             latest      overlay, Stage B
//!               Stage A (convert)               only          │
//!                     │                                       ▼
//!                     └─► convert watcher (GPU done:        present waiter
//!                         decode latency)                   (vkWaitForPresentKHR:
//!                                                            present latency)
//! ```
//!
//! The decode thread decodes every frame it can (later frames refer to it) but the mailbox
//! keeps only the newest picture: when the presenter is slower than the stream (FIFO on a
//! slower display), older pictures are dropped there, never queued.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ash::vk;
use crossbeam_channel::{Receiver, Sender, unbounded};
use quiver_stream_proto::stamp::Stamp;
use quiver_stream_proto::{Control, Micros, Pacing, StreamConfig, now_micros};
use tracing::{info, warn};

use crate::av::Backend;
use crate::decode::VideoDecoder;
use crate::gap::{FrameMeta, GapTracker, Verdict};
use crate::gpu::Gpu;
use crate::gpu::convert::{Converted, Converter};
use crate::gpu::present::{PresentConfig, Presenter, SwapLock, fit};
use crate::link::{Outgoing, VideoIn};
use crate::stats::{ClockSync, Samples, Stats};
use crate::ui::atlas::Atlas;
use crate::ui::overlay::{self, OverlayState};

/// Frames waiting to be decoded beyond which the decode thread drops them and restarts from
/// a keyframe, so latency can't grow without bound.
const BACKLOG_MAX: usize = 6;

/// A picture waiting to be shown.
#[derive(Debug, Clone, Copy)]
pub struct Shown {
    pub pic: Converted,
    pub seq: u64,
    /// Host clock.
    pub captured: Micros,
    /// Our clock.
    pub arrived: Micros,
}

#[derive(Default)]
pub struct MailboxState {
    pub latest: Option<Shown>,
    /// Pictures published and taken, to count the ones never shown.
    published: u64,
    taken: u64,
    /// Draw again (the overlay or the window changed).
    pub redraw: bool,
}

/// What the presenter is set to, changed by the main thread.
#[derive(Debug, Clone)]
pub struct PresentPrefs {
    pub pacing: Pacing,
    pub mode_override: Option<vk::PresentModeKHR>,
    /// HDR allowed (the person didn't turn it off).
    pub hdr: bool,
    pub window: (u32, u32),
}

/// One frame's timings, in `--measure` mode.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FrameRecord {
    pub seq: u64,
    /// When it arrived (our clock).
    #[serde(skip)]
    pub arrived: Micros,
    /// The test pattern read back from the decoded picture matched the frame.
    pub pattern: Option<bool>,
    pub decode_us: u64,
    pub present_us: Option<u64>,
    pub capture_to_present_us: Option<u64>,
}

#[derive(Default)]
pub struct MeasureLog {
    pub frames: HashMap<u64, FrameRecord>,
    pub shown: u64,
}

/// Everything the threads share.
pub struct Shared {
    pub stats: Stats,
    pub clock: Mutex<ClockSync>,
    pub config: Mutex<Option<StreamConfig>>,
    config_cv: Condvar,
    pub mailbox: Mutex<MailboxState>,
    mailbox_cv: Condvar,
    pub overlay: Mutex<OverlayState>,
    pub present: Mutex<PresentPrefs>,
    /// Where the picture is in the window (for absolute mouse positions).
    pub picture_rect: Mutex<(i32, i32, u32, u32)>,
    pub decoder_name: Mutex<String>,
    pub present_name: Mutex<String>,
    /// Converter timeline value → when the GPU finished it (our clock).
    ready: Mutex<VecDeque<(u64, Micros)>>,
    pub quit: AtomicBool,
    pub measure: Option<Mutex<MeasureLog>>,
    pub out: Sender<Outgoing>,
    pub frames_shown: AtomicU64,
}

impl Shared {
    pub fn new(out: Sender<Outgoing>, present: PresentPrefs, measure: bool) -> Arc<Self> {
        Arc::new(Self {
            stats: Stats::default(),
            clock: Mutex::new(ClockSync::default()),
            config: Mutex::new(None),
            config_cv: Condvar::new(),
            mailbox: Mutex::new(MailboxState::default()),
            mailbox_cv: Condvar::new(),
            overlay: Mutex::new(OverlayState::default()),
            present: Mutex::new(present),
            picture_rect: Mutex::new((0, 0, 0, 0)),
            decoder_name: Mutex::new(String::new()),
            present_name: Mutex::new(String::new()),
            ready: Mutex::new(VecDeque::new()),
            quit: AtomicBool::new(false),
            measure: measure.then(|| Mutex::new(MeasureLog::default())),
            out,
            frames_shown: AtomicU64::new(0),
        })
    }

    pub fn set_config(&self, c: StreamConfig) {
        *self.config.lock().unwrap() = Some(c);
        self.config_cv.notify_all();
    }

    /// Ask the presenter to draw again.
    pub fn redraw(&self) {
        self.mailbox.lock().unwrap().redraw = true;
        self.mailbox_cv.notify_all();
    }

    pub fn stop(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.mailbox_cv.notify_all();
        self.config_cv.notify_all();
    }

    fn send(&self, c: Control) {
        let _ = self.out.send(Outgoing::Control(c));
    }

    fn ready_at(&self, value: u64) -> Option<Micros> {
        self.ready.lock().unwrap().iter().find(|r| r.0 == value).map(|r| r.1)
    }

    fn publish(&self, s: Shown) {
        let mut m = self.mailbox.lock().unwrap();
        m.latest = Some(s);
        m.published += 1;
        self.mailbox_cv.notify_all();
    }
}

// --- the decode thread ---------------------------------------------------------------------

/// Start the decode thread. `conv` is the Stage A converter whose [`Converter::reads`] the
/// presenter was made with.
pub fn spawn_decode(
    shared: Arc<Shared>,
    gpu: Arc<Gpu>,
    mut conv: Converter,
    rx: Receiver<VideoIn>,
    backend: Option<Backend>,
) -> Result<std::thread::JoinHandle<()>> {
    conv.readback = shared.measure.is_some();
    let (wtx, wrx) = unbounded::<(u64, Micros, u64)>();
    if shared.measure.is_none() {
        let (sh, g) = (shared.clone(), gpu.clone());
        std::thread::Builder::new().name("convert-watch".into()).spawn(move || convert_watch(sh, g, wrx))?;
    }
    Ok(std::thread::Builder::new().name("decode".into()).spawn(move || {
        decode_loop(&shared, &gpu, conv, rx, backend, wtx);
        shared.stop();
    })?)
}

/// Wait on each conversion in turn and note when the GPU finished it.
fn convert_watch(shared: Arc<Shared>, gpu: Arc<Gpu>, rx: Receiver<(u64, Micros, u64)>) {
    let mut sem = None;
    while let Ok((value, arrived, sem_raw)) = rx.recv() {
        let s: vk::Semaphore = *sem.get_or_insert(vk::Handle::from_raw(sem_raw));
        if !crate::gpu::util::wait_timeline(&gpu, s, value, 1_000_000_000).unwrap_or(false) {
            continue;
        }
        let t = now_micros();
        shared.stats.window.lock().unwrap().decode.push(t.saturating_sub(arrived));
        let mut r = shared.ready.lock().unwrap();
        r.push_back((value, t));
        while r.len() > 64 {
            r.pop_front();
        }
    }
}

fn decode_loop(
    shared: &Arc<Shared>,
    gpu: &Arc<Gpu>,
    mut conv: Converter,
    rx: Receiver<VideoIn>,
    backend: Option<Backend>,
    watch: Sender<(u64, Micros, u64)>,
) {
    let mut vd = VideoDecoder::new(gpu.clone(), backend);
    let mut gap = GapTracker::new();
    let mut config: Option<StreamConfig> = None;
    let mut last_drain: Option<Instant> = None;
    loop {
        if shared.quit.load(Ordering::SeqCst) {
            return;
        }
        // Falling behind (a slow decoder): never let a queue build up. Drop what's waiting,
        // tell the host it's lost, and start again from a keyframe.
        if rx.len() > BACKLOG_MAX && last_drain.is_none_or(|t: Instant| t.elapsed() > Duration::from_secs(1)) {
            last_drain = Some(Instant::now());
            let mut dropped: Option<(u64, u64)> = None;
            while let Ok(m) = rx.try_recv() {
                let (a, b) = match m {
                    VideoIn::Frame(f) => (f.seq, f.seq),
                    VideoIn::Lost { first, last } => (first, last),
                };
                dropped = Some(dropped.map_or((a, b), |(x, _)| (x, b)));
            }
            if let Some((first, last)) = dropped {
                warn!("decoding fell behind: dropped frames {first}..={last}");
                shared.stats.window.lock().unwrap().frames_dropped += (last - first + 1) as u32;
                for c in gap.on_lost(first, last, Instant::now()) {
                    shared.send(c);
                }
                shared.send(Control::Keyframe);
                shared.overlay.lock().unwrap().toast("This device can't decode the stream fast enough");
            }
            continue;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(m) => m,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if let Some(c) = gap.tick(Instant::now()) {
                    shared.send(c);
                }
                continue;
            }
            Err(_) => return,
        };
        let now = Instant::now();
        let frame = match msg {
            VideoIn::Lost { first, last } => {
                shared.stats.window.lock().unwrap().frames_lost += (last.saturating_sub(first) + 1) as u32;
                for c in gap.on_lost(first, last, now) {
                    shared.send(c);
                }
                continue;
            }
            VideoIn::Frame(f) => f,
        };
        let arrived = now_micros();
        {
            let mut w = shared.stats.window.lock().unwrap();
            w.frames_received += 1;
            w.video_bytes += frame.data.len() as u64;
            if let Some(local) = shared.clock.lock().unwrap().to_local(frame.captured) {
                w.network.push(arrived.saturating_sub(local));
            }
        }
        // The stream's settings: wait for the Configure that precedes the first frame.
        let current = {
            let mut c = shared.config.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while c.is_none() && Instant::now() < deadline && !shared.quit.load(Ordering::SeqCst) {
                c = shared.config_cv.wait_timeout(c, Duration::from_millis(100)).unwrap().0;
            }
            c.clone()
        };
        let Some(current) = current else {
            warn!("a frame before any Configure; dropped");
            continue;
        };
        if config.as_ref().is_none_or(|c| c.codec != current.codec) || vd.codec().is_none() {
            match vd.open(current.codec) {
                Ok(b) => {
                    *shared.decoder_name.lock().unwrap() = format!("{b:?}").to_lowercase();
                    conv.forget_decoder_images();
                    gap.reset(now);
                }
                Err(e) => {
                    warn!("{e:#}");
                    shared.overlay.lock().unwrap().waiting = Some(format!("Can't decode {:?} here", current.codec));
                    config = Some(current);
                    continue;
                }
            }
        }
        config = Some(current.clone());
        let (verdict, ctl) = gap.on_frame(FrameMeta { seq: frame.seq, keyframe: frame.keyframe, refs_from_seq: frame.refs_from_seq }, now);
        for c in ctl {
            shared.send(c);
        }
        if verdict != Verdict::Decode {
            if verdict == Verdict::Skip {
                shared.stats.window.lock().unwrap().frames_dropped += 1;
            }
            continue;
        }
        let pics = match vd.decode(&frame.data, frame.seq as i64) {
            Ok(p) => p,
            Err(e) => {
                warn!("decoding frame {}: {}", frame.seq, crate::av::strerror(e));
                for c in gap.on_decode_error(frame.seq, now) {
                    shared.send(c);
                }
                continue;
            }
        };
        let hdr = current.color.hdr.is_some();
        for mut pic in pics {
            let c = match conv.convert(&mut pic, hdr, current.color.full_range) {
                Ok(c) => c,
                Err(e) => {
                    warn!("showing a {:?} picture: {e:#}", vd.backend());
                    drop(pic);
                    if pic_backend_failed(&mut vd, &mut conv, shared) {
                        // The new decoder needs a keyframe now.
                        gap.reset(Instant::now());
                        shared.send(Control::Keyframe);
                    }
                    break;
                }
            };
            drop(pic);
            let seq = frame.seq;
            if let Some(m) = &shared.measure {
                let stamp: Option<Stamp> = conv.read_pattern(&c).ok().flatten();
                let ready = now_micros();
                shared.stats.window.lock().unwrap().decode.push(ready.saturating_sub(arrived));
                let mut r = shared.ready.lock().unwrap();
                r.push_back((c.value, ready));
                if r.len() > 64 {
                    r.pop_front();
                }
                drop(r);
                m.lock().unwrap().frames.insert(
                    seq,
                    FrameRecord {
                        seq,
                        pattern: Some(stamp.is_some_and(|s| s.seq == seq)),
                        decode_us: ready.saturating_sub(arrived),
                        arrived,
                        ..Default::default()
                    },
                );
            } else {
                let _ = watch.send((c.value, arrived, vk::Handle::as_raw(c.semaphore)));
            }
            {
                let mut o = shared.overlay.lock().unwrap();
                if o.waiting.is_some() {
                    o.waiting = None;
                }
            }
            shared.publish(Shown { pic: c, seq, captured: frame.captured, arrived });
        }
    }
}

/// A picture from this backend couldn't be shown: switch to the next backend.
fn pic_backend_failed(vd: &mut VideoDecoder, conv: &mut Converter, shared: &Shared) -> bool {
    conv.forget_decoder_images();
    match vd.fall_back() {
        Ok(b) => {
            info!("now decoding with {b:?}");
            *shared.decoder_name.lock().unwrap() = format!("{b:?}").to_lowercase();
            true
        }
        Err(e) => {
            warn!("{e:#}");
            false
        }
    }
}

// --- the present thread ----------------------------------------------------------------------

enum WaitMsg {
    Wait { swapchain: vk::SwapchainKHR, id: u64, shown: Shown },
    Flush(Sender<()>),
}

pub fn spawn_present(
    shared: Arc<Shared>,
    gpu: Arc<Gpu>,
    presenter: Presenter,
    atlas: Arc<Atlas>,
    scale: f32,
) -> Result<std::thread::JoinHandle<()>> {
    let (wtx, wrx) = unbounded::<WaitMsg>();
    if gpu.present_wait.is_some() {
        let (sh, g, l) = (shared.clone(), gpu.clone(), presenter.swap_lock());
        std::thread::Builder::new().name("present-wait".into()).spawn(move || present_wait(sh, g, l, wrx))?;
    }
    Ok(std::thread::Builder::new().name("present".into()).spawn(move || {
        if let Err(e) = present_loop(&shared, &gpu, presenter, &atlas, scale, wtx) {
            warn!("presenting: {e:#}");
        }
        shared.stop();
    })?)
}

fn record_present(shared: &Shared, shown: &Shown, at: Micros) {
    let ready = shared.ready_at(shown.pic.value);
    let local = shared.clock.lock().unwrap().to_local(shown.captured);
    let present_us = ready.map(|r| at.saturating_sub(r));
    let c2p = local.map(|l| at.saturating_sub(l));
    {
        let mut w = shared.stats.window.lock().unwrap();
        if let Some(p) = present_us {
            w.present.push(p);
        }
        if let Some(c) = c2p {
            w.capture_to_present.push(c);
        }
    }
    if let Some(m) = &shared.measure
        && let Some(r) = m.lock().unwrap().frames.get_mut(&shown.seq)
    {
        r.present_us = present_us;
        r.capture_to_present_us = c2p;
    }
}

fn present_wait(shared: Arc<Shared>, gpu: Arc<Gpu>, lock: SwapLock, rx: Receiver<WaitMsg>) {
    while let Ok(m) = rx.recv() {
        match m {
            WaitMsg::Flush(ack) => {
                let _ = ack.send(());
            }
            WaitMsg::Wait { swapchain, id, shown } => {
                if Presenter::wait_present(&gpu, &lock, swapchain, id, Duration::from_millis(250)) {
                    record_present(&shared, &shown, now_micros());
                }
            }
        }
    }
}

fn present_loop(
    shared: &Arc<Shared>,
    gpu: &Arc<Gpu>,
    mut presenter: Presenter,
    atlas: &Atlas,
    scale: f32,
    waiter: Sender<WaitMsg>,
) -> Result<()> {
    let mut last: Option<Shown> = None;
    let mut last_shown_at: Option<Micros> = None;
    let flush = |waiter: &Sender<WaitMsg>| {
        if gpu.present_wait.is_some() {
            let (t, r) = crossbeam_channel::bounded(1);
            if waiter.send(WaitMsg::Flush(t)).is_ok() {
                let _ = r.recv_timeout(Duration::from_secs(1));
            }
        }
    };
    loop {
        // Wait for a new picture, a redraw, or (while something animates) the next tick.
        {
            let mut m = shared.mailbox.lock().unwrap();
            let animating = shared.overlay.lock().unwrap().animating(Instant::now());
            let deadline = Instant::now() + if animating { Duration::from_millis(250) } else { Duration::from_secs(1) };
            while m.published == m.taken && !m.redraw && !shared.quit.load(Ordering::SeqCst) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                m = shared.mailbox_cv.wait_timeout(m, deadline - now).unwrap().0;
            }
            m.redraw = false;
        }
        if shared.quit.load(Ordering::SeqCst) {
            break;
        }
        let prefs = shared.present.lock().unwrap().clone();
        let hdr = if prefs.hdr { shared.config.lock().unwrap().as_ref().and_then(|c| c.color.hdr.clone()) } else { None };
        let cfg = PresentConfig { pacing: prefs.pacing, mode_override: prefs.mode_override, hdr, window: prefs.window };
        if presenter.needs_rebuild(&cfg) {
            flush(&waiter);
        }
        // The swapchain image first (FIFO blocks here), then the newest picture: late
        // latching, so a picture that arrived during the wait is the one shown.
        let Some(acq) = presenter.acquire(&cfg)? else {
            continue;
        };
        let new = {
            let mut m = shared.mailbox.lock().unwrap();
            let new = if m.published > m.taken {
                let skipped = m.published - m.taken - 1;
                if skipped > 0 {
                    shared.stats.window.lock().unwrap().frames_dropped += skipped as u32;
                }
                m.taken = m.published;
                m.latest
            } else {
                None
            };
            if let Some(s) = new.or(last) {
                presenter.claim(&s.pic);
            }
            new
        };
        if new.is_some() {
            last = new;
        }
        let verts = {
            let mut o = shared.overlay.lock().unwrap();
            o.expire(Instant::now());
            overlay::build(&o, atlas, scale, (prefs.window.0 as f32, prefs.window.1 as f32), Instant::now())
        };
        let r = presenter.present(acq, last.as_ref().map(|s| &s.pic), &verts, &cfg)?;
        *shared.present_name.lock().unwrap() = presenter.describe();
        if let Some(s) = &last {
            *shared.picture_rect.lock().unwrap() = fit((s.pic.extent.width, s.pic.extent.height), prefs.window);
        }
        let (Some(p), Some(shown)) = (r, new) else {
            continue;
        };
        shared.frames_shown.fetch_add(1, Ordering::Relaxed);
        shared.stats.window.lock().unwrap().frames_shown += 1;
        let interval = shared.config.lock().unwrap().as_ref().map_or(0, |c| 1_000_000_000 / c.refresh_mhz.max(1) as u64);
        shared.stats.shown(&mut last_shown_at, now_micros(), interval);
        if let Some(m) = &shared.measure {
            m.lock().unwrap().shown += 1;
        }
        match p.present_id {
            Some(id) => {
                let _ = waiter.send(WaitMsg::Wait { swapchain: p.swapchain, id, shown });
            }
            None => record_present(shared, &shown, now_micros()),
        }
    }
    flush(&waiter);
    drop(presenter);
    Ok(())
}

/// The `--measure` report.
#[derive(Debug, serde::Serialize)]
pub struct MeasureReport {
    pub frames: usize,
    pub shown: u64,
    pub pattern_ok: usize,
    pub pattern_bad: usize,
    pub decoder: String,
    pub present: String,
    pub decode_us: crate::bench::Summary,
    pub present_us: crate::bench::Summary,
    pub capture_to_present_us: crate::bench::Summary,
}

pub fn measure_report(shared: &Shared) -> Option<MeasureReport> {
    let m = shared.measure.as_ref()?.lock().unwrap();
    let (mut d, mut p, mut c) = (Samples::default(), Samples::default(), Samples::default());
    let (mut ok, mut bad) = (0, 0);
    // The first second includes start-up (device and swapchain creation, the decoder
    // probe for the Hello); leave it out.
    let mut recs: Vec<&FrameRecord> = m.frames.values().collect();
    recs.sort_by_key(|r| r.seq);
    let first = recs.first().map_or(0, |r| r.arrived);
    let recs: Vec<&FrameRecord> = recs.into_iter().filter(|r| r.arrived >= first + 1_000_000).collect();
    for r in &recs {
        match r.pattern {
            Some(true) => ok += 1,
            Some(false) => bad += 1,
            None => {}
        }
        d.push(r.decode_us);
        if let Some(x) = r.present_us {
            p.push(x);
        }
        if let Some(x) = r.capture_to_present_us {
            c.push(x);
        }
    }
    Some(MeasureReport {
        frames: recs.len(),
        shown: m.shown,
        pattern_ok: ok,
        pattern_bad: bad,
        decoder: shared.decoder_name.lock().unwrap().clone(),
        present: shared.present_name.lock().unwrap().clone(),
        decode_us: (&d).into(),
        present_us: (&p).into(),
        capture_to_present_us: (&c).into(),
    })
}
