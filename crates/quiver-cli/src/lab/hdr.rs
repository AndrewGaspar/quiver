//! `quiver lab hdr` and `quiver lab shot --hdr` (design/LAB.md §13): what the game's swapchain
//! is, the HDR metadata it set and what gamescope makes of it; a capture of the presented image
//! in its own format, measured in nits.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_core::lab::Inst;
use quiver_core::lab::hdr::{self as labhdr, CaptureMeta, GamescopeHdr, HdrStats};
use quiver_perf::LayerStatus;
use quiver_perf::layout::{HDR_CAPTURE_READY, HDR_METADATA_SET};

use super::{ops, sight};

/// What gamescope says about HDR (its root-window properties).
pub fn gamescope(inst: &Inst) -> Option<GamescopeHdr> {
    let mut cmd = vec!["xprop".to_owned(), "-root".to_owned()];
    cmd.extend(labhdr::GAMESCOPE_ATOMS.iter().map(|a| (*a).to_owned()));
    let o = ops::exec(inst, &[], &cmd).ok()?;
    Some(GamescopeHdr::parse(&String::from_utf8_lossy(&o.stdout)))
}

fn need_layer(inst: &Inst, run_dir: Option<&Path>) -> Result<(PathBuf, LayerStatus)> {
    let m = inst.load()?;
    if !m.sight {
        bail!("{0} doesn't load the Sight layer, which reads the swapchain: quiver lab edit {0} sight=on, then quiver lab down {0} && quiver lab up {0}", inst.name);
    }
    let dir = sight::session_dir(inst, run_dir).with_context(|| format!("no Sight session for this run (launch it: quiver lab launch {} --wait frame)", inst.name))?;
    let st = sight::layer_status(&dir).with_context(|| format!("the Sight layer hasn't started in the game yet ({})", dir.display()))?;
    Ok((dir, st))
}

fn metadata_json(s: &LayerStatus) -> serde_json::Value {
    if s.hdr_flags & HDR_METADATA_SET == 0 {
        return serde_json::Value::Null;
    }
    let nits = |v: u32| f64::from(v) / 10_000.0;
    let xy: Vec<f64> = s.hdr_primaries.iter().map(|&v| f64::from(v) / 100_000.0).collect();
    serde_json::json!({
        "max_cll": nits(s.hdr_max_cll), "max_fall": nits(s.hdr_max_fall),
        "max_luminance": nits(s.hdr_max_luminance), "min_luminance": nits(s.hdr_min_luminance),
        "primaries": { "red": [xy[0], xy[1]], "green": [xy[2], xy[3]], "blue": [xy[4], xy[5]], "white": [xy[6], xy[7]] },
        "calls": s.hdr_metadata_calls,
    })
}

/// `quiver lab hdr N`.
pub fn status(inst: &Inst, run_dir: Option<&Path>, json: bool) -> Result<()> {
    let m = inst.load()?;
    let layer = need_layer(inst, run_dir);
    let gs = gamescope(inst);
    if json {
        let l = layer.as_ref().ok().map(|(dir, s)| {
            serde_json::json!({
                "session_dir": dir,
                "extent": [s.extent_w, s.extent_h],
                "format": s.vk_format, "format_name": labhdr::format_name(s.vk_format),
                "color_space": s.color_space, "color_space_name": labhdr::color_space_name(s.color_space),
                "hdr": labhdr::is_hdr_color_space(s.color_space) && s.swapchain != 0,
                "metadata": metadata_json(s),
                "capture_ready": s.hdr_flags & HDR_CAPTURE_READY != 0,
                "captures_done": s.capture_done,
            })
        });
        return super::print_json(&serde_json::json!({
            "display_hdr": m.display.hdr, "display": m.display.to_string(),
            "swapchain": l, "layer_error": layer.as_ref().err().map(|e| format!("{e:#}")),
            "gamescope": gs,
        }));
    }
    println!("display: {} — {}", m.display, if m.display.hdr { "gamescope forces its headless output to HDR10 PQ" } else { "SDR (quiver lab edit N display.hdr=on, then down/up)" });
    match &layer {
        Ok((_, s)) if s.swapchain == 0 => println!("swapchain: none yet"),
        Ok((_, s)) => {
            println!(
                "swapchain: {}x{} {} in {}{}",
                s.extent_w,
                s.extent_h,
                labhdr::format_name(s.vk_format),
                labhdr::color_space_name(s.color_space),
                if labhdr::is_hdr_color_space(s.color_space) { " — HDR" } else { " — SDR" }
            );
            if s.hdr_flags & HDR_METADATA_SET != 0 {
                let n = |v: u32| f64::from(v) / 10_000.0;
                println!(
                    "metadata (vkSetHdrMetadataEXT, {} call(s)): max CLL {:.0}, max FALL {:.0}, mastering {:.4}–{:.0} nits",
                    s.hdr_metadata_calls,
                    n(s.hdr_max_cll),
                    n(s.hdr_max_fall),
                    n(s.hdr_min_luminance),
                    n(s.hdr_max_luminance)
                );
                let p: Vec<f64> = s.hdr_primaries.iter().map(|&v| f64::from(v) / 100_000.0).collect();
                println!("  primaries R ({:.3}, {:.3}) G ({:.3}, {:.3}) B ({:.3}, {:.3}) white ({:.4}, {:.4})", p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
            } else {
                println!("metadata: none set (vkSetHdrMetadataEXT not called)");
            }
            println!("capture: {}", if s.hdr_flags & HDR_CAPTURE_READY != 0 { format!("ready (quiver lab shot {} --hdr)", inst.name) } else { "not possible on this swapchain (no TRANSFER_SRC usage, or an unknown format)".into() });
        }
        Err(e) => println!("swapchain: unknown — {e:#}"),
    }
    match gs {
        Some(g) => {
            let yn = |v: Option<bool>| v.map_or("not set", |b| if b { "yes" } else { "no" });
            println!("gamescope: output HDR {}, the game presents HDR {}{}", yn(g.output_hdr), yn(g.app_hdr), g.app_eotf.map(|e| format!(" (metadata EOTF {})", if e == 2 { "PQ".to_owned() } else { e.to_string() })).unwrap_or_default());
        }
        None => println!("gamescope: not reachable (is the instance up?)"),
    }
    Ok(())
}

/// The capture's output files: `<stem>.raw`, `.json`, `.png` (16-bit).
pub struct HdrShot {
    pub png: PathBuf,
    pub raw: PathBuf,
    pub json: PathBuf,
    pub meta: CaptureMeta,
    pub stats: HdrStats,
}

/// `quiver lab shot N --hdr [OUT]`: asks the layer for a capture of the next present, waits
/// for it, measures it and writes the files next to `out` (its extension replaced).
pub fn shot(inst: &Inst, run_dir: Option<&Path>, out: &Path) -> Result<HdrShot> {
    let (dir, st) = need_layer(inst, run_dir)?;
    if st.hdr_flags & HDR_CAPTURE_READY == 0 {
        bail!("the game's swapchain can't be captured ({} in {}): made before Sight's capture support, or a format it can't read", labhdr::format_name(st.vk_format), labhdr::color_space_name(st.color_space));
    }
    let req = dir.join("capture.req");
    let prev = std::fs::read_to_string(&req).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    let n = prev.max(st.capture_done) + 1;
    std::fs::write(&req, format!("{n}\n"))?;
    // The layer's heartbeat (250 ms) sees it, the next present takes it, a later one reads it
    // back, the heartbeat writes it.
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if sight::layer_status(&dir).is_some_and(|s| s.capture_done >= n) {
            break;
        }
        if Instant::now() > end {
            bail!("no capture within 10 s (is the game presenting? A game held by quiver lab clock is captured from the frame the layer kept, which needs the current Sight layer; quiver lab logs {})", inst.name);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let raw_src = dir.join(format!("capture-{n}.raw"));
    let meta_src = dir.join(format!("capture-{n}.json"));
    let meta: CaptureMeta = serde_json::from_slice(&std::fs::read(&meta_src).with_context(|| meta_src.display().to_string())?).context("capture sidecar")?;
    let raw = std::fs::read(&raw_src).with_context(|| raw_src.display().to_string())?;
    let stats = labhdr::stats(&raw, meta.format, meta.color_space, meta.width, meta.height)?;
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p)?;
    }
    let png = out.with_extension("png");
    let raw_out = out.with_extension("raw");
    let json_out = out.with_extension("json");
    labhdr::png16(&raw, meta.format, meta.color_space, meta.width, meta.height)?.save(&png)?;
    std::fs::write(&raw_out, &raw)?;
    let sidecar = serde_json::json!({
        "instance": inst.name,
        "raw": raw_out.file_name().map(|n| n.to_string_lossy().into_owned()),
        "png": png.file_name().map(|n| n.to_string_lossy().into_owned()),
        "png_encoding": match meta.color_space {
            labhdr::vk_color_space::HDR10_ST2084 => "16-bit RGB: the swapchain's PQ code values (BT.2020 PQ), lossless",
            labhdr::vk_color_space::EXTENDED_SRGB_LINEAR => "16-bit RGB: converted from scRGB to BT.2020 PQ (derived; the .raw is exact)",
            _ => "16-bit RGB: the sRGB values",
        },
        "width": meta.width, "height": meta.height,
        "format": meta.format, "format_name": meta.format_name,
        "color_space": meta.color_space, "color_space_name": meta.color_space_name,
        "color_space_description": labhdr::color_space_name(meta.color_space),
        "bytes_per_pixel": meta.bytes_per_pixel, "row_bytes": meta.row_bytes,
        "hdr_metadata": meta.hdr_metadata,
        "stats": stats,
        "captured": chrono::Utc::now(),
    });
    std::fs::write(&json_out, serde_json::to_vec_pretty(&sidecar)?)?;
    // The session dir is tmpfs: don't keep full frames there.
    let _ = std::fs::remove_file(&raw_src);
    let _ = std::fs::remove_file(&meta_src);
    Ok(HdrShot { png, raw: raw_out, json: json_out, meta, stats })
}

pub fn print_shot(s: &HdrShot) {
    println!("{}", s.png.display());
    println!("  {} (exact, {}), {} (sidecar)", s.raw.display(), s.meta.format_name, s.json.display());
    println!("  {}x{} {} — {}", s.meta.width, s.meta.height, s.meta.format_name, labhdr::color_space_name(s.meta.color_space));
    let t = &s.stats;
    println!(
        "  max {:.0} nits (brightest channel {:.0}), avg {:.0}, p99 {:.0}; {:.1}% above 203 nits, {:.1}% above 1000",
        t.max_nits,
        t.max_channel_nits,
        t.avg_nits,
        t.p99_nits,
        t.above_203 * 100.0,
        t.above_1000 * 100.0
    );
}
