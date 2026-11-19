//! `quiver queue …`, `pause`, `resume`, `update`, `verify` and `install --dry-run`.

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::downloads::{DownloadSettings, InstallPreview, QueueEntry, QueueMove, QueueState, SampleState, Throughput, UpdatePreview, VerifyReport};
use quiver_api::{Client, Request, StoreId};
use quiver_core::install::human_bytes;

use crate::{follow, game_label, print_json, resolve_any, resolve_installable};

#[derive(Subcommand)]
pub enum QueueAction {
    /// Move a game up, down or to the top of the queue.
    Move { game: String, to: QueueMove },
    /// Take a game out of the queue (its downloaded files stay; install again to resume).
    Remove { game: String },
    /// A queued game's throughput as quiverd sampled it: one sample a second, up to the
    /// last ten minutes, pauses included (since quiverd started).
    History {
        game: String,
        /// Only the newest this many samples.
        #[arg(long)]
        last: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Show or change download settings.
    Settings {
        /// Downloads that run at the same time.
        #[arg(long)]
        max_active: Option<usize>,
        /// Bandwidth limit across all downloads, e.g. 20M or 500K (bytes per second);
        /// 0 or off removes it.
        #[arg(long)]
        limit: Option<String>,
        /// Queue updates for installed games automatically.
        #[arg(long)]
        auto_update: Option<OnOff>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

/// `20M`, `1.5G`, `500K`, `1000` (bytes per second); `0`/`off`: no limit.
fn parse_rate(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("none") {
        return Ok(0);
    }
    let lower = s.to_ascii_lowercase();
    let t = lower.trim_end_matches("/s").trim_end_matches('b').trim_end_matches('i');
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1u64 << 10),
        Some('m') => (&t[..t.len() - 1], 1 << 20),
        Some('g') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    let n: f64 = num.trim().parse().map_err(|_| anyhow::anyhow!("bad rate '{s}' (e.g. 20M, 500K, off)"))?;
    if n < 0.0 {
        bail!("bad rate '{s}'");
    }
    Ok((n * mult as f64) as u64)
}

fn state_word(s: QueueState) -> &'static str {
    match s {
        QueueState::Queued => "queued",
        QueueState::Downloading => "downloading",
        QueueState::Paused => "paused",
        QueueState::Verifying => "verifying",
        QueueState::Done => "done",
        QueueState::Failed => "failed",
    }
}

fn print_settings(s: &DownloadSettings) {
    println!(
        "{} at a time · {} · auto-update {}",
        s.max_active,
        s.bandwidth_limit.map(|b| format!("limit {}/s", human_bytes(b))).unwrap_or_else(|| "no bandwidth limit".into()),
        if s.auto_update { "on" } else { "off" }
    );
}

/// Samples `quiver queue --json` adds to each item: the last minute.
const QUEUE_JSON_SAMPLES: usize = 60;

/// The queue as JSON, each item with history also carrying it as `throughput` (the
/// existing fields as they were).
fn with_throughput(q: &[QueueEntry], history: &[Throughput]) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(q)?;
    for (item, e) in v.as_array_mut().into_iter().flatten().zip(q) {
        if let (Some(obj), Some(t)) = (item.as_object_mut(), history.iter().find(|t| t.game == e.game_ref())) {
            obj.insert("throughput".into(), serde_json::to_value(t)?);
        }
    }
    Ok(v)
}

/// One character a column, at most 60 columns (wider histories averaged); blank in a gap.
fn sparkline(t: &Throughput) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let per = t.samples.len().div_ceil(60).max(1);
    let cols: Vec<Option<u64>> = t
        .samples
        .chunks(per)
        .map(|c| {
            let ran: Vec<u64> = c.iter().filter(|s| !s.state.is_gap()).map(|s| s.bytes).collect();
            (!ran.is_empty()).then(|| ran.iter().sum::<u64>() / ran.len() as u64)
        })
        .collect();
    let top = cols.iter().flatten().copied().max().unwrap_or(0).max(1);
    cols.iter().map(|c| c.map_or(' ', |b| BARS[((b * 7).div_ceil(top) as usize).min(7)])).collect()
}

fn print_history(name: &str, t: &Throughput) {
    let secs = |n: usize| n as u64 * t.interval_ms / 1000;
    let count = |st: SampleState| t.samples.iter().filter(|s| s.state == st).count();
    let moving: Vec<u64> = t.samples.iter().filter(|s| matches!(s.state, SampleState::Downloading | SampleState::Stalled | SampleState::Retrying)).map(|s| s.bytes).collect();
    let avg = moving.iter().sum::<u64>() / (moving.len() as u64).max(1);
    let peak = moving.iter().copied().max().unwrap_or(0);
    let retries: u64 = t.samples.iter().map(|s| s.retries).sum();
    println!(
        "{name}: the last {} s, a sample every {} ms, up to {}",
        secs(t.samples.len()),
        t.interval_ms,
        t.at.with_timezone(&chrono::Local).format("%H:%M:%S")
    );
    let mut parts = vec![format!("average {}/s", human_bytes(avg)), format!("peak {}/s", human_bytes(peak))];
    for (st, word) in [
        (SampleState::Stalled, "stalled"),
        (SampleState::Retrying, "retrying"),
        (SampleState::Verifying, "verifying"),
        (SampleState::Paused, "paused"),
        (SampleState::Waiting, "waiting"),
    ] {
        if count(st) > 0 {
            parts.push(format!("{word} {} s", secs(count(st))));
        }
    }
    if retries > 0 {
        parts.push(format!("{retries} {}", if retries == 1 { "retry" } else { "retries" }));
    }
    println!("  {}", parts.join(" · "));
    println!("  {}", sparkline(t));
}

pub async fn queue(client: &Client, action: Option<QueueAction>, json: bool) -> Result<()> {
    match action {
        None => {
            let q: Vec<QueueEntry> = client.call(Request::Queue).await?;
            if json {
                // Each item that has run since quiverd started also gets its last minute
                // of throughput (`throughput`); a daemon from before has none.
                let history: Vec<Throughput> = client.call(Request::DownloadThroughput { last: Some(QUEUE_JSON_SAMPLES) }).await.unwrap_or_default();
                return print_json(&with_throughput(&q, &history)?);
            }
            let s: DownloadSettings = client.call(Request::DownloadSettings { max_active: None, bandwidth_limit: None, auto_update: None }).await?;
            print_settings(&s);
            if q.is_empty() {
                println!("the queue is empty");
            }
            for e in &q {
                let progress = if e.total_bytes > 0 {
                    format!(
                        "{}/{} {:>3}%",
                        human_bytes(e.done_bytes),
                        human_bytes(e.total_bytes),
                        100 * e.done_bytes / e.total_bytes.max(1)
                    )
                } else {
                    String::new()
                };
                let speed = if e.state == QueueState::Downloading && e.speed > 0 { format!("{}/s", human_bytes(e.speed)) } else { String::new() };
                println!(
                    "{:>2}  {:<11} {:<7} {:<24} {:<11} {} ({})",
                    e.position,
                    state_word(e.state),
                    format!("{:?}", e.kind).to_lowercase(),
                    progress,
                    speed,
                    e.name,
                    game_label(&e.game_ref())
                );
                if let Some(err) = &e.error {
                    println!("      {err}");
                }
            }
        }
        Some(QueueAction::Move { game, to }) => {
            let e = resolve_installable(client, &game).await?;
            let q: Vec<QueueEntry> = client.call(Request::QueueMove { game: e.game.into(), to }).await?;
            for e in q {
                println!("{:>2}  {:<11} {}", e.position, state_word(e.state), e.name);
            }
        }
        Some(QueueAction::History { game, last, json: own_json }) => {
            let e = resolve_installable(client, &game).await?;
            let history: Vec<Throughput> = client.call(Request::DownloadThroughput { last }).await?;
            let Some(t) = history.into_iter().find(|t| t.game == e.game) else {
                bail!("no throughput history for {}: it hasn't downloaded since quiverd started, or isn't in the queue", e.name);
            };
            if json || own_json {
                return print_json(&t);
            }
            print_history(&e.name, &t);
        }
        Some(QueueAction::Remove { game }) => {
            let e = resolve_installable(client, &game).await?;
            let () = client.call(Request::QueueRemove { game: e.game.into()}).await?;
            println!("removed {} from the queue (files kept; install again to resume)", e.name);
        }
        Some(QueueAction::Settings { max_active, limit, auto_update }) => {
            let bandwidth_limit = limit.as_deref().map(parse_rate).transpose()?;
            let s: DownloadSettings = client
                .call(Request::DownloadSettings { max_active, bandwidth_limit, auto_update: auto_update.map(|a| matches!(a, OnOff::On)) })
                .await?;
            if json {
                return print_json(&s);
            }
            print_settings(&s);
        }
    }
    Ok(())
}

pub async fn pause(client: &Client, game: &str) -> Result<()> {
    let e = resolve_installable(client, game).await?;
    let () = client.call(Request::QueuePause { game: e.game.into()}).await?;
    println!("pausing {} (quiver resume {} to continue)", e.name, game_label(&e.game));
    Ok(())
}

pub async fn resume(client: &Client, game: &str) -> Result<()> {
    let e = resolve_installable(client, game).await?;
    let id: u64 = client.call(Request::QueueResume { game: e.game.into()}).await?;
    println!("resumed {} (job {id})", e.name);
    Ok(())
}

fn print_update(u: &UpdatePreview, files: bool) {
    let v = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
    if u.up_to_date {
        println!("{} is up to date ({})", u.name, v(&u.installed_version));
        return;
    }
    println!("{}: {} → {}", u.name, v(&u.installed_version), v(&u.latest_version));
    println!(
        "download {}, write {}; {} changed, {} added, {} removed, {} unchanged",
        human_bytes(u.download_bytes),
        human_bytes(u.write_bytes),
        u.changed.len(),
        u.added.len(),
        u.removed.len(),
        u.unchanged
    );
    for n in &u.notes {
        println!("  {n}");
    }
    if files {
        for (label, list) in [("changed", &u.changed), ("added", &u.added), ("removed", &u.removed)] {
            for p in list {
                println!("  {label:<8} {p}");
            }
        }
    }
}

pub async fn dry_run(client: &Client, game: &str, language: Option<String>, json: bool) -> Result<()> {
    let e = resolve_installable(client, game).await?;
    let p: InstallPreview = client.call(Request::InstallPreview { game: e.game.into(), language }).await?;
    if json {
        return print_json(&p);
    }
    println!("{} {} → {}", p.name, p.version.as_deref().unwrap_or(""), p.dest.display());
    println!("language {}: download {}, installs to {}", p.language, human_bytes(p.download_bytes), human_bytes(p.install_bytes));
    println!(
        "still to download {}, to write {}; {} free, {} kept as margin: {}",
        human_bytes(p.remaining_download_bytes),
        human_bytes(p.required_bytes),
        human_bytes(p.free_bytes),
        human_bytes(p.margin_bytes),
        if p.fits { "fits" } else { "DOES NOT FIT" }
    );
    if !p.languages.is_empty() {
        let langs: Vec<String> = p
            .languages
            .iter()
            .map(|l| if l.install_bytes > 0 { format!("{} (+{})", l.code, human_bytes(l.install_bytes)) } else { l.code.clone() })
            .collect();
        println!("languages: {}", langs.join(", "));
    }
    if let Some(u) = &p.update {
        print_update(u, false);
    }
    Ok(())
}

pub async fn update(client: &Client, game: &str, preview: bool, detach: bool, json: bool) -> Result<()> {
    // A game its store's client updates (Steam): show the client's update, or ask the
    // client (quiverd opens its Downloads page).
    let any = resolve_any(client, game).await?;
    if let Some(c) = any.client.as_ref().filter(|_| !matches!(any.store(), StoreId::Ubisoft | StoreId::Epic)) {
        let label = any.store().label();
        if preview || json {
            if json {
                return print_json(&c.update);
            }
            match &c.update {
                Some(u) => {
                    let builds = u.target_build_id.as_ref().map(|t| format!(" (build {} → {t})", c.build_id.as_deref().unwrap_or("?"))).unwrap_or_default();
                    println!("{}: {}{builds}", any.name, u.describe(label));
                }
                None => println!("{}: no update waiting in {label}{}", any.name, c.build_id.as_ref().map(|b| format!(" (build {b})")).unwrap_or_default()),
            }
            println!("{label} updates it itself; `quiver update {}` opens {label}'s downloads", game_label(&any.game));
            return Ok(());
        }
        let events = client.subscribe().await?;
        let id: u64 = client.call(Request::Update { game: any.game.into() }).await?;
        return follow(client, events, id).await;
    }
    let e = resolve_installable(client, game).await?;
    if preview {
        let u: UpdatePreview = client.call(Request::UpdatePreview { game: e.game.into()}).await?;
        if json {
            return print_json(&u);
        }
        print_update(&u, true);
        return Ok(());
    }
    let events = client.subscribe().await?;
    let id: u64 = client.call(Request::Update { game: e.game.into()}).await?;
    eprintln!("updating {} (job {id})", e.name);
    if !detach {
        follow(client, events, id).await?;
    }
    Ok(())
}

pub async fn verify(client: &Client, game: &str, repair: bool, detach: bool, json: bool) -> Result<()> {
    let e = resolve_installable(client, game).await?;
    let events = client.subscribe().await?;
    let id: u64 = client.call(Request::Verify { game: e.game.into(), repair }).await?;
    eprintln!("{} {} (job {id})", if repair { "repairing" } else { "verifying" }, e.name);
    if detach {
        return Ok(());
    }
    follow(client, events, id).await?;
    let r: Option<VerifyReport> = client.call(Request::VerifyResult { game: e.game.into()}).await?;
    match r {
        Some(r) if json => print_json(&r)?,
        Some(r) if !r.is_ok() && !repair => {
            println!("quiver verify {} --repair re-downloads only those", game_label(&e.game));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_rate;

    #[test]
    fn rates() {
        assert_eq!(parse_rate("20M").unwrap(), 20 << 20);
        assert_eq!(parse_rate("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_rate("500KiB/s").unwrap(), 500 << 10);
        assert_eq!(parse_rate("1000").unwrap(), 1000);
        assert_eq!(parse_rate("off").unwrap(), 0);
        assert_eq!(parse_rate("0").unwrap(), 0);
        assert!(parse_rate("fast").is_err());
    }

    #[test]
    fn queue_json_adds_throughput() {
        use super::*;
        use quiver_api::downloads::ThroughputSample;
        let item = |product_id: u32| -> QueueEntry {
            serde_json::from_value(serde_json::json!({"product_id": product_id, "name": "G", "kind": "install", "state": "downloading", "source": "cli", "added": "2026-11-19T00:00:00Z", "speed": 7}))
                .unwrap()
        };
        let q = vec![item(1), item(2)];
        let s = |bytes, state| ThroughputSample { bytes, state, retries: 0 };
        let t = Throughput {
            game: quiver_api::GameRef::ubisoft(2),
            interval_ms: 1000,
            at: "2026-11-19T00:01:00Z".parse().unwrap(),
            samples: vec![s(10, SampleState::Downloading), s(0, SampleState::Paused), s(70, SampleState::Downloading), s(0, SampleState::Stalled)],
        };
        let v = with_throughput(&q, std::slice::from_ref(&t)).unwrap();
        // Only added: the existing fields stay as they were.
        assert!(v[0].get("throughput").is_none());
        assert_eq!(v[0], serde_json::to_value(&q[0]).unwrap());
        assert_eq!(v[1]["speed"], 7);
        assert_eq!(v[1]["throughput"]["samples"][2]["bytes"], 70);
        assert_eq!(sparkline(&t), "▂ █▁");
    }
}
