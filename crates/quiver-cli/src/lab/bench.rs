//! `quiver lab bench`: A/B runs over config variants (quiver_core::lab::bench has the model).
//!
//! For each round, each variant in turn: apply its assignments (live, waiting for the mod's
//! reload when the manifest says how; or `--relaunch`, waiting for `--ready`), settle,
//! take `--samples` screenshots (metrics over `--crop`) and read `--log-metric` numbers
//! the log gained meanwhile. The host's state is kept per sample window. The variants'
//! original values are put back at the end (also after a failure).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_core::lab::bench::{LogMetric, Variant, VariantResult, schedule, table};
use quiver_core::lab::image::{Crop, crop, series};
use quiver_core::lab::ready::Want;
use quiver_core::lab::{Lab, configset, host, procs};

use super::{inst_of, log_len, log_since, ops, set_and_wait, sup};

#[derive(clap::Args)]
pub struct BenchArgs {
    pub instance: String,
    /// `LABEL=FILE:[section]key=value[;…]` (repeat; an empty list is the baseline).
    #[arg(long = "variant", required = true)]
    pub variants: Vec<String>,
    /// Rounds: each runs every variant once (A B A B …).
    #[arg(long, default_value_t = 2)]
    pub rounds: usize,
    /// Screenshots per variant per round.
    #[arg(long, default_value_t = 4)]
    pub samples: usize,
    /// Seconds between screenshots.
    #[arg(long, default_value_t = 0.35)]
    pub interval: f64,
    /// Seconds to settle after a variant is applied (after the reload line, if any).
    #[arg(long, default_value_t = 4.0)]
    pub settle: f64,
    /// Image metrics over this crop (`WxH+X+Y` or fractions `x,y,w,h`); none: no shots.
    #[arg(long)]
    pub crop: Option<String>,
    /// Numbers from the mod's log: `NAME=REGEX` with one capture group (repeat).
    #[arg(long = "log-metric")]
    pub log_metrics: Vec<String>,
    /// The log the metrics are read from (default: the manifest's log_file).
    #[arg(long)]
    pub log: Option<String>,
    /// Relaunch the game for each variant instead of applying it live.
    #[arg(long)]
    pub relaunch: bool,
    /// With --relaunch: the state to wait for after each launch.
    #[arg(long, default_value = "frame")]
    pub ready: String,
    /// Take the GPU lock for the whole run (`quiver lab gpu`).
    #[arg(long)]
    pub gpu: bool,
    /// Where results go (default: the instance's lab/bench/<time>).
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

pub fn run(lab: &Lab, source: &str, a: BenchArgs) -> Result<()> {
    if a.gpu && std::env::var_os("QUIVER_LAB_GPU_HELD").is_none() {
        // Re-run under the GPU lock.
        let mut cmd: Vec<String> = std::env::args().filter(|x| x != "--gpu").collect();
        cmd[0] = std::env::current_exe()?.display().to_string();
        let mut full = vec!["lab".to_owned(), "gpu".into(), "--for".into(), a.instance.clone(), "--".into()];
        full.extend(cmd);
        let st = std::process::Command::new(std::env::current_exe()?).args(&full[..]).status()?;
        std::process::exit(st.code().unwrap_or(1));
    }
    let (inst, m) = inst_of(lab, &a.instance)?;
    let variants: Vec<Variant> = a.variants.iter().map(|v| Variant::parse(v)).collect::<Result<_>>()?;
    let metrics: Vec<LogMetric> = a.log_metrics.iter().map(|v| LogMetric::parse(v)).collect::<Result<_>>()?;
    let crop_spec = a.crop.as_deref().map(Crop::parse).transpose()?;
    let log_path = match a.log.as_ref().or(m.log_file.as_ref()) {
        Some(l) => Some(m.resolve_file(&inst, l)?),
        None if !metrics.is_empty() => bail!("--log-metric needs --log or the manifest's log_file"),
        None => None,
    };
    if crop_spec.is_none() && metrics.is_empty() {
        bail!("nothing to measure: give --crop (image metrics) and/or --log-metric");
    }
    let out = a.out.clone().unwrap_or_else(|| inst.lab().join("bench").join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()));
    std::fs::create_dir_all(&out)?;
    let live_wait = !a.relaunch && m.reload_pattern.is_some() && m.log_file.is_some();
    // What to put back: every key any variant touches, as it is now.
    let mut originals: Vec<configset::Assign> = Vec::new();
    let mut results: Vec<VariantResult> = variants.iter().map(|v| VariantResult { label: v.label.clone(), ..Default::default() }).collect();
    let run = (|| -> Result<()> {
        for (n, &vi) in schedule(variants.len(), a.rounds).iter().enumerate() {
            let v = &variants[vi];
            let round = n / variants.len() + 1;
            eprintln!("bench: round {round}, variant {}", v.label);
            if a.relaunch {
                configset::apply(&inst, &m, &v.assigns).map(|done| remember(&mut originals, &done))?;
                let r = sup::request(lab, &inst, &sup::SupRequest::Launch { source: source.into(), mod_dir: None, no_mod: false, args: Vec::new(), clock: None })?;
                let id = r["run"].as_u64().map(|x| x as u32);
                super::wait_for(&inst, &Want::parse(&a.ready), id, Duration::from_secs(600))?;
            } else {
                let done = set_and_wait(&inst, &m, &v.assigns, live_wait, Duration::from_secs(20))?;
                remember(&mut originals, &done);
            }
            std::thread::sleep(Duration::from_secs_f64(a.settle));
            let res = &mut results[vi];
            res.host.push(host::snapshot(&procs::scan(), &ops::containers()));
            let from = log_path.as_ref().map(|p| log_len(p)).unwrap_or(0);
            let t0 = Instant::now();
            if let Some(c) = &crop_spec {
                let mut imgs = Vec::new();
                for i in 0..a.samples {
                    let f = out.join(format!("{}-r{round}-{i}.png", v.label));
                    ops::shot(&inst, &f)?;
                    imgs.push(crop(&quiver_core::lab::image::load_gray(&f)?, Some(c)));
                    res.shots.push(f.display().to_string());
                    std::thread::sleep(Duration::from_secs_f64(a.interval));
                }
                let s = series(&imgs)?;
                res.samples.entry("sharpness".into()).or_default().push(s.sharpness);
                res.samples.entry("mean_level".into()).or_default().push(s.mean_level);
                if let Some(n) = s.temporal_noise {
                    res.samples.entry("temporal_noise".into()).or_default().push(n);
                }
            } else {
                std::thread::sleep(Duration::from_secs_f64(a.interval * a.samples as f64));
            }
            if let Some(p) = &log_path {
                // At least a second of log, even with few shots.
                if let Some(rest) = Duration::from_secs(1).checked_sub(t0.elapsed()) {
                    std::thread::sleep(rest);
                }
                let text = log_since(p, from);
                for lm in &metrics {
                    let vals = lm.values(&text);
                    if vals.is_empty() {
                        res.notes.push(format!("round {round}: no '{}' lines in {}", lm.name, p.display()));
                    }
                    res.samples.entry(lm.name.clone()).or_default().extend(vals);
                }
            }
        }
        Ok(())
    })();
    // Put the keys back as they were before the first variant.
    if !originals.is_empty()
        && let Err(e) = configset::apply(&inst, &m, &originals)
    {
        eprintln!("bench: couldn't restore the original values: {e:#}");
    }
    run?;
    // Under a lab limit without the virtual clock, wall-clock numbers measure the limit.
    let run_id = super::read_state(&inst).run.map(|r| r.id);
    let vclock = super::limits::run_has_virtual_clock(&inst, run_id);
    let throttled = super::limits::status_of(&inst).filter(|s| s.active()).map(|s| (s.effective.describe(), super::limits::throttle_note(&inst, vclock)));
    let label = throttled.as_ref().filter(|_| !vclock).map(|_| quiver_core::lab::limits::THROTTLED_LABEL);
    let report = serde_json::json!({ "instance": inst.name, "game": m.game, "display": m.display, "variants": variants, "results": results,
        "stats": results.iter().map(|r| (r.label.clone(), r.stats())).collect::<std::collections::BTreeMap<_, _>>(),
        "limits": throttled.as_ref().map(|(d, _)| d), "virtual_clock": vclock, "timing": label.unwrap_or(if vclock { "virtual clock (fixed step)" } else { "wall clock" }) });
    std::fs::write(out.join("bench.json"), serde_json::to_vec_pretty(&report)?).context("write bench.json")?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if let Some(l) = label {
            println!("[{l}] frame rates and timings below measure the lab limit, not the variants");
        }
        print!("{}", table(&results));
        for r in &results {
            for n in &r.notes {
                println!("{}: {n}", r.label);
            }
        }
        if let Some((_, Some(n))) = &throttled {
            println!("{n}");
        }
        println!("{}", out.join("bench.json").display());
    }
    Ok(())
}

/// Keep the first-seen original of each key (the value before the bench touched it).
fn remember(originals: &mut Vec<configset::Assign>, done: &[configset::Done]) {
    for d in done {
        if originals.iter().any(|o| o.file == d.file && o.target == d.target && o.key == d.key) {
            continue;
        }
        if let Some(u) = d.undo() {
            originals.push(u);
        }
    }
}
