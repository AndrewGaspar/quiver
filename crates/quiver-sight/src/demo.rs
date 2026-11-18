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
            r.push(value("latency.pc", Some("PC"), v("16.8", "ms", Level::Normal)));
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
                HudRow { cells: vec![value("latency.render", Some("LAT"), v("21", "ms", Level::Good)), value("latency.pc", Some("PC"), v("16.8", "ms", Level::Normal))] },
            ];
            if layout == "full" {
                // NVIDIA (SIGHT-HUD §3.17): Reflex and the FG multiplier the layer counts, then DLSS.
                rows.push(HudRow {
                    cells: vec![
                        HudCell::Badge { metric: "nv.reflex.mode".into(), label: Some("RFX".into()), value: v("boost", "", Level::Good) },
                        value("nv.reflex.cap", Some("CAP"), v("120", "fps", Level::Normal)),
                        value("fg.ratio", Some("FG"), v("2.0", "×", Level::Normal)),
                    ],
                });
                rows.push(HudRow {
                    cells: vec![
                        value("dlss.sr.quality", Some("SR"), v("Perf", "", Level::Normal)),
                        value("dlss.sr.model", None, v("Transformer", "", Level::Normal)),
                        value("dlss.version", Some("DLL"), v("310.9.16", "", Level::Normal)),
                        value("dlss.sr.requested", Some("REQ"), v("K", "", Level::Dim)),
                    ],
                });
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
                        "latency.pc" => "1002.5",
                        "dlss.sr.model" => "Transformer (DLSS 4.5)",
                        "dlss.version" => "310.10.123.4",
                        "dlss.sr.requested" => "latest · Ultra Perf",
                        "nv.reflex.cap" => "1000",
                        _ => continue,
                    };
                    value.text = text.into();
                    value.value = text.parse().ok();
                }
            }
        }
        "short" => {
            // No NVIDIA GPU, no DLSS, Reflex off: every NVIDIA cell goes with them.
            let gone = [
                "fps.base",
                "frametime.base",
                "render.resolution",
                "output.resolution",
                "upscaler",
                "fg.ratio",
                "latency.render",
                "latency.pc",
                "nv.reflex.mode",
                "nv.reflex.cap",
                "dlss.sr.quality",
                "dlss.sr.model",
                "dlss.version",
                "dlss.sr.requested",
            ];
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
    } else if !matches!(demo, "toasts" | "desktop") {
        anyhow::bail!("--demo toasts|desktop|image");
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
