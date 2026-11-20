//! `--demo`: the UI without quiverd, fed with sample models and toasts (for screenshots and
//! for trying the surfaces on a desktop). Everything closes after `--seconds`.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::perf::{Anchor, Captures, CellStyle, Emphasis, HudCell, HudModel, HudRow, Level, Orientation, PerfValue, Presenter, SourceKind, SplitPart, hud_card};

use crate::Args;
use crate::link::Msg;
use crate::toasts::{ToastData, ToastKind};

fn v(text: &str, unit: &str, level: Level) -> PerfValue {
    PerfValue { value: text.parse().ok(), text: text.into(), unit: unit.into(), level, stale: false }
}

fn st(source: SourceKind, emphasis: Emphasis) -> CellStyle {
    let source_name = (source == SourceKind::Mod).then(|| "pharos".to_owned());
    CellStyle { emphasis, source, source_name, after: false }
}

/// A value cell as quiverd sends it.
fn cell(metric: &str, label: Option<&str>, pv: PerfValue, style: CellStyle) -> HudCell {
    HudCell::Value { metric: metric.into(), label: label.map(String::from), value: pv, style }
}

/// "118 FPS": a label after the value.
fn after(metric: &str, label: &str, pv: PerfValue, style: CellStyle) -> HudCell {
    cell(metric, Some(label), pv, CellStyle { after: true, ..style })
}

fn dim(text: &str) -> HudCell {
    HudCell::text(text, Level::Dim)
}

/// Sample frame times (ms), oldest first, with a hitch.
pub fn frametimes(i: u32, n: usize) -> Vec<f32> {
    (0..n).map(|k| {
        let k = k + i as usize * 7;
        let spike = if k % 97 == 40 { 26.0 } else if k % 23 == 5 { 3.0 } else { 0.0 };
        6.3 + ((k * 7) % 5) as f32 * 0.3 + spike
    }).collect()
}

/// The rendered frames under [`frametimes`] at 2× frame generation: one per two presents.
pub fn base_frametimes(i: u32, n: usize) -> Vec<f32> {
    frametimes(i, n).chunks(2).map(|c| c.iter().sum()).collect()
}

/// The presets the demo has models for, in the cycle's order.
pub const LAYOUTS: [&str; 6] = ["fps", "compact", "standard", "full", "diagnostics", "clock"];

/// A model like quiverd's for `layout` (its built-in presets), with values that move with
/// `i`: frame generation on, a mod ("pharos") supplying PC latency, Reflex's mode and the
/// latency split through the SDK, the base FPS from the Sight layer's Reflex counting. The
/// `short` variant ([`vary`]) is the same game without the mod and FG ("Vanilla").
pub fn sample_model(layout: &str, i: u32, presenter: Presenter, extent: Option<[u32; 2]>) -> HudModel {
    use Emphasis::*;
    use SourceKind::{Layer, Mod, Quiver, System};
    let w = [0i32, 3, -2, 5, -4, 1, 2, -1][i as usize % 8];
    let fps = (158 + w).to_string();
    let base = ((158 + w) / 2).to_string();
    let n = |x: &str| v(x, "", Level::Normal);
    let desktop = presenter == Presenter::Desktop;
    let graph = HudCell::Graph {
        series: vec!["frametime".into(), "frametime.base".into()],
        span_ms: 3000,
        rows: 2,
        min_ms: 0.0,
        max_ms: 0.0,
        warn_above_ms: 33.4,
        bad_above_ms: 50.0,
        points: if desktop { frametimes(i, 300) } else { Vec::new() },
        base_points: if desktop { base_frametimes(i, 300) } else { Vec::new() },
    };
    let gpu = |vram: bool| {
        let mut r = vec![
            cell("gpu.load", Some("GPU"), v(&(97 - w.abs()).to_string(), "%", Level::Warn), st(System, Normal)),
            cell("gpu.temp", None, v("71", "°", Level::Normal), st(System, Quiet)),
            cell("gpu.power", None, v(&(92 + w).to_string(), "W", Level::Normal), st(System, Quiet)),
        ];
        if vram {
            r.push(cell("gpu.vram.used", None, v("6.1", "GB", Level::Normal), st(System, Quiet)));
        }
        r
    };
    let cpu = || vec![cell("cpu.load", Some("CPU"), v(&(38 - w).to_string(), "%", Level::Normal), st(System, Normal)), cell("cpu.temp", None, v("64", "°", Level::Normal), st(System, Quiet))];
    let fg_tag = || cell("fg.ratio", Some("FG"), v("2.0", "×", Level::Normal), st(Layer, Tag));
    let split = |numbers: bool| HudCell::Split {
        numbers,
        label: None,
        parts: [("latency.split.sim", "SIM", 6.0), ("latency.split.render", "RENDER", 9.0), ("latency.split.queue", "QUEUE", 4.0), ("latency.split.gpu", "GPU", 9.0)]
            .iter()
            .map(|(m, l, x)| SplitPart { metric: (*m).into(), label: (*l).into(), value: *x })
            .collect(),
        style: st(Mod, Normal),
    };
    let row = |cells: Vec<HudCell>| HudRow { cells, divider: false };
    let (orientation, rows) = match layout {
        "fps" => (Orientation::Row, vec![row(vec![after("fps", "FPS", n(&fps), st(Layer, Strong)), after("fps.base", "BASE", n(&base), st(Layer, Strong))])]),
        "compact" => {
            let mut r = vec![
                after("fps", "FPS", n(&fps), st(Layer, Strong)),
                after("fps.base", "BASE", n(&base), st(Layer, Strong)),
                cell("frametime", None, v("6.3", "ms", Level::Normal), st(Layer, Quiet)),
                after("fps.p1low", "1% LOW", n("128"), st(Layer, Quiet)),
                after("latency.pc", "LAT", v("28", "ms", Level::Normal), st(Mod, Normal)),
                HudCell::Separator,
            ];
            r.extend(gpu(false));
            r.push(HudCell::Separator);
            r.extend(cpu());
            r.push(HudCell::Separator);
            r.push(HudCell::Badge { metric: "upscaler".into(), label: None, value: v("DLSS K", "", Level::Accent), style: st(Layer, Normal) });
            r.push(fg_tag());
            (Orientation::Row, vec![row(r)])
        }
        "standard" | "full" => {
            let full = layout == "full";
            let mut lat = vec![after("latency.pc", if full { "PC LAT" } else { "PC LATENCY" }, v("28", "ms", Level::Normal), st(Mod, Normal))];
            if full {
                lat.push(after("latency.render", "RENDER", v("21", "ms", Level::Normal), st(Mod, Quiet)));
            }
            lat.extend([HudCell::Spacer, cell("nv.reflex.mode", Some("REFLEX"), v("boost", "", Level::Normal), st(Mod, Tag))]);
            let mut rows = vec![
                row(vec![after("fps", "FPS", n(&fps), st(Layer, Hero)), HudCell::Spacer, after("fps.base", "BASE", n(&base), st(Layer, Secondary))]),
                row(vec![graph]),
                row(vec![
                    cell("frametime", None, v("6.3", "ms", Level::Normal), st(Layer, Quiet)),
                    dim("·"),
                    cell("fps.p1low", Some("1% low"), n("128"), st(Layer, Quiet)),
                    HudCell::Spacer,
                    fg_tag(),
                ]),
                row(lat),
                row(vec![split(false)]),
            ];
            if full {
                rows.push(row(vec![split(true)]));
                rows.push(row(gpu(true)));
                rows.push(row([cpu(), vec![cell("mem.used", Some("RAM"), v("11.2", "GB", Level::Normal), st(System, Quiet))]].concat()));
                rows.push(row(vec![
                    cell("display.vrr", Some("VRR"), n("on"), st(System, Normal)),
                    cell("display.tearing", Some("TEAR"), v("off", "", Level::Dim), st(System, Normal)),
                    cell("display.refresh", None, v("165", "Hz", Level::Normal), st(System, Quiet)),
                    cell("nv.reflex.cap", Some("CAP"), v("120", "fps", Level::Normal), st(Layer, Normal)),
                ]));
                rows.push(row(vec![
                    HudCell::Badge { metric: "upscaler".into(), label: None, value: v("DLSS K", "", Level::Accent), style: st(Layer, Normal) },
                    cell("render.resolution", None, n("1280×800"), st(Layer, Quiet)),
                    dim("→"),
                    cell("output.resolution", None, n("2560×1600"), st(Layer, Quiet)),
                ]));
                rows.push(row(vec![cell("dlss.sr.quality", Some("SR"), n("Perf"), st(Layer, Normal)), cell("dlss.sr.model", None, n("Transformer"), st(Layer, Quiet))]));
                // The mod's strip, from the metrics it declares.
                let m = |id: &str, label: Option<&str>, text: &str| cell(id, label, n(text), st(Mod, Quiet));
                rows.push(HudRow {
                    cells: vec![
                        HudCell::Text { text: "PHAROS".into(), level: Level::Normal, style: st(Mod, Tag) },
                        m("pharos.render", None, "1280×800"),
                        dim("·"),
                        m("pharos.model", Some("model"), "M"),
                        dim("·"),
                        m("pharos.fg", Some("FG"), "on"),
                        dim("·"),
                        m("pharos.pacing", Some("pacing"), "on"),
                    ],
                    divider: true,
                });
            } else {
                rows.push(row([gpu(false), cpu()].concat()));
            }
            (Orientation::Block, rows)
        }
        "diagnostics" => (
            Orientation::Block,
            vec![
                row(vec![
                    after("fps", "FPS", n(&fps), st(Layer, Strong)),
                    cell("frametime", None, v("6.3", "ms", Level::Normal), st(Layer, Quiet)),
                    HudCell::Spacer,
                    cell("session.time", Some("TIME"), n("1:12:03"), st(Quiver, Normal)),
                    cell("clock.time", None, n("14:52"), st(Quiver, Quiet)),
                ]),
                row(vec![cell("gpu.clock", Some("GPU"), v("1980", "MHz", Level::Normal), st(System, Normal)), cell("cpu.clock", Some("CPU"), v("4210", "MHz", Level::Normal), st(System, Normal))]),
                row(vec![
                    cell("gpu.vram.used", Some("VRAM"), v("6.1", "GB", Level::Normal), st(System, Normal)),
                    cell("gpu.vram.total", Some("of"), v("8.0", "GB", Level::Normal), st(System, Quiet)),
                    cell("mem.used", Some("RAM"), v("11.2", "GB", Level::Normal), st(System, Normal)),
                ]),
                row(vec![cell("proc.rss", Some("GAME"), v("4.3", "GB", Level::Normal), st(System, Normal)), cell("proc.cpu", None, v("212", "%", Level::Normal), st(System, Quiet))]),
                row(vec![
                    cell("bat.level", Some("BAT"), v("84", "%", Level::Normal), st(System, Normal)),
                    cell("bat.power", None, v("41", "W", Level::Normal), st(System, Quiet)),
                    HudCell::Badge { metric: "bat.status".into(), label: None, value: v("battery", "", Level::Warn), style: st(System, Normal) },
                ]),
                row(vec![
                    cell("display.mode", Some("DISP"), n("2560×1600@165"), st(System, Normal)),
                    cell("display.vrr", Some("VRR"), n("on"), st(System, Normal)),
                    cell("display.tearing", Some("TEAR"), v("off", "", Level::Dim), st(System, Normal)),
                ]),
                row(vec![cell("display.path", Some("PATH"), n("composited"), st(System, Normal)), cell("display.blocked", Some("blocked by"), n("overlays"), st(System, Quiet))]),
                row(vec![
                    cell("dlss.version", Some("DLL"), n("310.9.16"), st(Layer, Normal)),
                    cell("dlss.fg.version", Some("FG"), n("310.9.16"), st(Layer, Normal)),
                    cell("dlss.sr.requested", Some("REQ"), v("K", "", Level::Dim), st(Layer, Normal)),
                ]),
                row(vec![cell("tweaks.summary", Some("TWK"), n("CAP 160 · DLSS K · LL on"), st(SourceKind::Tweaks, Normal))]),
            ],
        ),
        _ => (
            Orientation::Row,
            vec![row(vec![
                cell("clock.time", None, n("14:52"), st(Quiver, Normal)),
                HudCell::Separator,
                cell("session.time", None, n("1:12:03"), st(Quiver, Normal)),
                HudCell::Separator,
                cell("bat.level", Some("BAT"), v("84", "%", Level::Normal), st(System, Normal)),
            ])],
        ),
    };
    let background = 0.62;
    let text_px = extent.map(|[_, h]| h as f32 * 0.016).unwrap_or(17.0);
    // As quiverd sizes the presets: their width, and the height of the rows shown.
    let width = hud_card::preset_width(layout).unwrap_or(hud_card::default_width(orientation));
    let size = [width, fit_height(orientation, &rows, background)];
    HudModel {
        session: "s-demo".into(),
        product_id: 0,
        rev: i as u64 + 1,
        on: true,
        layout: layout.into(),
        name: layout.into(),
        presenter,
        presenter_reason: "demo".into(),
        anchor: Anchor::TopLeft,
        offset: [12.0, 12.0],
        scale: 1.0,
        text_px,
        background,
        text_opacity: 1.0,
        level_colors: true,
        orientation,
        safe_area: 0.0,
        image: None,
        extent,
        vk_format: Some(44),
        color_space: Some(0),
        monitor: None,
        captures: Captures::Shown,
        size,
        rows,
        backdrop: None,
        game_focus: None,
        stereo: None,
    }
}

/// The card's height for these rows, as quiverd fits it (`hud::fit_size`).
pub fn fit_height(o: Orientation, rows: &[HudRow], background: f32) -> f32 {
    let slots: Vec<f32> = rows.iter().map(hud_card::row_height).collect();
    hud_card::height(o, &slots, hud_card::panel(background))
}

/// Demo content variants (`--variant`), for checking the card's size: `long` stretches every
/// value (4-digit fps, long text values and badges) and keeps the size, `short` is what
/// quiverd sends for the same game without the mod and frame generation ("Standard, Vanilla
/// profile": no base FPS, no latency, no split, no strip; one graph line), in a card fitted to
/// those rows as a session of that game gets.
pub const VARIANTS: [&str; 3] = ["base", "long", "short"];

pub fn vary(mut m: HudModel, variant: &str) -> HudModel {
    match variant {
        "long" => {
            for r in &mut m.rows {
                for c in &mut r.cells {
                    let id = c.metric().unwrap_or_default().to_owned();
                    let (HudCell::Value { value, .. } | HudCell::Badge { value, .. } | HudCell::Bar { value, .. } | HudCell::Sparkline { value, .. }) = c else { continue };
                    let text = match id.as_str() {
                        "fps" => "1234",
                        "fps.base" => "617",
                        "fps.p1low" => "1012",
                        "frametime" | "frametime.base" => "112.7",
                        "gpu.power" => "312",
                        "gpu.temp" => "101",
                        "upscaler" => "DLSS Ultra Performance K",
                        "render.resolution" => "3840×2160",
                        "output.resolution" => "7680×4320",
                        "display.mode" => "7680×4320@240",
                        "display.path" => "composited",
                        "display.blocked" => "overlays, a notification, the cursor",
                        "latency.render" => "1021",
                        "latency.pc" => "1002.5",
                        "dlss.sr.model" => "Transformer (DLSS 4.5)",
                        "dlss.version" => "310.10.123.4",
                        "dlss.sr.requested" => "latest · Ultra Perf",
                        "nv.reflex.cap" => "1000",
                        "pharos.render" => "3840×2160 (DLAA off)",
                        "tweaks.summary" => "CAP 160 · DLSS K · LL on · HDR on · FSYNC",
                        _ => continue,
                    };
                    value.text = text.into();
                    value.value = text.parse().ok();
                }
            }
        }
        "short" => {
            // No mod and no frame generation: base FPS, the FG tag, the mod's latency and its
            // split and strip go, and the graph has one series.
            let gone = ["fps.base", "frametime.base", "fg.ratio", "upscaler", "render.resolution", "output.resolution", "dlss.sr.quality", "dlss.sr.model"];
            let is_content = |c: &HudCell| !matches!(c, HudCell::Text { .. } | HudCell::Separator | HudCell::Spacer);
            m.rows.retain(|r| !r.divider);
            for r in &mut m.rows {
                r.cells.retain(|c| {
                    let gone_metric = c.metric().is_some_and(|id| gone.contains(&id));
                    let modded = c.style().is_some_and(|s| s.source == SourceKind::Mod);
                    !gone_metric && !modded && !matches!(c, HudCell::Split { .. })
                });
                for c in &mut r.cells {
                    if let HudCell::Graph { series, base_points, .. } = c {
                        series.retain(|s| s == "frametime");
                        base_points.clear();
                    }
                }
                // Tidy as quiverd does: no "·" without a value each side, nothing trailing.
                let keep: Vec<bool> = (0..r.cells.len())
                    .map(|i| {
                        !matches!(&r.cells[i], HudCell::Text { text, .. } if text == "·" || text == "→")
                            || (i > 0 && is_content(&r.cells[i - 1]) && r.cells.get(i + 1).is_some_and(is_content))
                    })
                    .collect();
                let mut keep = keep.into_iter();
                r.cells.retain(|_| keep.next().unwrap_or(true));
                while matches!(r.cells.last(), Some(HudCell::Separator | HudCell::Spacer)) {
                    r.cells.pop();
                }
            }
            m.rows.retain(|r| r.cells.iter().any(is_content));
            m.size[1] = fit_height(m.orientation, &m.rows, m.background);
        }
        _ => {}
    }
    m
}

pub fn sample_toasts() -> Vec<ToastData> {
    vec![
        ToastData {
            kind: ToastKind::Achievement,
            caption: "Achievement unlocked".into(),
            title: "Mysteries of Egypt".into(),
            body: Some("Find every tomb in the Giza necropolis.".into()),
            image: None,
            level: Level::Accent,
            duration: Duration::from_secs(6),
        },
        ToastData {
            kind: ToastKind::Options,
            caption: "Mod setting".into(),
            title: "Applied live".into(),
            body: Some("DLSS fg.enabled off → on".into()),
            image: None,
            level: Level::Good,
            duration: Duration::from_secs(4),
        },
        ToastData {
            kind: ToastKind::Options,
            caption: "Mod setting".into(),
            title: "Saved · applies at swapchain creation".into(),
            body: Some("DLSS fg.mode d3d11 → d3d12".into()),
            image: None,
            level: Level::Accent,
            duration: Duration::from_secs(5),
        },
    ]
}

/// Feeds the UI from a thread, as the quiverd link would.
pub fn feed(demo: &str, a: &Args, tx: UnboundedSender<Msg>) -> Result<()> {
    let seconds = a.seconds;
    let layout = a.layout.clone();
    let variant = a.variant.clone();
    let extent = a.extent;
    let image = a.image.clone();
    if demo == "image" {
        let p = image.clone().context("--demo image needs --image PATH")?;
        let bytes = quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap();
        quiver_perf::session::create_hud_file(&p, bytes).with_context(|| format!("creating {}", p.display()))?;
    } else if demo == "panel" {
        feed_panel(a, &tx);
    } else if !matches!(demo, "toasts" | "desktop") {
        anyhow::bail!("--demo toasts|desktop|image|panel");
    }
    let demo = demo.to_owned();
    std::thread::spawn(move || {
        let send = |m: Msg| tx.unbounded_send(m).is_ok();
        let start = std::time::Instant::now();
        match demo.as_str() {
            "toasts" => {
                for t in sample_toasts() {
                    send(Msg::Toast(t));
                    std::thread::sleep(Duration::from_millis(700));
                }
            }
            // The panel closes after `--seconds` (or with B / Escape / Shift+F2).
            "panel" => {}
            "desktop" => {
                let mut i = 0;
                while start.elapsed().as_secs_f32() < seconds {
                    send(Msg::Model(Box::new(vary(sample_model(&layout, i, Presenter::Desktop, None), &variant))));
                    i += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            _ => {
                let mut m = vary(sample_model(&layout, 0, Presenter::Game, Some([extent.0, extent.1])), &variant);
                m.image = image;
                send(Msg::Model(Box::new(m)));
                // The session ends before the process does, as with quiverd.
                std::thread::sleep(Duration::from_secs_f32((seconds - 0.5).max(0.2)));
                send(Msg::Ended("s-demo".into()));
            }
        }
        let left = seconds - start.elapsed().as_secs_f32();
        if left > 0.0 {
            std::thread::sleep(Duration::from_secs_f32(left));
        }
        send(Msg::Quit("demo done".into()));
    });
    Ok(())
}

// ---- The panel (`--demo panel`, the `views_png` example) --------------------------------

/// A running AC Origins session with the DLSS profile, as quiverd would describe it: the
/// panel's fixture.
pub fn sample_panel() -> crate::panel::state::PanelData {
    use crate::panel::state::{Ach, Achs, HudState, Load, PanelData, SessionView};
    use quiver_api::options::{Apply, EnumValue, GroupState, LiveChannel, LiveInfo, Mark, OptionDef, OptionState, OptionType, PageState, Pending, PresetInfo, SurfaceFile, SurfaceKind, SurfaceState};
    use quiver_api::perf::{HudConfig, HudSettingsView, HudStatus, PerfValue, Stat};
    use serde_json::json;
    let now = chrono::Utc::now();
    let opt = |ty: OptionType, label: &str, help: Option<&str>, apply: Apply, value: serde_json::Value| OptionState {
        def: OptionDef { ty, label: Some(label.into()), help: help.map(String::from), apply, ..Default::default() },
        value: value.clone(),
        default: value,
        visible: true,
        ..Default::default()
    };
    let choices = |vals: &[(&str, &str)]| vals.iter().map(|(v, l)| EnumValue { value: json!(v), label: Some((*l).into()), ..Default::default() }).collect::<Vec<_>>();
    let mut dlss = SurfaceState {
        key: "m1/aco-dlss".into(),
        kind: SurfaceKind::Mod,
        surface: "aco-dlss".into(),
        title: "DLSS".into(),
        version_label: Some("v3".into()),
        live: LiveInfo { channel: LiveChannel::Sdk, connected: true, session: Some("s-demo".into()) },
        ..Default::default()
    };
    let mut up = opt(OptionType::Enum, "Upscaler", Some("What renders the frame"), Apply::Live, json!("dlss"));
    up.def.values = choices(&[("dlss", "DLSS"), ("fsr", "FSR 3"), ("taa", "TAA (the game's)")]);
    dlss.options.insert("upscale.mode".into(), up);
    let mut preset = opt(OptionType::Enum, "DLSS preset", None, Apply::Live, json!("k"));
    preset.def.values = choices(&[("j", "J"), ("k", "K (transformer)"), ("m", "M (DLSS 4.5)")]);
    dlss.options.insert("upscale.preset".into(), preset);
    let mut scale = opt(OptionType::Float, "Render scale", Some("Of the output resolution"), Apply::Restart, json!(0.5));
    (scale.def.min, scale.def.max, scale.def.step, scale.def.precision) = (Some(0.33), Some(1.0), Some(0.01), Some(2));
    scale.pending = Pending::Restart;
    dlss.options.insert("upscale.scale".into(), scale);
    dlss.options.insert("fg.enabled".into(), opt(OptionType::Bool, "Frame generation", Some("F9 in game"), Apply::Live, json!(true)));
    // Its toggle key (not on a page: the Session section's Frame generation toggle names it).
    let mut fgkey = opt(OptionType::Key, "Toggle key", None, Apply::Live, json!("F9"));
    fgkey.def.allowed = Some("F1-F24".into());
    dlss.options.insert("fg.toggle_key".into(), fgkey);
    let mut fgmode = opt(OptionType::Enum, "Frame generation path", None, Apply::At, json!("d3d12"));
    fgmode.def.at = Some("swapchain creation (the game makes its swapchain at startup and on mode changes)".into());
    fgmode.def.values = choices(&[("d3d11", "D3D11"), ("d3d12", "D3D12 (vkd3d)")]);
    dlss.options.insert("fg.mode".into(), fgmode);
    let mut npc = opt(OptionType::Float, "Crowd density", Some("Raises the engine's cap on people near you. Costs CPU in cities."), Apply::Live, json!(2.0));
    (npc.def.min, npc.def.max, npc.def.step, npc.def.precision, npc.def.unit) = (Some(1.0), Some(5.0), Some(0.1), Some(1), Some("×".into()));
    npc.default = json!(1.0);
    npc.def.default = Some(json!(1.0));
    dlss.options.insert("world.npc_density".into(), npc);
    let mut spawn = opt(OptionType::Float, "Crowd spawn distance", Some("How far away people appear. Experimental."), Apply::Live, json!(1.0));
    (spawn.def.min, spawn.def.max, spawn.def.step, spawn.def.precision, spawn.def.unit) = (Some(1.0), Some(3.0), Some(0.1), Some(1), Some("×".into()));
    dlss.options.insert("world.spawn".into(), spawn);
    let mut lod = opt(OptionType::Float, "Draw distance", Some("Multiplies the level-of-detail distance."), Apply::At, json!(1.5));
    (lod.def.min, lod.def.max, lod.def.step, lod.def.precision, lod.def.unit) = (Some(1.0), Some(2.75), Some(0.25), Some(1), Some("×".into()));
    lod.def.at = Some("next area load (the game streams areas in as you travel)".into());
    lod.default = json!(1.0);
    dlss.options.insert("world.lod".into(), lod);
    let mut cloth = opt(OptionType::Enum, "Cloth simulation", Some("Every frame: never slower than the game; costs CPU in crowds."), Apply::Live, json!("every"));
    cloth.def.values = choices(&[("every", "Every frame"), ("half", "Every other frame"), ("off", "Off")]);
    dlss.options.insert("world.cloth".into(), cloth);
    let mut handles = opt(OptionType::Bool, "Hide 64-bit image view handles", None, Apply::Restart, json!(false));
    handles.def.danger = Some("some drivers crash with it".into());
    dlss.options.insert("compat.handles".into(), handles);
    let mut cap = opt(OptionType::Int, "Frame cap", None, Apply::Live, json!(0));
    (cap.def.min, cap.def.max, cap.def.step) = (Some(30.0), Some(240.0), Some(1.0));
    cap.def.special = vec![Mark { value: json!(0), label: "No cap".into() }];
    dlss.options.insert("pace.cap".into(), cap);
    let mut reset = opt(OptionType::Bool, "Rebuild shader cache", None, Apply::Restart, json!(false));
    reset.def.danger = Some("the next start takes several minutes".into());
    dlss.options.insert("debug.rebuild".into(), reset);
    let group = |title: &str, ids: &[&str]| GroupState { title: title.into(), options: ids.iter().map(|s| s.to_string()).collect() };
    let page = |id: &str, title: &str, groups: Vec<GroupState>| PageState { id: id.into(), title: title.into(), groups };
    dlss.pages = vec![
        page("upscaling", "Upscaling", vec![group("", &["upscale.mode", "upscale.preset", "upscale.scale"])]),
        page("latency", "Latency", vec![group("", &["pace.cap"])]),
        page("fg", "Frame Generation", vec![group("", &["fg.enabled", "fg.mode"])]),
        page("world", "World", vec![group("", &["world.npc_density", "world.spawn", "world.lod", "world.cloth", "debug.rebuild", "compat.handles"])]),
    ];
    dlss.presets = vec![
        PresetInfo { id: "busy".into(), label: "Busy cities".into(), help: None, set: [("world.npc_density".to_owned(), json!(2.0)), ("world.spawn".to_owned(), json!(1.0))].into() },
        PresetInfo { id: "default".into(), label: "The game's".into(), help: None, set: [("world.npc_density".to_owned(), json!(1.0)), ("world.spawn".to_owned(), json!(1.0))].into() },
    ];
    dlss.metrics = ["aco-dlss.fg", "aco-dlss.res", "aco-dlss.fps", "aco-dlss.reflex"].map(String::from).to_vec();
    // The game's own settings and DXVK: next-launch surfaces.
    let mut game = SurfaceState { key: "game/aco-ini".into(), surface: "aco-ini".into(), title: "Game settings".into(), kind: SurfaceKind::Game, ..Default::default() };
    game.files = vec![SurfaceFile { rel: "Documents/ACO.ini".into(), ..Default::default() }];
    let mut res = opt(OptionType::Enum, "Resolution", None, Apply::Launch, json!("2560x1600"));
    res.def.values = choices(&[("1920x1200", "1920 × 1200"), ("2560x1600", "2560 × 1600")]);
    game.options.insert("display.res".into(), res);
    game.options.insert("display.vsync".into(), opt(OptionType::Bool, "V-sync", Some("The game's own; Quiver's frame cap is in Tweaks"), Apply::Launch, json!(false)));
    game.pages = vec![page("display", "Display", vec![group("", &["display.res", "display.vsync"])])];
    let mut dxvk = SurfaceState { key: "runtime/dxvk".into(), surface: "dxvk".into(), title: "DXVK".into(), kind: SurfaceKind::Runtime, ..Default::default() };
    dxvk.files = vec![SurfaceFile { rel: "dxvk.conf".into(), ..Default::default() }];
    let mut lat = opt(OptionType::Int, "Maximum frame latency", Some("Frames the CPU may queue ahead of the GPU"), Apply::Launch, json!(0));
    (lat.def.min, lat.def.max, lat.def.step) = (Some(1.0), Some(16.0), Some(1.0));
    lat.def.special = vec![Mark { value: json!(0), label: "The game's".into() }];
    dxvk.options.insert("dxgi.maxFrameLatency".into(), lat);
    dxvk.pages = vec![page("dxvk", "DXVK", vec![group("", &["dxgi.maxFrameLatency"])])];
    let mut tweaks = SurfaceState { key: "quiver/tweaks".into(), surface: "tweaks".into(), title: "Quiver Tweaks".into(), kind: SurfaceKind::Platform, ..Default::default() };
    let mut lsfg = opt(OptionType::Enum, "Lossless Scaling FG", Some("Generated frames between the game's own (lsfg-vk). Adds latency."), Apply::Live, json!("2"));
    lsfg.def.values = choices(&[("off", "Off"), ("2", "2×"), ("3", "3×"), ("4", "4×")]);
    lsfg.default = json!("off");
    tweaks.options.insert("lsfg.multiplier".into(), lsfg);
    let mut flow = opt(OptionType::Float, "Flow scale", Some("Lower is faster; higher keeps fine motion cleaner."), Apply::Live, json!(0.8));
    (flow.def.min, flow.def.max, flow.def.step, flow.def.precision) = (Some(0.25), Some(1.0), Some(0.05), Some(2));
    tweaks.options.insert("lsfg.flow".into(), flow);
    tweaks.options.insert("lsfg.performance".into(), opt(OptionType::Bool, "Performance mode", None, Apply::Live, json!(false)));
    let mut dfg = opt(OptionType::Enum, "DLSS Frame Generation", Some("Can't run with Lossless Scaling FG; turning it on turns that off."), Apply::Launch, json!("off"));
    dfg.def.values = choices(&[("off", "Off"), ("on", "On")]);
    tweaks.options.insert("dlss.fg".into(), dfg);
    let mut tcap = opt(OptionType::Int, "Frame-rate cap", None, Apply::Launch, json!(120));
    (tcap.def.min, tcap.def.max, tcap.def.step) = (Some(30.0), Some(240.0), Some(1.0));
    tcap.pending = Pending::Restart;
    // The DLSS mod provides it (Reflex's limiter): the Session section's quick toggle says so.
    tcap.tweak = Some(Box::new(quiver_api::tweaks::TweakRow {
        planned: Some(quiver_api::tweaks::Planned::Provided { by: "DLSS".into(), surface: "m1/aco-dlss".into(), option: "reflex.fps_limit".into(), value: None, mode: "bind".into(), can_prefer_quiver: true }),
        ..Default::default()
    }));
    tweaks.options.insert("fps.cap".into(), tcap);
    tweaks.options.insert("dxvk.async".into(), opt(OptionType::Bool, "Asynchronous shader compiles", Some("Fewer stutters the first time effects show up"), Apply::Launch, json!(true)));
    tweaks.options.insert("nvidia.reflex".into(), opt(OptionType::Bool, "NVIDIA Reflex", Some("Lower latency on RTX cards"), Apply::Launch, json!(true)));
    let mut hdr = opt(OptionType::Enum, "HDR", None, Apply::Launch, json!("off"));
    hdr.def.values = choices(&[("off", "Off"), ("on", "On (Wayland)")]);
    tweaks.options.insert("display.hdr".into(), hdr);
    let mut fsync = opt(OptionType::Bool, "Fsync", None, Apply::Launch, json!(true));
    fsync.def.help = Some("Faster Wine synchronisation on this kernel".into());
    tweaks.options.insert("wine.fsync".into(), fsync);
    // The frame-rate cap, which the DLSS mod implements (its Reflex limiter): the
    // Performance section says who enforces it.
    if let Some(cap) = tweaks.options.get_mut("fps.cap") {
        cap.def.special = vec![Mark { value: json!(0), label: "No cap".into() }, Mark { value: json!("vrr"), label: "Refresh rate − 3".into() }];
        cap.tweak = Some(Box::new(quiver_api::tweaks::TweakRow {
            profile: Some(json!(120)),
            at: quiver_api::tweaks::ScopeKind::Profile,
            planned: Some(quiver_api::tweaks::Planned::Provided {
                by: "DLSS".into(),
                surface: "m1/aco-dlss".into(),
                option: "reflex.fps_limit".into(),
                value: Some(json!(120)),
                mode: "bind".into(),
                can_prefer_quiver: true,
            }),
            ..Default::default()
        }));
    }
    tweaks.pages = vec![
        page("fg", "Frame generation", vec![group("", &["lsfg.multiplier", "lsfg.flow", "lsfg.performance", "dlss.fg"])]),
        page("rate", "Frame rate", vec![group("", &["fps.cap"])]),
        page("gfx", "Graphics", vec![group("", &["dxvk.async", "nvidia.reflex", "display.hdr"])]),
        page("wine", "Wine", vec![group("", &["wine.fsync"])]),
    ];
    let readouts: std::collections::BTreeMap<String, Vec<(String, String)>> =
        [("m1/aco-dlss".to_owned(), [("aco-dlss.fg", "Frame Generation"), ("aco-dlss.res", "Rendered → shown"), ("aco-dlss.fps", "Rendered / presented"), ("aco-dlss.reflex", "Reflex")].map(|(a, b)| (a.to_owned(), b.to_owned())).to_vec())].into();
    let effective = HudConfig { enabled: true, on: true, layout: "standard".into(), scale: 1.0, ..HudConfig::default() };
    let status: HudStatus = serde_json::from_value(json!({
        "session": "s-demo", "product_id": 3539, "enabled": true, "on": true, "layout": "standard",
        "presenter": "game", "presenter_reason": "the layer draws", "renderer_attached": true, "model_rev": 12, "captures": "shown"
    }))
    .unwrap();
    let pv = |text: &str, unit: &str, level: Level| PerfValue { value: text.parse().ok(), text: text.into(), unit: unit.into(), level, stale: false };
    let mut metrics = std::collections::BTreeMap::new();
    let mut put = |id: &str, stat: Stat, v: PerfValue| {
        metrics.entry(id.to_owned()).or_insert_with(std::collections::BTreeMap::new).insert(stat, v);
    };
    // The SightPerformance mock-up's numbers: 118 shown, 59 rendered with 2× frame generation.
    put("fps", Stat::Mean, pv("118", "", Level::Normal));
    put("fps", Stat::P1low, pv("94", "", Level::Normal));
    put("fps.p1low", Stat::Last, pv("94", "", Level::Normal));
    put("fps.base", Stat::Mean, pv("59", "", Level::Normal));
    put("fps.generated", Stat::Mean, pv("59", "", Level::Normal));
    put("frametime", Stat::Mean, pv("8.5", "ms", Level::Normal));
    put("gpu.load", Stat::Last, pv("87", "%", Level::Normal));
    put("gpu.temp", Stat::Last, pv("72", "°", Level::Normal));
    put("gpu.power", Stat::Last, pv("96", "W", Level::Normal));
    put("gpu.vram.used", Stat::Last, pv("6.1", "GB", Level::Normal));
    put("gpu.vram.total", Stat::Last, pv("8.0", "GB", Level::Normal));
    put("gpu.source", Stat::Last, PerfValue { text: "nvml".into(), ..Default::default() });
    put("cpu.load", Stat::Last, pv("41", "%", Level::Normal));
    put("cpu.temp", Stat::Last, pv("78", "°", Level::Normal));
    put("proc.rss", Stat::Last, pv("5.4", "GB", Level::Normal));
    put("mem.used", Stat::Last, pv("11.2", "GB", Level::Normal));
    put("latency.pc", Stat::Mean, pv("28", "ms", Level::Normal));
    put("latency.render", Stat::Mean, pv("21", "ms", Level::Normal));
    put("nv.reflex.mode", Stat::Last, PerfValue { text: "boost".into(), ..Default::default() });
    put("nv.reflex.cap", Stat::Last, pv("120", "fps", Level::Normal));
    put("aco-dlss.fg", Stat::Last, pv("On · F9", "", Level::Normal));
    put("aco-dlss.res", Stat::Last, pv("1280×800 → 2560×1600", "", Level::Normal));
    put("aco-dlss.fps", Stat::Last, pv("59 / 118", "fps", Level::Normal));
    put("aco-dlss.reflex", Stat::Last, pv("On + Boost", "", Level::Normal));
    // Minutes ago it was unlocked (`None`: locked), and progress (current, max).
    let ach = |id: &str, name: &str, desc: &str, hidden: bool, at: Option<i64>, progress: Option<(f64, f64)>| Ach {
        id: id.into(),
        name: name.into(),
        description: desc.into(),
        hidden,
        unlocked: at.is_some(),
        unlocked_at: at.map(|m| now - chrono::Duration::minutes(m)),
        progress: progress.map(|(current, max)| quiver_api::stores::AchievementProgress { current, max, percent: false }),
        ..Default::default()
    };
    let mut achs = Achs::sorted(vec![
        ach("12", "Hidden Ones", "Complete the main quest \"The Hidden Ones\".", false, Some(12), None),
        ach("7", "First Visit", "Complete one tour.", false, Some(55), None),
        ach("3", "The Medjay", "Complete the main quest's first chapter.", false, Some(60 * 24 * 9), None),
        ach("21", "Seasoned Seer", "Find 50 papyrus puzzles.", false, None, Some((22.0, 50.0))),
        ach("41", "Who Watches the Watchers", "Reach the end of the story.", true, None, None),
        ach("33", "Phylakes Slayer", "Kill all the Phylakes.", false, None, Some((3.0, 30.0))),
        ach("44", "Photo Finish", "Win a chariot race in the Hippodrome.", false, None, None),
        ach("40", "Seasoned Hunter", "Kill 10 hippos.", false, None, Some((6.0, 10.0))),
    ]);
    // The rest of the game's 70 aren't listed in the fixture.
    (achs.unlocked, achs.total) = (31, 70);
    achs.reporting = Some(quiver_api::sight::session::Reporting { store: Some(quiver_api::StoreId::Ubisoft), mode: "on".into() });
    use crate::panel::sections::session::{Extras, ModLine, OptionsLine, SavesSummary};
    use quiver_api::sight::session::{SessionEvent, SessionEventKind, SessionInfo};
    let event = |mins: i64, kind, text: &str, by: Option<&str>| SessionEvent { at: now - chrono::Duration::minutes(mins), kind, text: text.into(), by: by.map(String::from) };
    let extras = Extras {
        info: Some(SessionInfo {
            session: "s-demo".into(),
            proton: Some("GE-Proton10-25".into()),
            events: vec![
                event(59, SessionEventKind::Mods, "DLSS v4 is ready", Some("aco-dlss")),
                event(33, SessionEventKind::Download, "Download finished: Far Cry 5", None),
            ],
            ..Default::default()
        }),
        info_error: None,
        mods: vec![ModLine { name: "DLSS".into(), chip: Some("v3 · current".into()) }, ModLine { name: "Skip intro videos".into(), chip: Some("built-in · 2".into()) }],
        saves: Load::Ready(SavesSummary {
            store: "Default".into(),
            state: "Cloud in sync".into(),
            tone: Some(quiver_ui::options::Tone::Ok),
            last_save: Some(now - chrono::Duration::minutes(2)),
            last_what: Some("slot 3".into()),
            shared_with: vec!["Vanilla".into()],
            note: None,
        }),
        options: Some(OptionsLine { launched: 14, changed: 3 }),
    };
    PanelData {
        session: Load::Ready(SessionView {
            session: "s-demo".into(),
            game: quiver_api::GameRef::ubisoft(3539),
            name: "Assassin's Creed Origins".into(),
            profile_id: Some("p-dlss".into()),
            profile_name: Some("DLSS".into()),
            mods: vec!["DLSS v3 (current)".into(), "Skip intro videos".into()],
            settings: Some("its own (Documents/Assassin's Creed Origins)".into()),
            started: Some(now - chrono::Duration::minutes(72)),
            total_secs: Some(212 * 3600),
            copy: "Quiver install".into(),
            reported: Some(true),
            pids: vec![],
        }),
        options: Load::Ready(vec![dlss, game, dxvk]),
        tweaks: Load::Ready(tweaks),
        tweak_scope_name: "the DLSS profile".into(),
        hud: Load::Ready(HudState { status: Some(status), settings: HudSettingsView { scope: Default::default(), set: Default::default(), effective, revision: 4, anti_cheat: None } }),
        achievements: Load::Ready(achs),
        metrics,
        extras,
        prefs: Default::default(),
        readouts,
    }
}

/// Built-in Standard as quiverd's `HudLayout` request returns it (the editor's starting point).
pub fn standard_layout() -> quiver_api::perf::HudLayout {
    use serde_json::json;
    quiver_api::perf::HudLayout {
        id: "standard".into(),
        name: "Standard".into(),
        builtin: true,
        orientation: Orientation::Block,
        anchor: None,
        background: None,
        size: [284.0, 162.0],
        rows: vec![
            vec![json!({"metric": "fps", "suffix": "FPS", "emphasis": "hero"}), json!({"spacer": true}), json!({"metric": "fps.base", "suffix": "BASE", "emphasis": "secondary", "when": "fg.active"})],
            vec![json!({"graph": "frametime", "series": ["frametime", "frametime.base"], "rows": 2})],
            vec![
                json!({"metric": "frametime", "emphasis": "quiet"}),
                json!({"text": "·", "level": "dim"}),
                json!({"metric": "fps.p1low", "label": "1% low", "emphasis": "quiet"}),
                json!({"spacer": true}),
                json!({"metric": "fg.ratio", "short": "FG", "emphasis": "tag", "when": "fg.active"}),
                json!({"metric": "nv.fg.driver", "short": "FG", "emphasis": "tag"}),
            ],
            vec![json!({"metric": "latency.pc", "suffix": "PC LATENCY"}), json!({"spacer": true}), json!({"metric": "nv.reflex.mode", "short": "REFLEX", "emphasis": "tag", "when": "nv.reflex.mode"})],
            vec![json!({"split": "latency", "parts": ["latency.split.sim", "latency.split.render", "latency.split.queue", "latency.split.gpu"]})],
            vec![
                json!({"metric": "gpu.load", "short": "GPU"}),
                json!({"metric": "gpu.temp", "emphasis": "quiet"}),
                json!({"metric": "gpu.power", "emphasis": "quiet"}),
                json!({"metric": "cpu.load", "short": "CPU"}),
                json!({"metric": "cpu.temp", "emphasis": "quiet"}),
            ],
        ],
        row_options: Vec::new(),
    }
}

/// The Performance section's fixture: 30 s of frame times (presented near 8.5 ms with three
/// hitches, rendered near 17 ms), the session's catalog (the layer and NVIDIA's counting, a
/// mod named pharos with its own latency split, the system) and the layouts.
pub fn sample_perf() -> crate::panel::sections::performance::PerfData {
    use crate::panel::sections::performance::PerfData;
    use crate::panel::state::Load;
    use quiver_api::perf::{FrameTimes, HudLayoutInfo, MetricDescriptor, MetricInfo, MetricKind, MetricState, Unit, Visibility};
    let presented: Vec<Option<f32>> = (0..300)
        .map(|k| Some(if [36, 70, 210].contains(&k) { [21.0, 14.0, 25.0][[36, 70, 210].iter().position(|x| *x == k).unwrap()] } else { 8.3 + ((k * 7) % 5) as f32 * 0.12 }))
        .collect();
    let rendered: Vec<Option<f32>> = presented.iter().map(|p| p.map(|v| if v > 12.0 { v + 7.0 } else { 16.6 + (v - 8.3) })).collect();
    let info = |id: &str, kind: MetricKind, unit: Unit, label: &str, short: Option<&str>, canonical: bool, producer: &str, roles: &[&str], state: MetricState| {
        let mut d = MetricDescriptor::new(id, kind);
        d.unit = unit;
        d.label = Some(label.into());
        d.short = short.map(String::from);
        let live_label = match producer {
            "layer" => "Sight layer",
            "nvidia" => "NVIDIA (Reflex, DLSS)",
            "system" => "System",
            "mod:pharos" => "pharos",
            _ => "Quiver",
        };
        MetricInfo {
            descriptor: d,
            producer: if canonical { "quiverd".into() } else { producer.into() },
            producer_label: if canonical { "Quiver".into() } else { live_label.into() },
            canonical,
            source: None,
            roles: roles.iter().map(|r| r.to_string()).collect(),
            conflicts: Vec::new(),
            state,
            last: None,
            live_producer: Some(producer.into()),
            live_producer_label: Some(live_label.into()),
        }
    };
    use MetricKind::{Gauge, State, Text};
    use MetricState::{Declared, Live};
    let mut catalog = vec![
        info("fps", Gauge, Unit::Fps, "FPS", Some("FPS"), true, "layer", &[], Live),
        info("fps.base", Gauge, Unit::Fps, "Base FPS", Some("BASE"), true, "nvidia", &[], Live),
        info("frametime", Gauge, Unit::Ms, "Frame time", Some("FT"), true, "layer", &[], Live),
        info("frametime.base", Gauge, Unit::Ms, "Rendered frame time", None, true, "nvidia", &[], Live),
        info("fps.p1low", Gauge, Unit::Fps, "1% low", None, true, "layer", &[], Live),
        info("latency.pc", Gauge, Unit::Ms, "PC latency", Some("PC"), true, "nvidia", &[], Live),
        info("fg.ratio", Gauge, Unit::None, "Frame generation", Some("FG"), true, "nvidia", &[], Live),
        info("upscaler", Text, Unit::None, "Upscaler", None, true, "nvidia", &[], Declared),
        info("nv.latency.pc", Gauge, Unit::Ms, "PC latency (to present)", Some("PC"), false, "nvidia", &["latency.pc"], Live),
        info("nv.reflex.mode", State, Unit::None, "Reflex", Some("RFX"), false, "nvidia", &[], Live),
        info("pharos.fg", State, Unit::None, "Frame Generation", Some("FG"), false, "mod:pharos", &[], Live),
        info("pharos.render", Text, Unit::None, "Render resolution", None, false, "mod:pharos", &[], Live),
    ];
    for (id, label) in [("latency.split.sim", "Latency: simulation"), ("latency.split.render", "Latency: render submit"), ("latency.split.queue", "Latency: queue"), ("latency.split.gpu", "Latency: GPU")] {
        catalog.push(info(id, Gauge, Unit::Ms, label, None, true, "mod:pharos", &[], Live));
    }
    for (id, label, short, unit) in [
        ("gpu.load", "GPU load", Some("GPU"), Unit::Percent),
        ("gpu.temp", "GPU temperature", None, Unit::Celsius),
        ("gpu.power", "GPU power", None, Unit::Watt),
        ("gpu.vram.used", "VRAM used", Some("VRAM"), Unit::Bytes),
        ("cpu.load", "CPU load", Some("CPU"), Unit::Percent),
        ("cpu.temp", "CPU temperature", None, Unit::Celsius),
        ("proc.rss", "Game RAM", None, Unit::Bytes),
    ] {
        catalog.push(info(id, Gauge, unit, label, short, false, "system", &[], Live));
    }
    catalog.push(info("bat.level", Gauge, Unit::Percent, "Battery", Some("BAT"), false, "system", &[], Declared));
    let mut src = info("gpu.source", Text, Unit::None, "GPU sensors", None, false, "system", &[], Live);
    src.descriptor.visibility = Some(Visibility::Advanced);
    catalog.push(src);
    let layouts = crate::panel::state::PRESETS
        .iter()
        .map(|(id, name)| HudLayoutInfo { id: (*id).into(), name: (*name).into(), builtin: true, path: None, error: None })
        .chain(std::iter::once(HudLayoutInfo { id: "dlss-tuning".into(), name: "DLSS tuning".into(), builtin: false, path: None, error: None }))
        .collect();
    PerfData {
        frames: Some(FrameTimes { session: "s-demo".into(), span_ms: 30_000, presented, rendered, frames: 3540, hitches_per_min: Some(3.0) }),
        catalog: Load::Ready(catalog),
        layouts: Load::Ready(layouts),
        scoped: None,
    }
}

/// `--demo panel`: the panel with [`sample_panel`] on this desktop (a test namespace unless
/// `--namespace quiver-sight`), closed after `--seconds`.
fn feed_panel(a: &Args, tx: &UnboundedSender<Msg>) {
    let section = a.section.as_deref().and_then(quiver_api::sight::PanelSection::parse);
    let _ = tx.unbounded_send(Msg::Panel(quiver_api::sight::PanelCommand {
        seq: 1,
        action: quiver_api::sight::PanelAction::Open,
        session: Some("s-demo".into()),
        section,
        monitor: None,
        by: "demo".into(),
        at: chrono::Utc::now(),
    }));
    let _ = tx.unbounded_send(Msg::Loaded { session: "s-demo".into(), data: Box::new(crate::link::Loaded::All(Box::new(sample_panel()))) });
}
