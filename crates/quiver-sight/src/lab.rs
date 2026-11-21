//! `quiver-sight --lab DIR`: Sight in a `quiver lab` instance (design/LAB.md §12), run by the
//! instance's supervisor inside the game's container for each launch.
//!
//! **No quiverd.** In a lab instance nothing the panel does may change the person's settings,
//! so this mode never connects to quiverd (none is reachable from the container either). What
//! quiverd normally provides comes from the instance itself:
//!
//! - **The HUD model** from the layer's ring in the session dir: frames per second, 1% low and
//!   frame time from its present records, the frame-time graph the layer draws itself, and a
//!   line saying it's a lab run. No GPU or CPU sensors (quiverd's), no mod metrics.
//! - **The keys**: Shift+F2 (panel), Shift+F3 (HUD) and Ctrl+Shift+F3 (next preset), from the
//!   layer's key counters in its status block, as quiverd's ring thread reads them.
//! - **The panel's data**: the instance's game and run; Mods, Tweaks and Achievements say
//!   they're read-only in a lab (mod settings go through `quiver lab set`).
//! - **Stereo** (design/SIGHT.md §15): a live mod's word (`quiver_set_stereo_layout`) through
//!   the instance's own read-only mod SDK endpoint, which its supervisor writes into the session
//!   dir (`quiver_api::lab::SDK_STEREO_FILE`); else the launch's `sight.stereo` setting, which
//!   the layer reports from `QUIVER_SIGHT_STEREO` in the game's environment
//!   (`quiver lab edit N env.QUIVER_SIGHT_STEREO=sbs`).
//! - **Actions**: every change the panel would send to quiverd (an option, a Tweak, HUD
//!   settings, quitting the game) is refused, logged, and shown as refused in the panel; showing
//!   or hiding the HUD only changes this process's HUD.
//!
//! Its status (the panel's report, the HUD, frames per second, refused actions) goes to
//! `--lab-status FILE` (`runs/r<N>/sight.json`), its log to its stderr (`runs/r<N>/sight.log`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::GameRef;
use quiver_api::perf::{Anchor, Captures, CellStyle, Emphasis, HudCell, HudModel, HudRow, LayerBackdrop, LayerStereo, Level, Orientation, PerfValue, Presenter, SourceKind, StereoInfo, hud_card};
use quiver_api::sight::{PanelAction, PanelCommand, PanelReport};
use quiver_perf::layout::{LAYER_BLUR_READY, LAYER_BLURRING};
use quiver_perf::record::LayerPresent;
use quiver_perf::session::{FileKind, SessionDir, open_ring_reader};

use crate::link::{Change, LinkReq, Loaded, Msg};
use crate::panel::state::{Load, PanelData, SessionView};

/// `--lab` and its companions.
#[derive(Debug, Clone, Default)]
pub struct LabArgs {
    pub dir: PathBuf,
    pub status: Option<PathBuf>,
    pub title: String,
    pub game: String,
}

/// The HUD presets cycled by Ctrl+Shift+F3.
pub const LAYOUTS: [&str; 3] = ["fps", "compact", "standard"];

/// Why a lab panel can't do something.
pub const READ_ONLY: &str = "read-only in a lab instance: nothing here changes the person's settings";

#[derive(Default)]
struct Shared {
    report: Option<PanelReport>,
    refused: Vec<String>,
    fps: Option<f64>,
    extent: Option<[u32; 2]>,
    stereo: Option<StereoInfo>,
}

struct Lab {
    args: LabArgs,
    session: String,
    tx: UnboundedSender<Msg>,
    hud_on: AtomicBool,
    layout: AtomicUsize,
    /// Bumped by HUD changes from the panel, so the feeder sends a model at once.
    hud_gen: AtomicU64,
    shared: Mutex<Shared>,
}

/// Frame statistics over the newest present times (ns, oldest first).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub fps: f64,
    pub frametime_ms: f64,
    pub p1low: f64,
}

/// FPS over the last second, the mean frame time, and the 1% low (the frame rate of the
/// slowest 1% of intervals) over the window.
pub fn stats(times: &VecDeque<i64>, now: i64) -> Option<Stats> {
    let newest = *times.back()?;
    if now - newest > 1_000_000_000 || times.len() < 3 {
        return None;
    }
    let last_s = times.iter().filter(|&&t| now - t <= 1_000_000_000).count();
    let mut iv: Vec<f64> = times.iter().zip(times.iter().skip(1)).map(|(a, b)| (b - a) as f64 / 1e6).filter(|d| *d > 0.0).collect();
    if iv.is_empty() {
        return None;
    }
    let mean = iv.iter().sum::<f64>() / iv.len() as f64;
    iv.sort_by(|a, b| b.total_cmp(a));
    let n = (iv.len() / 100).max(1);
    let slow = iv[..n].iter().sum::<f64>() / n as f64;
    Some(Stats { fps: last_s as f64, frametime_ms: mean, p1low: 1000.0 / slow })
}

fn v(text: String, unit: &str, level: Level) -> PerfValue {
    PerfValue { value: text.parse().ok(), text, unit: unit.into(), level, stale: false }
}

/// A value from the layer (the lab's only producer), with its emphasis and, for `after`, its
/// label after it ("144 FPS").
fn value(metric: &str, label: Option<&str>, pv: PerfValue, emphasis: Emphasis, after: bool) -> HudCell {
    let style = CellStyle { emphasis, after, source: SourceKind::Layer, source_name: None };
    HudCell::Value { metric: metric.into(), label: label.map(String::from), value: pv, style }
}

/// The lab HUD's model: what the layer's ring says, in the preset's shape.
#[allow(clippy::too_many_arguments)]
pub fn model(session: &str, rev: u64, on: bool, layout: &str, extent: Option<[u32; 2]>, image: &Path, backdrop: Option<LayerBackdrop>, s: Option<Stats>, run: &str, stereo: Option<StereoInfo>) -> HudModel {
    let num = |x: Option<f64>, prec: usize| x.map_or_else(|| "—".to_owned(), |x| format!("{x:.prec$}"));
    let row = |cells: Vec<HudCell>| HudRow { cells, divider: false };
    let block = !matches!(layout, "fps" | "compact" | "clock");
    let fps = value("fps", Some("FPS"), v(num(s.map(|s| s.fps), 0), "", Level::Normal), if block { Emphasis::Hero } else { Emphasis::Strong }, true);
    let low = if block {
        value("fps.p1low", Some("1% low"), v(num(s.map(|s| s.p1low), 0), "", Level::Normal), Emphasis::Quiet, false)
    } else {
        value("fps.p1low", Some("1% LOW"), v(num(s.map(|s| s.p1low), 0), "", Level::Normal), Emphasis::Quiet, true)
    };
    let ft = value("frametime", None, v(num(s.map(|s| s.frametime_ms), 1), "ms", Level::Normal), Emphasis::Quiet, false);
    let tag = HudCell::Text { text: format!("LAB {run}"), level: Level::Dim, style: CellStyle::emphasis(Emphasis::Tag) };
    let (orientation, background, rows) = match layout {
        "fps" => (Orientation::Row, 0.62, vec![row(vec![fps])]),
        "compact" | "clock" => (Orientation::Row, 0.62, vec![row(vec![fps, ft, low, HudCell::Separator, tag])]),
        _ => {
            let graph = HudCell::Graph {
                series: vec!["frametime".into()],
                span_ms: 3000,
                rows: 2,
                min_ms: 0.0,
                max_ms: 0.0,
                warn_above_ms: 33.4,
                bad_above_ms: 50.0,
                points: Vec::new(),
                base_points: Vec::new(),
            };
            (Orientation::Block, 0.62, vec![row(vec![fps, HudCell::Spacer, tag]), row(vec![graph]), row(vec![ft, HudCell::text("·", Level::Dim), low])])
        }
    };
    let width = hud_card::preset_width(layout).unwrap_or(hud_card::default_width(orientation));
    let slots: Vec<f32> = rows.iter().map(hud_card::row_height).collect();
    let size = [width, hud_card::height(orientation, &slots, hud_card::panel(background))];
    HudModel {
        session: session.into(),
        product_id: 0,
        rev,
        on,
        layout: layout.into(),
        name: layout.into(),
        presenter: Presenter::Game,
        presenter_reason: "lab instance: drawn by the Sight layer".into(),
        anchor: Anchor::TopLeft,
        offset: [12.0, 12.0],
        scale: 1.0,
        // 1.6% of one eye's height.
        text_px: extent.map(|[w, h]| stereo.as_ref().map_or(quiver_perf::stereo::Stereo::Mono, |s| s.stereo()).logical_extent((w, h)).1 as f32 * 0.016).unwrap_or(17.0),
        background,
        text_opacity: 1.0,
        level_colors: true,
        orientation,
        safe_area: 0.0,
        image: Some(image.to_path_buf()),
        extent,
        vk_format: None,
        color_space: None,
        monitor: None,
        captures: Captures::Shown,
        size,
        rows,
        backdrop: backdrop.map(Box::new),
        game_focus: None,
        stereo,
    }
}

/// The panel's data in a lab: the instance's game and run, the rest read-only.
pub fn panel_data(session: &str, title: &str, game: &str, started: chrono::DateTime<chrono::Utc>) -> PanelData {
    let ro = || "read-only in a lab instance (mod settings: quiver lab set)".to_owned();
    PanelData {
        session: Load::Ready(SessionView {
            session: session.into(),
            game: GameRef::parse(game).unwrap_or(GameRef::ubisoft(0)),
            name: if title.is_empty() { "Lab instance".into() } else { title.into() },
            profile_name: Some("lab instance".into()),
            started: Some(started),
            copy: format!("quiver lab run {session}"),
            ..Default::default()
        }),
        options: Load::Unavailable(ro()),
        tweaks: Load::Unavailable(ro()),
        tweak_scope_name: "this lab instance".into(),
        hud: Load::Unavailable("the lab HUD: Shift+F3 shows it, Ctrl+Shift+F3 steps the preset".into()),
        achievements: Load::Unavailable("nothing is reported from a lab instance".into()),
        metrics: Default::default(),
        extras: crate::panel::sections::session::Extras { saves: Load::Unavailable(ro()), ..Default::default() },
        prefs: Default::default(),
        readouts: Default::default(),
    }
}

impl Lab {
    fn write_status(&self) {
        let Some(path) = &self.args.status else { return };
        let sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let st = quiver_core_status(self, &sh);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&st).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            tracing::debug!("couldn't write {}", path.display());
        }
    }

    fn refuse(&self, what: String) -> String {
        tracing::info!(%what, "lab: refused ({READ_ONLY})");
        {
            let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            sh.refused.push(what.clone());
            let n = sh.refused.len();
            if n > 50 {
                sh.refused.drain(..n - 50);
            }
        }
        self.write_status();
        format!("{READ_ONLY} ({what})")
    }

    fn panel_toggle(&self, by: &str) {
        let c = PanelCommand { seq: 0, action: PanelAction::Toggle, session: Some(self.session.clone()), section: None, monitor: None, by: by.into(), at: chrono::Utc::now() };
        let _ = self.tx.unbounded_send(Msg::Panel(c));
    }
}

/// The status file's content (`quiver_core::lab::sight::LabSightStatus`'s shape).
fn quiver_core_status(l: &Lab, sh: &Shared) -> serde_json::Value {
    serde_json::json!({
        "pid": std::process::id(),
        "session": l.session,
        "at": chrono::Utc::now(),
        "hud_on": l.hud_on.load(Ordering::Relaxed),
        "hud_layout": LAYOUTS[l.layout.load(Ordering::Relaxed) % LAYOUTS.len()],
        "extent": sh.extent,
        "stereo": sh.stereo,
        "fps": sh.fps,
        "panel": sh.report,
        "refused": sh.refused,
    })
}

/// Starts lab mode: the feeder (the layer's ring → HUD models and keys) and the link stand-in
/// (the panel's requests). Returns at once.
pub fn start(args: LabArgs, tx: UnboundedSender<Msg>, mut req_rx: tokio::sync::mpsc::UnboundedReceiver<LinkReq>) -> Result<()> {
    let dir = SessionDir::open(&args.dir).with_context(|| format!("the lab session dir {}", args.dir.display()))?;
    let session = dir.id().as_str().to_owned();
    tracing::info!(%session, dir = %args.dir.display(), "lab mode: no quiverd; HUD and panel from the instance, actions read-only");
    let lab = Arc::new(Lab { args, session, tx, hud_on: AtomicBool::new(false), layout: AtomicUsize::new(2), hud_gen: AtomicU64::new(0), shared: Mutex::new(Shared::default()) });
    lab.write_status();
    let started = chrono::Utc::now();
    let l = lab.clone();
    std::thread::Builder::new().name("quiver-sight-lab-link".into()).spawn(move || {
        while let Some(r) = req_rx.blocking_recv() {
            match r {
                LinkReq::Load { session, .. } => {
                    let data = panel_data(&l.session, &l.args.title, &l.args.game, started);
                    let _ = l.tx.unbounded_send(Msg::Loaded { session, data: Box::new(Loaded::All(Box::new(data))) });
                }
                LinkReq::SetOption { key, .. } => {
                    let why = l.refuse(format!("set option {} {}", key.0, key.1));
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Option(key.0, key.1), result: Err(why) });
                }
                LinkReq::SetTweak { id, .. } => {
                    let why = l.refuse(format!("set tweak {id}"));
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Tweak(id), result: Err(why) });
                }
                LinkReq::OptionOps { tweaks, why, ops, .. } => {
                    let refused = l.refuse(format!("{} {} change(s) ({why:?})", ops.len(), if tweaks { "tweak" } else { "option" }));
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Ops { tweaks, why }, result: Err(refused) });
                }
                LinkReq::RevertSession { .. } => {
                    let refused = l.refuse("revert the session's options".into());
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Ops { tweaks: false, why: crate::link::Why::Revert }, result: Err(refused) });
                }
                LinkReq::SessionOnly { .. } => {
                    let refused = l.refuse("make the session's options session-only".into());
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Ops { tweaks: false, why: crate::link::Why::SessionOnly }, result: Err(refused) });
                }
                LinkReq::HudSet { .. } => {
                    let why = l.refuse("HUD settings".into());
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Hud, result: Err(why) });
                }
                LinkReq::HudToggle { .. } => {
                    let on = !l.hud_on.fetch_xor(true, Ordering::Relaxed);
                    l.hud_gen.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(on, "lab: HUD shown/hidden from the panel (this instance only)");
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Hud, result: Ok(None) });
                }
                LinkReq::Stop { .. } => {
                    let why = l.refuse("quit the game (use quiver lab kill)".into());
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Stop, result: Err(why) });
                }
                LinkReq::Shot { say, .. } => {
                    let why = l.refuse("screenshot (use quiver lab shot)".into());
                    if say {
                        let _ = l.tx.unbounded_send(Msg::Toast(crate::toasts::shot_toast(Err(why))));
                    }
                }
                LinkReq::Screenshot { .. } => {
                    let why = l.refuse("screenshot (use quiver lab shot)".into());
                    let _ = l.tx.unbounded_send(Msg::Done { change: Change::Screenshot(Default::default()), result: Err(why) });
                }
                LinkReq::Report(r) => {
                    l.shared.lock().unwrap_or_else(|p| p.into_inner()).report = Some(r);
                    l.write_status();
                }
                LinkReq::Toggle => l.panel_toggle("pad"),
                // The Performance section: read-only here like the rest (no quiverd to ask).
                LinkReq::Perf { session, req } => {
                    let reply = crate::panel::sections::performance::link::refused(&req, "not in a lab instance (no quiverd)".into());
                    let _ = l.tx.unbounded_send(Msg::Perf { session, reply });
                }
                LinkReq::Subscribe { .. } | LinkReq::Unsubscribe => {}
                // Sight's settings and notices are the person's quiverd's (design/SIGHT.md §16).
                LinkReq::NoticeAct { id, button } => {
                    let _ = l.refuse(format!("answer notice {id} with {button}"));
                    let _ = l.tx.unbounded_send(Msg::NoticeAnswered(id));
                }
                LinkReq::SettingsSet { key, value } => {
                    let _ = l.refuse(format!("Sight setting {key} = {value}"));
                }
                LinkReq::Pin { scope, pin, on } => {
                    let reason = l.refuse(format!("{} {} in {scope}", if on { "pin" } else { "unpin" }, pin.id));
                    let _ = l.tx.unbounded_send(Msg::PinFailed { scope, reason });
                }
            }
        }
    })?;
    let l = lab.clone();
    std::thread::Builder::new().name("quiver-sight-lab-feed".into()).spawn(move || feed(&l, &dir))?;
    Ok(())
}

/// What a HUD model depends on besides the numbers: shown, preset, extent, blur ready, blurring,
/// stereo.
type Shape = (bool, usize, Option<[u32; 2]>, bool, bool, Option<StereoInfo>);

/// The layer's ring → keys, HUD models, the status file.
fn feed(l: &Lab, dir: &SessionDir) {
    let image = dir.file(&FileKind::HudImage);
    let run = l.session.rsplit('-').next().unwrap_or("").to_owned();
    let mut ring: Option<(PathBuf, quiver_perf::ring::RingReader<quiver_perf::shm::Mapping>)> = None;
    let mut times: VecDeque<i64> = VecDeque::new();
    let mut keys: Option<(u16, u16, u16)> = None;
    let mut rev = 0u64;
    let (mut last_model, mut last_status) = (Instant::now() - Duration::from_secs(10), Instant::now());
    let mut sent_gen = u64::MAX;
    let mut tick = 0u64;
    let mut last_shape: Option<Shape> = None;
    let mut sdk_stereo: Option<quiver_api::lab::SdkStereo> = None;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        tick += 1;
        // The newest layer ring (the game's process; a relaunch makes a new one).
        if ring.as_ref().is_none_or(|(p, _)| !p.exists()) || tick.is_multiple_of(20) {
            let newest = dir.list().ok().and_then(|l| l.into_iter().filter(|(k, _)| matches!(k, FileKind::Layer { .. })).filter_map(|(_, p)| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p))).max().map(|(_, p)| p));
            if let Some(p) = newest
                && ring.as_ref().is_none_or(|(old, _)| *old != p)
                && let Ok(r) = open_ring_reader(&p)
            {
                tracing::info!(ring = %p.display(), "lab: the layer's ring");
                ring = Some((p, r));
                times.clear();
                keys = None;
            }
        }
        // A live mod's stereo word (the instance's mod SDK endpoint writes it), 5 times a second.
        if tick.is_multiple_of(4) {
            let now = quiver_api::lab::SdkStereo::read(dir.path());
            if now != sdk_stereo {
                tracing::info!(stereo = ?now.as_ref().map(|w| (&w.layout, &w.surface)), "lab: the mod SDK's stereo word");
                sdk_stereo = now;
            }
        }
        let Some((_, r)) = ring.as_mut() else { continue };
        let st = r.layer_status();
        let target = st.as_ref().map(|s| s.swapchain).unwrap_or(0);
        r.poll(4096, |_, rec| {
            if let Some(p) = LayerPresent::from_record(rec)
                && (target == 0 || p.swapchain == target)
            {
                times.push_back(p.entry_ns);
            }
        });
        let now = quiver_perf::clock::mono_raw_ns();
        while times.front().is_some_and(|&t| now - t > 3_000_000_000) {
            times.pop_front();
        }
        let Some(st) = st else { continue };
        // The layer's key counters (wrapping u16s): Shift+F2, Shift+F3, Ctrl+Shift+F3.
        let k = (st.key_panels, st.key_toggles, st.key_nexts);
        if let Some((p, t, n)) = keys {
            for _ in 0..k.0.wrapping_sub(p).min(4) {
                tracing::info!("lab: Shift+F2 in the game: panel toggle");
                l.panel_toggle("keys");
            }
            for _ in 0..k.1.wrapping_sub(t).min(4) {
                let on = !l.hud_on.fetch_xor(true, Ordering::Relaxed);
                tracing::info!(on, "lab: Shift+F3 in the game: HUD {}", if on { "shown" } else { "hidden" });
            }
            for _ in 0..k.2.wrapping_sub(n).min(4) {
                let i = l.layout.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(layout = LAYOUTS[i % LAYOUTS.len()], "lab: Ctrl+Shift+F3: next HUD preset");
            }
        }
        keys = Some(k);
        let extent = (st.extent_w > 0 && st.extent_h > 0).then_some([st.extent_w, st.extent_h]);
        let s = stats(&times, now);
        let on = l.hud_on.load(Ordering::Relaxed);
        let layout = l.layout.load(Ordering::Relaxed) % LAYOUTS.len();
        let (ready, blurring) = (st.flags & LAYER_BLUR_READY != 0, st.flags & LAYER_BLURRING != 0);
        let stereo = Some(StereoInfo::resolve(sdk_stereo.as_ref().and_then(|w| w.word()), &LayerStereo::from_status(&st)));
        let shape = (on, layout, extent, ready, blurring, stereo.clone());
        let hud_gen = l.hud_gen.load(Ordering::Relaxed);
        // A new model: on any change of shape, and twice a second while shown (the values).
        if last_shape.as_ref() != Some(&shape) || hud_gen != sent_gen || (on && last_model.elapsed() >= Duration::from_millis(500)) {
            rev += 1;
            let backdrop = LayerBackdrop { image: image.clone(), ready, blurring, reason: if ready { String::new() } else { format!("backdrop error {}", st.backdrop_error) } };
            let m = model(&l.session, rev, on, LAYOUTS[layout], extent, &image, Some(backdrop), s, &run, stereo.clone());
            let _ = l.tx.unbounded_send(Msg::Model(Box::new(m)));
            last_model = Instant::now();
            last_shape = Some(shape);
            sent_gen = hud_gen;
        }
        if last_status.elapsed() >= Duration::from_secs(1) {
            {
                let mut sh = l.shared.lock().unwrap_or_else(|p| p.into_inner());
                sh.fps = s.map(|s| s.fps);
                sh.extent = extent;
                sh.stereo = stereo;
            }
            l.write_status();
            last_status = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_stats() {
        let now = 10_000_000_000i64;
        // 60 fps for 2 s, with one 50 ms hitch.
        let mut t = VecDeque::new();
        let mut x = now - 2_000_000_000;
        while x <= now {
            t.push_back(x);
            x += if t.len() == 60 { 50_000_000 } else { 16_666_667 };
        }
        let s = stats(&t, now).unwrap();
        assert!((s.fps - 60.0).abs() <= 2.0, "{s:?}");
        assert!((s.frametime_ms - 16.9).abs() < 0.6, "{s:?}");
        assert!(s.p1low < 30.0, "the hitch shows in the 1% low: {s:?}");
        // Nothing for a second: no numbers (stale ones would lie).
        assert_eq!(stats(&t, now + 1_500_000_000), None);
        assert_eq!(stats(&VecDeque::new(), now), None);
    }

    #[test]
    fn models_follow_the_presets() {
        let image = Path::new("/s/hud.image");
        let s = Some(Stats { fps: 144.0, frametime_ms: 6.9, p1low: 120.0 });
        for layout in LAYOUTS {
            let m = model("lab-a-r1", 1, true, layout, Some([2560, 1600]), image, None, s, "r1", None);
            assert_eq!(m.presenter, Presenter::Game);
            assert_eq!(m.image.as_deref(), Some(image));
            assert!(m.size[0] > 0.0 && m.size[1] > 0.0);
            let fps = m.rows.iter().flat_map(|r| &r.cells).find_map(|c| match c {
                HudCell::Value { metric, value, .. } if metric == "fps" => Some(value.text.clone()),
                _ => None,
            });
            assert_eq!(fps.as_deref(), Some("144"));
        }
        let m = model("lab-a-r1", 1, true, "standard", None, image, None, None, "r1", None);
        // A stereo setting: the text is sized for one eye (tab on 1920x2160: 1080 tall).
        let st = StereoInfo::resolve(None, &LayerStereo { setting: quiver_perf::stereo::Setting::Fixed(quiver_perf::stereo::Stereo::Tab).code(), drawn: None, ..Default::default() });
        assert_eq!((st.layout.as_str(), st.source.as_str()), ("tab", "setting"));
        let m2 = model("lab-a-r1", 1, true, "standard", Some([1920, 2160]), image, None, None, "r1", Some(st));
        assert!((m2.text_px - 1080.0 * 0.016).abs() < 1e-3);
        assert!(m.rows.iter().flat_map(|r| &r.cells).any(|c| matches!(c, HudCell::Value { value, .. } if value.text == "—")));
    }

    #[test]
    fn panel_data_is_read_only() {
        let d = panel_data("lab-awr-r2", "Alan Wake Remastered", "epic:x", chrono::Utc::now());
        assert!(matches!(d.options, Load::Unavailable(_)) && matches!(d.tweaks, Load::Unavailable(_)));
        let Load::Ready(s) = d.session else { panic!() };
        assert_eq!(s.name, "Alan Wake Remastered");
        assert!(s.copy.contains("lab-awr-r2"));
    }
}
