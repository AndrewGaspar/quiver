//! The layer's side of the session dir (SIGHT-HUD §3.6–3.8): its present ring
//! (`layer-<pid>.ring`) with the status block, the heartbeat thread, and `hud.image`. The
//! in-game HUD keys ([`crate::keys`]) start with it and report through the status block.
//!
//! Everything that makes a syscall (creating the ring, opening or re-opening `hud.image`,
//! stat-ing it) happens at swapchain creation or on the heartbeat thread, never in the
//! present path. The present path only pushes ring records (lock-free), bumps atomics and
//! `try_lock`s the HUD reader.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use quiver_perf::LayerStatus;
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::HudReader;
use quiver_perf::layout::{HUD_HEADER_BYTES, LAYER_PANICKED};
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
    /// In-game HUD keys ([`crate::keys`]): `KEYS_*`, and the presses seen.
    pub keys: AtomicU32,
    pub key_toggles: AtomicU32,
    pub key_nexts: AtomicU32,
}

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
        s.flags |= self.flags.load(Ordering::Relaxed) | keys::SURFACES.load(Ordering::Relaxed);
        s.presents = self.presents.load(Ordering::Relaxed);
        s.drawn = self.drawn.load(Ordering::Relaxed);
        s.hud_published = self.hud_published.load(Ordering::Relaxed);
        s.draw_error = self.draw_error.load(Ordering::Relaxed);
        s.keys = self.keys.load(Ordering::Relaxed);
        // Wrapping u16s on the wire: quiverd reads every 50 ms, far below 65 536 presses.
        s.key_toggles = self.key_toggles.load(Ordering::Relaxed) as u16;
        s.key_nexts = self.key_nexts.load(Ordering::Relaxed) as u16;
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

    /// The key polling state (`KEYS_*`), written now.
    pub fn set_keys(&self, state: u32) {
        self.keys.store(state, Ordering::Relaxed);
        self.write_status();
    }

    /// A HUD chord was pressed: counted and written now, so quiverd's next drain (≤ 50 ms)
    /// sees it. A write lost to contention goes out with the next heartbeat (≤ 250 ms).
    pub fn key_pressed(&self, a: keys::Action) {
        match a {
            keys::Action::Toggle => &self.key_toggles,
            keys::Action::Next => &self.key_nexts,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.write_status();
    }

    pub fn mark_panicked(&self) {
        self.flags.fetch_or(LAYER_PANICKED, Ordering::Relaxed);
    }

    fn tick(&self) {
        self.ring.heartbeat(mono_raw_ns());
        self.refresh_hud();
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
