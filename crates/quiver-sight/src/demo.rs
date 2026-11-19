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
        parts: [("latency.split.sim", "SIM", 6.0), ("latency.split.render", "RENDER", 9.0), ("latency.split.queue", "QUEUE", 4.0), ("latency.split.out", "OUT", 9.0)]
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
    // As quiverd sizes the presets: their width, and the height of every row (Full keeps
    // room for a second mod strip).
    let width = hud_card::preset_width(layout).unwrap_or(hud_card::default_width(orientation));
    let mut slots: Vec<f32> = rows.iter().map(hud_card::row_height).collect();
    if layout == "full" {
        slots.push(hud_card::LINE);
    }
    let size = [width, hud_card::height(orientation, &slots, hud_card::panel(background))];
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

/// Demo content variants (`--variant`), for checking that the card keeps its size whatever
/// it holds: `long` stretches every value (4-digit fps, long text values and badges), `short`
/// is what quiverd sends for the same game without the mod and frame generation ("Standard,
/// Vanilla profile": no base FPS, no latency, no split, no strip; one graph line).
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
    use quiver_api::options::{Apply, EnumValue, GroupState, LiveChannel, LiveInfo, Mark, OptionDef, OptionState, OptionType, PageState, Pending, SurfaceState};
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
    let mut fgmode = opt(OptionType::Enum, "Frame generation path", None, Apply::At, json!("d3d12"));
    fgmode.def.at = Some("swapchain creation (the game makes its swapchain at startup and on mode changes)".into());
    fgmode.def.values = choices(&[("d3d11", "D3D11"), ("d3d12", "D3D12 (vkd3d)")]);
    dlss.options.insert("fg.mode".into(), fgmode);
    let mut npc = opt(OptionType::Float, "Crowd density", Some("Multiplies the game's NPC cap"), Apply::Live, json!(1.6));
    (npc.def.min, npc.def.max, npc.def.step, npc.def.precision, npc.def.latency) = (Some(1.0), Some(3.0), Some(0.1), Some(1), Some("about 10 seconds".into()));
    dlss.options.insert("world.npc_density".into(), npc);
    let mut cap = opt(OptionType::Int, "Frame cap", None, Apply::Live, json!(0));
    (cap.def.min, cap.def.max, cap.def.step) = (Some(30.0), Some(240.0), Some(1.0));
    cap.def.special = vec![Mark { value: json!(0), label: "No cap".into() }];
    dlss.options.insert("pace.cap".into(), cap);
    let mut reset = opt(OptionType::Bool, "Rebuild shader cache", None, Apply::Restart, json!(false));
    reset.def.danger = Some("the next start takes several minutes".into());
    dlss.options.insert("debug.rebuild".into(), reset);
    let group = |title: &str, ids: &[&str]| GroupState { title: title.into(), options: ids.iter().map(|s| s.to_string()).collect() };
    dlss.pages = vec![PageState {
        id: "main".into(),
        title: "DLSS".into(),
        groups: vec![
            group("Upscaling", &["upscale.mode", "upscale.preset", "upscale.scale"]),
            group("Frame generation", &["fg.enabled", "fg.mode", "pace.cap"]),
            group("World", &["world.npc_density", "debug.rebuild"]),
        ],
    }];
    let mut tweaks = SurfaceState { key: "quiver/tweaks".into(), surface: "tweaks".into(), title: "Quiver Tweaks".into(), ..Default::default() };
    tweaks.options.insert("dxvk.async".into(), opt(OptionType::Bool, "Asynchronous shader compiles", Some("Fewer stutters the first time effects show up"), Apply::Launch, json!(true)));
    tweaks.options.insert("nvidia.reflex".into(), opt(OptionType::Bool, "NVIDIA Reflex", Some("Lower latency on RTX cards"), Apply::Launch, json!(true)));
    let mut hdr = opt(OptionType::Enum, "HDR", None, Apply::Launch, json!("off"));
    hdr.def.values = choices(&[("off", "Off"), ("on", "On (Wayland)")]);
    tweaks.options.insert("display.hdr".into(), hdr);
    let mut fsync = opt(OptionType::Bool, "Fsync", None, Apply::Launch, json!(true));
    fsync.def.help = Some("Faster Wine synchronisation on this kernel".into());
    tweaks.options.insert("wine.fsync".into(), fsync);
    tweaks.pages = vec![PageState {
        id: "main".into(),
        title: "Tweaks".into(),
        groups: vec![group("Graphics", &["dxvk.async", "nvidia.reflex", "display.hdr"]), group("Wine", &["wine.fsync"])],
    }];
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
    put("fps", Stat::Mean, pv("158", "", Level::Normal));
    put("fps", Stat::P1low, pv("121", "", Level::Normal));
    put("fps.base", Stat::Mean, pv("79", "", Level::Normal));
    put("frametime", Stat::Mean, pv("6.3", "ms", Level::Normal));
    put("gpu.load", Stat::Last, pv("97", "%", Level::Warn));
    put("gpu.temp", Stat::Last, pv("71", "°", Level::Normal));
    put("gpu.power", Stat::Last, pv("92", "W", Level::Normal));
    put("gpu.vram.used", Stat::Last, pv("6.1", "GB", Level::Normal));
    put("cpu.load", Stat::Last, pv("38", "%", Level::Normal));
    put("mem.used", Stat::Last, pv("11.2", "GB", Level::Normal));
    let ach = |id: &str, name: &str, desc: &str, hidden: bool, at: Option<i64>| Ach {
        id: id.into(),
        name: name.into(),
        description: desc.into(),
        hidden,
        unlocked: at.is_some(),
        unlocked_at: at.map(|d| now - chrono::Duration::days(d)),
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
        options: Load::Ready(vec![dlss]),
        tweaks: Load::Ready(tweaks),
        tweak_scope_name: "the DLSS profile".into(),
        hud: Load::Ready(HudState { status: Some(status), settings: HudSettingsView { scope: Default::default(), set: Default::default(), effective, revision: 4, anti_cheat: None } }),
        achievements: Load::Ready(Achs::sorted(vec![
            ach("12", "Mysteries of Egypt", "Find every tomb in the Giza necropolis.", false, Some(0)),
            ach("3", "The Medjay", "Complete the main quest's first chapter.", false, Some(9)),
            ach("40", "Seasoned Hunter", "Kill 10 hippos.", false, None),
            ach("41", "Who Watches the Watchers", "Reach the end of the story.", true, None),
            ach("44", "Photo Finish", "Win a chariot race in the Hippodrome.", false, None),
        ])),
        metrics,
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
