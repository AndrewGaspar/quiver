//! `quiver queue …`, `pause`, `resume`, `update`, `verify` and `install --dry-run`.

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::downloads::{DownloadSettings, InstallPreview, QueueEntry, QueueMove, QueueState, UpdatePreview, VerifyReport};
use quiver_api::{Client, Request};
use quiver_core::install::human_bytes;

use crate::{follow, print_json, resolve};

#[derive(Subcommand)]
pub enum QueueAction {
    /// Move a game up, down or to the top of the queue.
    Move { game: String, to: QueueMove },
    /// Take a game out of the queue (its downloaded files stay; install again to resume).
    Remove { game: String },
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

pub async fn queue(client: &Client, action: Option<QueueAction>, json: bool) -> Result<()> {
    match action {
        None => {
            let q: Vec<QueueEntry> = client.call(Request::Queue).await?;
            if json {
                return print_json(&q);
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
                    e.product_id
                );
                if let Some(err) = &e.error {
                    println!("      {err}");
                }
            }
        }
        Some(QueueAction::Move { game, to }) => {
            let e = resolve(client, &game).await?;
            let q: Vec<QueueEntry> = client.call(Request::QueueMove { product_id: e.product_id, to }).await?;
            for e in q {
                println!("{:>2}  {:<11} {}", e.position, state_word(e.state), e.name);
            }
        }
        Some(QueueAction::Remove { game }) => {
            let e = resolve(client, &game).await?;
            let () = client.call(Request::QueueRemove { product_id: e.product_id }).await?;
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
    let e = resolve(client, game).await?;
    let () = client.call(Request::QueuePause { product_id: e.product_id }).await?;
    println!("pausing {} (quiver resume {} to continue)", e.name, e.product_id);
    Ok(())
}

pub async fn resume(client: &Client, game: &str) -> Result<()> {
    let e = resolve(client, game).await?;
    let id: u64 = client.call(Request::QueueResume { product_id: e.product_id }).await?;
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
    if files {
        for (label, list) in [("changed", &u.changed), ("added", &u.added), ("removed", &u.removed)] {
            for p in list {
                println!("  {label:<8} {p}");
            }
        }
    }
}

pub async fn dry_run(client: &Client, game: &str, language: Option<String>, json: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let p: InstallPreview = client.call(Request::InstallPreview { product_id: e.product_id, language }).await?;
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
    let e = resolve(client, game).await?;
    if preview {
        let u: UpdatePreview = client.call(Request::UpdatePreview { product_id: e.product_id }).await?;
        if json {
            return print_json(&u);
        }
        print_update(&u, true);
        return Ok(());
    }
    let events = client.subscribe().await?;
    let id: u64 = client.call(Request::Update { product_id: e.product_id }).await?;
    eprintln!("updating {} (job {id})", e.name);
    if !detach {
        follow(client, events, id).await?;
    }
    Ok(())
}

pub async fn verify(client: &Client, game: &str, repair: bool, detach: bool, json: bool) -> Result<()> {
    let e = resolve(client, game).await?;
    let events = client.subscribe().await?;
    let id: u64 = client.call(Request::Verify { product_id: e.product_id, repair }).await?;
    eprintln!("{} {} (job {id})", if repair { "repairing" } else { "verifying" }, e.name);
    if detach {
        return Ok(());
    }
    follow(client, events, id).await?;
    let r: Option<VerifyReport> = client.call(Request::VerifyResult { product_id: e.product_id }).await?;
    match r {
        Some(r) if json => print_json(&r)?,
        Some(r) if !r.is_ok() && !repair => {
            println!("quiver verify {} --repair re-downloads only those", e.product_id);
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
}
