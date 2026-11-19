//! The layer's side of the session dir (SIGHT-HUD §3.6–3.8): its present ring
//! (`layer-<pid>.ring`) with the status block, the heartbeat thread, and `hud.image`. The
//! in-game HUD keys ([`crate::keys`]) start with it and report through the status block.
//!
//! Everything that makes a syscall (creating the ring, opening or re-opening `hud.image`,
//! stat-ing it) happens at swapchain creation or on the heartbeat thread, never in the
//! present path. The present path only pushes ring records (lock-free), bumps atomics and
//! `try_lock`s the HUD reader.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use quiver_perf::LayerStatus;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::HudReader;
use quiver_perf::layout::{BACKDROP_ERR_OFF, HUD_HEADER_BYTES, KEYS_WL_SHIFT, LAYER_BLUR_READY, LAYER_BLURRING, LAYER_FOCUS_KNOWN, LAYER_FOCUSED, LAYER_PANICKED};
use quiver_perf::ring::RingWriter;
use quiver_perf::session::{FileKind, SessionDir, open_hud_reader};
use quiver_perf::shm::Mapping;

use crate::{keys, log};

/// How often the thread heartbeats, rewrites the status block and looks for `hud.image`.
const TICK: Duration = Duration::from_millis(250);

pub struct Telemetry {
    pub ring: RingWriter<Mapping>,
    hud_path: PathBuf,
    /// Everything but the counters and dynamic flags below; changed by the instance, device and
    /// swapchain hooks (never per present).
    pub status: Mutex<LayerStatus>,
    pub presents: AtomicU64,
    pub drawn: AtomicU64,
    pub hud_published: AtomicU64,
    pub draw_error: AtomicU32,
    /// `LAYER_*` flags the present path sets (DRAWING, PRESENT_ID, PANICKED, HUD_OFF).
    pub flags: AtomicU32,
    /// In-game HUD keys ([`crate::keys`]): `KEYS_*` | `KEYS_WL_*` << 8, and the presses seen.
    pub keys: AtomicU32,
    pub key_toggles: AtomicU32,
    pub key_nexts: AtomicU32,
    pub key_panels: AtomicU32,
    /// `LAYER_FOCUS_KNOWN` | `LAYER_FOCUSED` ([`crate::focus`]), from the keys thread.
    pub focus: AtomicU32,
    /// The backdrop blur (crate::blur): the last blurred present (CLOCK_MONOTONIC_RAW ns), its
    /// GPU time (running mean, ns), and an error found at present time (`BACKDROP_ERR_*`)
    /// that overrides the target swapchain's readiness until the target changes.
    pub blur_last_ns: AtomicI64,
    pub backdrop_gpu_ns: AtomicU32,
    pub blur_error: AtomicU32,
}

/// `LAYER_BLURRING` stays set this long after a blurred present.
const BLURRING_FOR_NS: i64 = 500_000_000;

/// The current `hud.image` reader. `generation` bumps whenever the thread (re)opens or drops
/// it, so the compositor knows to forget what it uploaded.
pub struct Hud {
    pub reader: Option<HudReader<Mapping>>,
    pub generation: u64,
}

pub static HUD: Mutex<Hud> = Mutex::new(Hud { reader: None, generation: 0 });

static TELEMETRY: OnceLock<Option<Telemetry>> = OnceLock::new();

/// The telemetry, once a swapchain made the layer set it up.
#[inline]
pub fn get() -> Option<&'static Telemetry> {
    TELEMETRY.get().and_then(|t| t.as_ref())
}

/// One backdrop blur's GPU time: folded into the running mean the status block reports.
pub fn note_blur_gpu(ns: u64) {
    let Some(t) = get() else { return };
    let ns = ns.min(u32::MAX as u64) as u32;
    let _ = t.backdrop_gpu_ns.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(if old == 0 { ns } else { (old as i64 + (ns as i64 - old as i64) / 8) as u32 })
    });
}

/// Sets up the ring and thread the first time a swapchain is created. `None` when the game
/// wasn't launched by Quiver (`QUIVER_PERF_DIR` unset) or the session dir can't be used:
/// then the layer is a pass-through.
pub fn init() -> Option<&'static Telemetry> {
    TELEMETRY
        .get_or_init(|| {
            let dir = match SessionDir::from_env() {
                Ok(Some(d)) => d,
                Ok(None) => {
                    log!("QUIVER_PERF_DIR not set: pass-through");
                    return None;
                }
                Err(e) => {
                    log!("session dir unusable ({e}): pass-through");
                    return None;
                }
            };
            let pid = std::process::id();
            let ring = match dir.create_layer_ring(pid, quiver_perf::random_nonce()) {
                Ok(r) => r,
                Err(e) => {
                    log!("can't create the layer ring in {}: {e}", dir.path().display());
                    return None;
                }
            };
            ring.heartbeat(mono_raw_ns());
            log!("ring {}", dir.file(&FileKind::Layer { pid }).display());
            Some(Telemetry {
                ring,
                hud_path: dir.file(&FileKind::HudImage),
                status: Mutex::new(LayerStatus::ZERO),
                presents: AtomicU64::new(0),
                drawn: AtomicU64::new(0),
                hud_published: AtomicU64::new(0),
                draw_error: AtomicU32::new(0),
                flags: AtomicU32::new(0),
                keys: AtomicU32::new(0),
                key_toggles: AtomicU32::new(0),
                key_nexts: AtomicU32::new(0),
                key_panels: AtomicU32::new(0),
                focus: AtomicU32::new(0),
                blur_last_ns: AtomicI64::new(0),
                backdrop_gpu_ns: AtomicU32::new(0),
                blur_error: AtomicU32::new(0),
            })
        })
        .as_ref()
        .inspect(|t| {
            let t: &'static Telemetry = t;
            static THREAD: OnceLock<()> = OnceLock::new();
            THREAD.get_or_init(|| {
                let spawned = std::thread::Builder::new().name("quiver-sight".into()).stack_size(256 << 10).spawn(move || run(t));
                if let Err(e) = spawned {
                    log!("no heartbeat thread: {e}");
                }
                keys::start(t);
            });
        })
}

impl Telemetry {
    /// Writes the status block now: the hooks' fields plus the live counters.
    pub fn write_status(&self) {
        let Ok(base) = self.status.lock().map(|s| *s) else { return };
        let mut s = base;
        s.flags |= self.flags.load(Ordering::Relaxed) | keys::SURFACES.load(Ordering::Relaxed) | self.focus.load(Ordering::Relaxed);
        s.presents = self.presents.load(Ordering::Relaxed);
        s.drawn = self.drawn.load(Ordering::Relaxed);
        s.hud_published = self.hud_published.load(Ordering::Relaxed);
        s.draw_error = self.draw_error.load(Ordering::Relaxed);
        s.keys = self.keys.load(Ordering::Relaxed);
        // Wrapping u16s on the wire: quiverd reads every 50 ms, far below 65 536 presses.
        s.key_toggles = self.key_toggles.load(Ordering::Relaxed) as u16;
        s.key_nexts = self.key_nexts.load(Ordering::Relaxed) as u16;
        let now = mono_raw_ns();
        crate::latency::fill_status(&mut s, now);
        s.key_panels = self.key_panels.load(Ordering::Relaxed) as u16;
        // The backdrop blur: readiness from the target swapchain (retarget), unless drawing
        // went off or a present found it can't.
        let err = if !crate::draw_enabled() { BACKDROP_ERR_OFF } else { self.blur_error.load(Ordering::Relaxed) };
        if err != 0 && s.flags & LAYER_BLUR_READY != 0 {
            s.flags &= !LAYER_BLUR_READY;
            s.backdrop_error = err as u16;
        }
        let last = self.blur_last_ns.load(Ordering::Relaxed);
        if last > 0 && now - last < BLURRING_FOR_NS {
            s.flags |= LAYER_BLURRING;
        }
        s.backdrop_gpu_ns = self.backdrop_gpu_ns.load(Ordering::Relaxed);
        // Contention means another thread is writing it this instant; the next tick retries.
        let _ = self.ring.set_layer_status(&s);
    }

    /// Changes the hooks' part of the status and writes it.
    pub fn update_status(&self, f: impl FnOnce(&mut LayerStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
        self.write_status();
    }

    /// The X11 key polling state (`KEYS_*`), written now.
    pub fn set_keys_x11(&self, state: u32) {
        self.set_keys_byte(0, state);
    }

    /// The Wayland keyboard's state (`KEYS_WL_*`), written now.
    pub fn set_keys_wayland(&self, state: u32) {
        self.set_keys_byte(KEYS_WL_SHIFT, state);
    }

    fn set_keys_byte(&self, shift: u32, state: u32) {
        let _ = self.keys.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |k| Some((k & !(0xff << shift)) | ((state & 0xff) << shift)));
        self.write_status();
    }

    /// A HUD chord was pressed: counted and written now, so quiverd's next drain (≤ 50 ms)
    /// sees it. A write lost to contention goes out with the next heartbeat (≤ 250 ms).
    pub fn key_pressed(&self, a: keys::Action) {
        match a {
            keys::Action::Toggle => &self.key_toggles,
            keys::Action::Next => &self.key_nexts,
            keys::Action::Panel => &self.key_panels,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.write_status();
    }

    /// Whether the game's window is in front (`None`: the layer can't tell); written at once
    /// on a change, so quiverd's next drain (≤ 50 ms) sees it.
    pub fn set_focus(&self, f: Option<bool>) {
        let v = match f {
            None => 0,
            Some(false) => LAYER_FOCUS_KNOWN,
            Some(true) => LAYER_FOCUS_KNOWN | LAYER_FOCUSED,
        };
        if self.focus.swap(v, Ordering::Relaxed) != v {
            log!("focus: {}", match f {
                None => "unknown",
                Some(false) => "the game's window isn't in front",
                Some(true) => "the game's window is in front",
            });
            self.write_status();
        }
    }

    pub fn mark_panicked(&self) {
        self.flags.fetch_or(LAYER_PANICKED, Ordering::Relaxed);
    }

    fn tick(&self) {
        let now = mono_raw_ns();
        self.ring.heartbeat(now);
        self.refresh_hud();
        if let Some(dir) = self.hud_path.parent() {
            crate::panel::tick(dir, now);
        }
        crate::latency::poll_timings();
        self.write_status();
    }

    /// Opens `hud.image` when it appears, re-opens it when quiverd replaces it, and drops it
    /// when it's removed or shrinks (a shrunk mapping would fault on read).
    fn refresh_hud(&self) {
        let mut hud = match HUD.lock() {
            Ok(h) => h,
            Err(p) => p.into_inner(),
        };
        let keep = hud.reader.as_ref().is_some_and(|r| {
            let region = r.region();
            let want = HUD_HEADER_BYTES as u64 + 2 * r.info().buffer_bytes as u64;
            region.is_file_at(&self.hud_path) && region.size_now().is_ok_and(|n| n >= want)
        });
        if keep {
            return;
        }
        let had = hud.reader.take().is_some();
        match open_hud_reader(&self.hud_path) {
            Ok(r) => {
                log!("hud.image opened ({} bytes per buffer)", r.info().buffer_bytes);
                hud.reader = Some(r);
                hud.generation += 1;
            }
            Err(_) if had => {
                log!("hud.image gone");
                hud.generation += 1;
            }
            Err(_) => {}
        }
    }
}

fn run(t: &'static Telemetry) {
    loop {
        if std::panic::catch_unwind(|| t.tick()).is_err() {
            t.mark_panicked();
        }
        std::thread::sleep(TICK);
    }
}
