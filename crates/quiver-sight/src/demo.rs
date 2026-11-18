//! `--demo`: the UI without quiverd, fed with sample models and toasts (for screenshots and
//! for trying the surfaces on a desktop). Everything closes after `--seconds`.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::perf::{Anchor, Captures, HudCell, HudModel, HudRow, Level, Orientation, PerfValue, Presenter, hud_card};

use crate::Args;
use crate::link::Msg;
use crate::toasts::{ToastData, ToastKind};

fn v(text: &str, unit: &str, level: Level) -> PerfValue {
    PerfValue { value: text.parse().ok(), text: text.into(), unit: unit.into(), level, stale: false }
}

fn value(metric: &str, label: Option<&str>, pv: PerfValue) -> HudCell {
    HudCell::Value { metric: metric.into(), label: label.map(String::from), value: pv }
}

/// Sample frame times (ms), oldest first, with a hitch.
pub fn frametimes(i: u32, n: usize) -> Vec<f32> {
    (0..n).map(|k| {
        let k = k + i as usize * 7;
        let spike = if k % 97 == 40 { 26.0 } else if k % 23 == 5 { 3.0 } else { 0.0 };
        6.3 + ((k * 7) % 5) as f32 * 0.3 + spike
    }).collect()
}

/// A model like quiverd's for `layout`, with values that move with `i`.
pub fn sample_model(layout: &str, i: u32, presenter: Presenter, extent: Option<[u32; 2]>) -> HudModel {
    let w = [0i32, 3, -2, 5, -4, 1, 2, -1][i as usize % 8];
    let fps = (158 + w).to_string();
    let base = ((158 + w) / 2).to_string();
    let fg = |cells: &mut Vec<HudCell>| {
        // FPS only has no label (quiverd's `fps_pair(None)`).
        cells.push(value("fps.base", (layout != "fps").then_some("FPS"), v(&base, "", Level::Normal)));
        cells.push(HudCell::Text { text: "→".into(), level: Level::Dim });
        cells.push(value("fps", None, v(&fps, "", Level::Normal)));
    };
    let points = if presenter == Presenter::Desktop { frametimes(i, 300) } else { Vec::new() };
    let graph = HudCell::Graph { series: vec!["frametime".into()], span_ms: 3000, rows: 2, min_ms: 0.0, max_ms: 0.0, warn_above_ms: 33.4, bad_above_ms: 50.0, points };
    let gpu = vec![
        value("gpu.load", Some("GPU"), v(&(97 - w.abs()).to_string(), "%", Level::Warn)),
        value("gpu.temp", None, v("71", "°", Level::Normal)),
        value("gpu.power", None, v(&(92 + w).to_string(), "W", Level::Normal)),
    ];
    let cpu = vec![value("cpu.load", Some("CPU"), v(&(38 - w).to_string(), "%", Level::Normal)), value("cpu.temp", None, v("64", "°", Level::Normal))];
    let (orientation, background, rows) = match layout {
        "fps" => {
            let mut r = Vec::new();
            fg(&mut r);
            (Orientation::Row, 0.0, vec![HudRow { cells: r }])
        }
        "compact" => {
            let mut r = Vec::new();
            fg(&mut r);
            r.push(value("frametime", None, v("6.3", "ms", Level::Normal)));
            r.push(value("fps.p1low", Some("1%↓"), v("128", "", Level::Normal)));
            r.push(HudCell::Separator);
            r.extend(gpu);
            r.push(HudCell::Separator);
            r.extend(cpu);
            r.push(HudCell::Separator);
            r.push(HudCell::Badge { metric: "upscaler".into(), label: None, value: v("DLSS K", "", Level::Accent) });
            r.push(value("render.resolution", None, v("1280×800", "", Level::Dim)));
            r.push(value("fg.ratio", Some("FG"), v("2.0", "×", Level::Normal)));
            r.push(value("latency.render", Some("LAT"), v("21", "ms", Level::Normal)));
            (Orientation::Row, 0.45, vec![HudRow { cells: r }])
        }
        _ => {
            let mut first = Vec::new();
            fg(&mut first);
            first.push(value("fps.p1low", Some("1% low"), v("128", "", Level::Normal)));
            let mut rows = vec![
                HudRow { cells: first },
                HudRow { cells: vec![value("frametime", Some("FT"), v("6.3", "ms", Level::Normal)), value("frametime.base", Some("base"), v("12.7", "ms", Level::Dim))] },
                HudRow { cells: vec![graph] },
                HudRow { cells: [gpu, vec![HudCell::Bar { metric: "gpu.vram.used".into(), label: None, value: v("6.1", "GB", Level::Normal), fraction: 0.76 }]].concat() },
                HudRow { cells: [cpu, vec![value("mem.used", Some("RAM"), v("11.2", "GB", Level::Normal))]].concat() },
                HudRow {
                    cells: vec![
                        HudCell::Badge { metric: "upscaler".into(), label: None, value: v("DLSS K", "", Level::Accent) },
                        value("render.resolution", None, v("1280×800", "", Level::Normal)),
                        HudCell::Text { text: "→".into(), level: Level::Dim },
                        value("output.resolution", None, v("2560×1600", "", Level::Normal)),
                    ],
                },
                HudRow { cells: vec![value("latency.render", Some("LAT"), v("21", "ms", Level::Good))] },
            ];
            if layout == "full" {
                rows.push(HudRow {
                    cells: vec![
                        value("display.mode", Some("DISP"), v("2560×1600@165", "", Level::Normal)),
                        value("display.vrr", Some("VRR"), v("on", "", Level::Normal)),
                        value("display.tearing", Some("TEAR"), v("off", "", Level::Normal)),
                        value("display.path", None, v("direct", "", Level::Normal)),
                    ],
                });
                rows.push(HudRow { cells: vec![HudCell::Sparkline { metric: "cpu.load".into(), label: Some("CPU".into()), value: v("38", "%", Level::Normal), points: (0..30).map(|k| ((k * 13 + i) % 17) as f32 / 16.0).collect() }] });
            }
            (Orientation::Block, 0.45, rows)
        }
    };
    let text_px = extent.map(|[_, h]| h as f32 * 0.016).unwrap_or(17.0);
    // As quiverd sizes the presets: their width, and the height of every row.
    let width = hud_card::preset_width(layout).unwrap_or(hud_card::default_width(orientation));
    let slots: Vec<f32> = rows.iter().map(|r| hud_card::row_slot(&r.cells)).collect();
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
    }
}

/// Demo content variants (`--variant`), for checking that the card keeps its size whatever
/// it holds: `long` stretches every value (4-digit fps, long text values and badges), `short`
/// is what quiverd sends with FG and the upscaler off and no latency provider (gated cells
/// and whole rows gone).
pub const VARIANTS: [&str; 3] = ["base", "long", "short"];

pub fn vary(mut m: HudModel, variant: &str) -> HudModel {
    let metric = |c: &HudCell| match c {
        HudCell::Value { metric, .. } | HudCell::Badge { metric, .. } | HudCell::Bar { metric, .. } | HudCell::Sparkline { metric, .. } => Some(metric.clone()),
        _ => None,
    };
    match variant {
        "long" => {
            for r in &mut m.rows {
                for c in &mut r.cells {
                    let id = metric(c).unwrap_or_default();
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
                        "display.path" => "composited (blocked: overlays, a notification)",
                        "latency.render" => "1021",
                        _ => continue,
                    };
                    value.text = text.into();
                    value.value = text.parse().ok();
                }
            }
        }
        "short" => {
            let gone = ["fps.base", "frametime.base", "render.resolution", "output.resolution", "upscaler", "fg.ratio", "latency.render"];
            for r in &mut m.rows {
                r.cells.retain(|c| metric(c).is_none_or(|id| !gone.contains(&id.as_str())) && !matches!(c, HudCell::Text { .. }));
                // The label of a dropped first cell moves to the next one (as quiverd does).
                if let Some(HudCell::Value { metric, label, .. }) = r.cells.first_mut()
                    && metric == "fps"
                    && m.layout != "fps"
                {
                    *label = Some("FPS".into());
                }
                while matches!(r.cells.last(), Some(HudCell::Separator)) {
                    r.cells.pop();
                }
            }
            m.rows.retain(|r| r.cells.iter().any(|c| metric(c).is_some() || matches!(c, HudCell::Graph { .. })));
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
            started: Some(now - chrono::Duration::minutes(72)),
            total_secs: Some(212 * 3600),
            copy: "Quiver install".into(),
            reported: Some(true),
            pids: vec![],
        }),
        options: Load::Ready(vec![dlss]),
        tweaks: Load::Ready(tweaks),
        tweak_scope_name: "the DLSS profile".into(),
        hud: Load::Ready(HudState { status: Some(status), settings: HudSettingsView { scope: Default::default(), set: Default::default(), effective, revision: 4 } }),
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
