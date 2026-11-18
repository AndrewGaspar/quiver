//! `quiver mods <game> options …`: mod options (design/MOD-CONFIG.md). Values belong to a
//! profile (`--profile`, default: the game's default profile; `vanilla` for vanilla) and are
//! changes from the defaults, with their own history.
//!
//! The agent workflow for shipping options with a mod version:
//!
//! ```sh
//! quiver mods origins options lint build/mod          # schema vs aco_dlss.ini: undescribed keys, bad defaults
//! quiver --source aco-dlss mods origins version add DLSS build/mod --notes "npc_distance; FG pacing"
//! #  notes: options: schema shipped with v5 (41 options, 18 keys undescribed); dormant values
//! quiver --source aco-dlss mods origins options set DLSS world.npc_density=5 --profile DLSS --no-live
//! ```

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::Value;
use quiver_api::mods::is_human_source;
use quiver_api::options::{
    Apply, InvokeResult, KeysReport, Level, Live, LintReport, OptionDiff, OptionOp, OptionState, OptionsChange, OptionsRequest, OptionsRevision, Outcome,
    Pending, SchemaInfo, SessionDetail, Severity, SurfaceState, ValueSource,
};
use quiver_api::{Client, Request};

use crate::mods::resolve_installed;

#[derive(Subcommand)]
pub enum OptionsAction {
    /// Every options surface of the profile (the default): basic options; --advanced or
    /// --all for more, --raw for keys without a description.
    List {
        #[arg(long)]
        advanced: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        raw: bool,
        /// Only options the profile changes.
        #[arg(long)]
        changed: bool,
    },
    /// One option: value, default, where it comes from, how it applies.
    Get { surface: String, id: String },
    /// Set options: SURFACE id=value… (`id=` resets). Saved to the profile and, if the game
    /// is running, delivered live where the mod allows it; each change says what happened.
    Set {
        surface: String,
        #[arg(required = true, value_name = "ID=VALUE")]
        changes: Vec<String>,
        /// For the running session only (not saved).
        #[arg(long, conflicts_with = "no_live")]
        live_only: bool,
        /// Save without touching the running game.
        #[arg(long)]
        no_live: bool,
        /// Why (recorded with the revision).
        #[arg(short, long, alias = "reason")]
        message: Option<String>,
        /// Confirm dangerous options without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Back to the default (deletes the profile's value). --all: every value of SURFACE.
    Reset {
        surface: String,
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        no_live: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Apply a preset (one change).
    Preset {
        surface: String,
        preset: String,
        #[arg(long)]
        no_live: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Set a key the schema doesn't describe: SURFACE section/key=value (`=` alone resets).
    Raw {
        surface: String,
        #[arg(required = true, value_name = "SECTION/KEY=VALUE")]
        changes: Vec<String>,
        #[arg(long)]
        no_live: bool,
    },
    /// What the profile changes: against the defaults, another profile, or a revision (rN).
    Diff {
        #[arg(long)]
        against: Option<String>,
    },
    /// The profile's options revisions.
    History,
    /// Make an earlier revision current again (rN; r0: no values).
    Restore { revision: String },
    /// Put back the values a session launched with (e.g. after a crash).
    Revert {
        #[arg(long)]
        session: String,
    },
    /// Every key binding in play for the profile, with conflicts.
    Keys,
    /// A mod version's schema, where it comes from, and lint findings; --set FILE writes a
    /// local schema for the mod.
    Schema {
        #[arg(value_name = "MOD")]
        mod_id: String,
        #[arg(long)]
        version: Option<String>,
        /// Show findings only.
        #[arg(long)]
        lint: bool,
        #[arg(long, value_name = "FILE")]
        set: Option<PathBuf>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Guess a schema from one of a mod's files (TOML on stdout).
    Infer {
        #[arg(value_name = "MOD")]
        mod_id: String,
        file: String,
    },
    /// Check a package or plain folder's quiver-options.toml against its files.
    Lint { dir: PathBuf },
    /// Running sessions with their options addressing (session, profile, revision, pids).
    Sessions,
    /// Run a mod's action in the running game (needs the mod SDK).
    Invoke {
        surface: String,
        action: String,
        arg: Option<String>,
    },
}

fn who(source: &str) -> String {
    match source {
        "cli" | "gui" => "you".into(),
        "sight" => "you, in game".into(),
        "in-game" => "in game".into(),
        "game" => "the game".into(),
        "file" => "hand edit".into(),
        s => s.to_owned(),
    }
}

fn from_text(o: &OptionState) -> String {
    match &o.from {
        ValueSource::Default => "default".into(),
        ValueSource::ModPatch { mod_id } => format!("set by mod {mod_id}"),
        ValueSource::Profile { revision, source } => format!("{}, r{revision} (default {})", who(source), o.def.display(&o.default)),
        ValueSource::Session { .. } => format!("this session only (saved: {})", o.def.display(&o.default)),
        ValueSource::Tweak { tweak, scope, revision } => format!("Quiver Tweaks {} r{revision} ({tweak})", scope.label().to_lowercase()),
        ValueSource::Scope { scope, revision, source } => format!("{}, {}, r{revision}", scope.label().to_lowercase(), who(source)),
    }
}

fn apply_text(o: &OptionState) -> String {
    let base = match o.def.apply {
        Apply::Live => match &o.def.latency {
            Some(l) => format!("live, {l}"),
            None => "live".into(),
        },
        Apply::At => format!("at {}", o.def.at.as_deref().unwrap_or("a later moment")),
        Apply::Restart => "restart".into(),
        Apply::Launch => "next launch".into(),
    };
    match &o.pending {
        Pending::None => base,
        Pending::Restart => format!("{base} (needs a restart)"),
        Pending::At { at } => format!("{base} (pending until {at})"),
        Pending::Reread => format!("{base} (not delivered yet)"),
    }
}

fn changed(o: &OptionState) -> bool {
    !matches!(o.from, ValueSource::Default | ValueSource::ModPatch { .. } | ValueSource::Tweak { .. })
}

fn print_surface(s: &SurfaceState, max: Level, raw: bool, only_changed: bool) {
    let file = s.files.first().map(|f| f.rel.rsplit('/').next().unwrap_or(&f.rel).to_owned()).unwrap_or_default();
    let live = match (s.live.channel, s.live.connected) {
        (quiver_api::options::LiveChannel::File, true) => " · running: live through the file",
        (quiver_api::options::LiveChannel::Sdk, true) => " · running: live through the mod SDK",
        _ => "",
    };
    println!("{} {} · {file} · {}{live}", s.title, s.version_label.as_deref().unwrap_or(""), s.schema_note);
    let width = s.options.keys().map(|k| k.len()).max().unwrap_or(10).clamp(10, 34);
    let (mut hidden_adv, mut hidden_dbg, mut shown) = (0, 0, 0);
    for p in &s.pages {
        let mut lines = Vec::new();
        for g in &p.groups {
            for id in &g.options {
                let Some(o) = s.options.get(id) else { continue };
                if o.def.level > max {
                    match o.def.level {
                        Level::Debug => hidden_dbg += 1,
                        _ => hidden_adv += 1,
                    }
                    continue;
                }
                if only_changed && !changed(o) {
                    continue;
                }
                let mut flags = String::new();
                if !o.visible {
                    flags.push_str(" (hidden: condition)");
                }
                if o.def.experimental {
                    flags.push_str(" EXP");
                }
                if o.def.danger.is_some() {
                    flags.push_str(" DANGER");
                }
                if let Some(d) = &o.dormant {
                    flags.push_str(&format!(" [{d}]"));
                }
                let val = o.def.display(&o.value);
                lines.push(format!("    {id:<width$}  {val:<26}  {:<34}  {}{flags}", from_text(o), apply_text(o)));
                shown += 1;
            }
        }
        if !lines.is_empty() {
            println!("  {}", p.title);
            for l in lines {
                println!("{l}");
            }
        }
    }
    let mut tail = Vec::new();
    if hidden_adv > 0 {
        tail.push(format!("{hidden_adv} advanced"));
    }
    if hidden_dbg > 0 {
        tail.push(format!("{hidden_dbg} debug"));
    }
    let mut foot = String::new();
    if !tail.is_empty() {
        foot = format!("{} options hidden (--advanced, --all)", tail.join(", "));
    }
    if !s.raw.is_empty() && !raw {
        foot = format!("{foot}{}{} other keys (--raw)", if foot.is_empty() { "" } else { "; " }, s.raw.len());
    }
    if shown == 0 && only_changed {
        println!("  (no changes)");
    }
    if !foot.is_empty() {
        println!("  {foot}");
    }
    if raw && !s.raw.is_empty() {
        println!("  Other keys");
        for r in &s.raw {
            let mark = if r.changed.is_some() { format!("  (yours; file: {})", r.default) } else { String::new() };
            println!("    {:<width$}  {}{mark}", r.binding, r.text);
        }
    }
    if !s.presets.is_empty() {
        println!("  presets: {}", s.presets.iter().map(|p| format!("{} ({})", p.id, p.label)).collect::<Vec<_>>().join(", "));
    }
    for d in &s.dormant {
        println!("  dormant: {}={} ({})", d.id, quiver_api::options::scalar_text(&d.value), d.reason);
    }
    for l in &s.lint {
        println!("  {l}");
    }
}

fn print_change(game: &str, c: &OptionsChange) {
    if c.revision > 0 {
        println!("r{}: {}", c.revision, c.changes.join(", "));
    } else if !c.changes.is_empty() {
        println!("{} (not saved): {}", game, c.changes.join(", "));
    } else if c.outcomes.iter().all(|o| o.outcome == Outcome::Unchanged) {
        println!("nothing changed");
    }
    for o in &c.outcomes {
        let id = if o.id.is_empty() { o.surface.clone() } else { o.id.clone() };
        println!("  {id}: {}", o.outcome.describe());
    }
    for l in &c.live {
        println!("{l}");
    }
    for w in &c.warnings {
        eprintln!("note: {w}");
    }
}

/// Ask the person at the terminal (for danger options and agents' live changes).
fn ask(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt} Allow? [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn json_out<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn parse_change(surface: &str, s: &str) -> Result<OptionOp> {
    let (id, value) = s.split_once('=').with_context(|| format!("expected id=value, got '{s}'"))?;
    let id = id.trim().to_owned();
    Ok(if value.is_empty() { OptionOp::Reset { surface: surface.to_owned(), id } } else { OptionOp::Set { surface: surface.to_owned(), id, value: Value::String(value.to_owned()) } })
}

pub async fn run(client: &Client, source: &str, game: &str, profile: Option<String>, json: bool, action: Option<OptionsAction>) -> Result<()> {
    let g = resolve_installed(client, game).await?;
    let product_id = crate::mods::GameArg(g.game_ref());
    let call = |req: OptionsRequest| client.call::<Value>(Request::Options(req));
    let action = action.unwrap_or(OptionsAction::List { advanced: false, all: false, raw: false, changed: false });
    let set = |ops: Vec<OptionOp>, live: Live, message: Option<String>, confirmed: bool| {
        let profile = profile.clone();
        async move {
            let req = OptionsRequest::Set { game: product_id.into(), profile: profile.clone(), ops: ops.clone(), live, message: message.clone(), confirmed };
            match client.call::<OptionsChange>(Request::Options(req)).await {
                Err(e) if !confirmed && e.to_string().contains("confirmation") => {
                    let what = e.to_string();
                    let prompt = if is_human_source(source) { what.clone() } else { format!("'{source}' asks: {what}") };
                    if ask(&prompt)? {
                        client.call::<OptionsChange>(Request::Options(OptionsRequest::Set { game: product_id.into(), profile, ops, live, message, confirmed: true })).await
                    } else {
                        Err(e)
                    }
                }
                r => r,
            }
        }
    };
    match action {
        OptionsAction::List { advanced, all, raw, changed } => {
            let list: Vec<SurfaceState> = client.call(Request::Options(OptionsRequest::List { game: product_id.into(), profile: profile.clone(), level: None })).await?;
            if json {
                return json_out(&list);
            }
            if list.is_empty() {
                println!("{}: no options in {} (no mod in the stack has a schema; `options infer MOD FILE` guesses one)", g.name, profile.as_deref().unwrap_or("the default profile"));
            }
            let max = if all { Level::Debug } else if advanced { Level::Advanced } else { Level::Basic };
            for s in &list {
                print_surface(s, max, raw, changed);
            }
        }
        OptionsAction::Get { surface, id } => {
            let list: Vec<SurfaceState> = client.call(Request::Options(OptionsRequest::List { game: product_id.into(), profile: profile.clone(), level: None })).await?;
            let q = surface.to_lowercase();
            let s = list
                .iter()
                .find(|s| s.key == surface || s.surface.to_lowercase() == q || s.title.to_lowercase() == q || s.mod_id.as_deref() == Some(surface.as_str()))
                .with_context(|| format!("no surface '{surface}' (surfaces: {})", list.iter().map(|s| s.title.clone()).collect::<Vec<_>>().join(", ")))?;
            let o = s.options.get(&id).with_context(|| format!("{} has no option {id}", s.title))?;
            if json {
                return json_out(o);
            }
            println!("{} {id}: {}", s.title, o.def.display(&o.value));
            println!("  file text: {}", o.text);
            println!("  from: {}", from_text(o));
            println!("  applies: {}", apply_text(o));
            if let Some(h) = &o.def.help {
                println!("  {h}");
            }
            if let Some(d) = &o.def.danger {
                println!("  danger: {d}");
            }
        }
        OptionsAction::Set { surface, changes, live_only, no_live, message, yes } => {
            let ops = changes.iter().map(|c| parse_change(&surface, c)).collect::<Result<Vec<_>>>()?;
            let live = if live_only { Live::Only } else if no_live { Live::No } else { Live::IfRunning };
            let c = set(ops, live, message, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
            if c.outcomes.iter().any(|o| matches!(o.outcome, Outcome::Rejected { .. })) {
                bail!("some changes were rejected");
            }
        }
        OptionsAction::Reset { surface, ids, all, no_live, message } => {
            let ops: Vec<OptionOp> = if all {
                vec![OptionOp::ResetAll { surface }]
            } else if ids.is_empty() {
                bail!("name options to reset, or --all");
            } else {
                ids.into_iter().map(|id| OptionOp::Reset { surface: surface.clone(), id }).collect()
            };
            let c = set(ops, if no_live { Live::No } else { Live::IfRunning }, message, false).await?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
        }
        OptionsAction::Preset { surface, preset, no_live, message } => {
            let c = set(vec![OptionOp::Preset { surface, id: preset }], if no_live { Live::No } else { Live::IfRunning }, message, false).await?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
        }
        OptionsAction::Raw { surface, changes, no_live } => {
            let ops = changes
                .iter()
                .map(|c| {
                    let (b, v) = c.split_once('=').with_context(|| format!("expected section/key=value, got '{c}'"))?;
                    Ok(OptionOp::Raw { surface: surface.clone(), binding: b.to_owned(), value: (!v.is_empty()).then(|| v.to_owned()) })
                })
                .collect::<Result<Vec<_>>>()?;
            let c = set(ops, if no_live { Live::No } else { Live::IfRunning }, None, false).await?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
        }
        OptionsAction::Diff { against } => {
            let d: Vec<OptionDiff> = serde_json::from_value(call(OptionsRequest::Diff { game: product_id.into(), profile: profile.clone(), against: against.clone() }).await?)?;
            if json {
                return json_out(&d);
            }
            let other = against.unwrap_or_else(|| "default".into());
            if d.is_empty() {
                println!("{}: no differences from {other}", profile.as_deref().unwrap_or("the default profile"));
            }
            let mut by: BTreeMap<String, Vec<&OptionDiff>> = BTreeMap::new();
            for x in &d {
                by.entry(x.title.clone()).or_default().push(x);
            }
            for (title, list) in by {
                println!("{title}");
                for x in list {
                    println!("  {:<30} {:<24} {other}: {}", x.id, x.value, x.against);
                }
            }
        }
        OptionsAction::History => {
            let h: Vec<OptionsRevision> = serde_json::from_value(call(OptionsRequest::History { game: product_id.into(), profile: profile.clone() }).await?)?;
            if json {
                return json_out(&h);
            }
            if h.is_empty() {
                println!("no options revisions (every value is the default)");
            }
            for r in h.iter().rev() {
                let local = r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
                println!("r{:<4} {local}  {:<10} {}", r.revision, who(&r.source), r.message.as_deref().unwrap_or(""));
                for c in &r.changes {
                    println!("        {c}");
                }
            }
        }
        OptionsAction::Restore { revision } => {
            let n: u32 = revision.trim_start_matches(['r', 'R']).parse().with_context(|| format!("'{revision}' isn't a revision (rN)"))?;
            let confirmed = is_human_source(source) || ask(&format!("'{source}' asks to restore options revision r{n} of {}.", g.name))?;
            let c: OptionsChange = serde_json::from_value(call(OptionsRequest::Restore { game: product_id.into(), profile: profile.clone(), revision: n, confirmed }).await?)?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
        }
        OptionsAction::Revert { session } => {
            let confirmed = is_human_source(source) || ask(&format!("'{source}' asks to revert session {session}'s option changes in {}.", g.name))?;
            let c: OptionsChange = serde_json::from_value(call(OptionsRequest::RevertSession { game: product_id.into(), session, confirmed }).await?)?;
            if json {
                return json_out(&c);
            }
            print_change(&g.name, &c);
        }
        OptionsAction::Keys => {
            let k: KeysReport = serde_json::from_value(call(OptionsRequest::Keys { game: product_id.into(), profile: profile.clone() }).await?)?;
            if json {
                return json_out(&k);
            }
            for (i, b) in k.bindings.iter().enumerate() {
                let clash = k.conflicts.iter().find(|c| c.bindings.contains(&i)).map(|c| {
                    let others: Vec<String> = c.bindings.iter().filter(|&&j| j != i).map(|&j| format!("{}: {}", k.bindings[j].owner, k.bindings[j].label)).collect();
                    format!("  CONFLICT with {}", others.join(", "))
                });
                let id = if b.id.is_empty() { String::new() } else { format!(" ({})", b.id) };
                println!("  {:<14} {:<12} {}{id}{}", b.key, b.owner, b.label, clash.unwrap_or_default());
            }
            if k.conflicts.is_empty() {
                println!("no conflicts");
            }
        }
        OptionsAction::Schema { mod_id, version, lint, set: Some(file), message } => {
            let _ = (version, lint);
            let toml = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let info: SchemaInfo = serde_json::from_value(call(OptionsRequest::SchemaSet { game: product_id.into(), mod_id, toml, message }).await?)?;
            if json {
                return json_out(&info);
            }
            println!("{}: local schema r{} for {} ('{}')", g.name, info.revision.unwrap_or(0), info.mod_id, info.schema.surface);
            if info.source != quiver_api::options::SchemaSource::Local {
                println!("note: {} {} uses {} (it comes first), not the local one", info.mod_id, info.version_label, info.note);
            }
        }
        OptionsAction::Schema { mod_id, version, lint, set: None, .. } => {
            let info: SchemaInfo = serde_json::from_value(call(OptionsRequest::Schema { game: product_id.into(), mod_id, version }).await?)?;
            if json {
                return json_out(&info);
            }
            if !lint {
                println!(
                    "{} {} · surface {} · {} · {} options, {} presets, {} metrics, {} actions",
                    info.mod_id,
                    info.version_label,
                    info.schema.surface,
                    info.note,
                    info.schema.options.len(),
                    info.schema.presets.len(),
                    info.schema.metrics.len(),
                    info.schema.actions.len()
                );
            }
            for f in &info.lint {
                println!("  {f}");
            }
            if lint && info.lint.is_empty() {
                println!("no findings");
            }
        }
        OptionsAction::Infer { mod_id, file } => {
            let text: String = serde_json::from_value(call(OptionsRequest::Infer { game: product_id.into(), mod_id, file }).await?)?;
            print!("{text}");
        }
        OptionsAction::Lint { dir } => {
            let path = dir.canonicalize().with_context(|| format!("{} not found", dir.display()))?;
            let r: LintReport = serde_json::from_value(call(OptionsRequest::Lint { path }).await?)?;
            if json {
                return json_out(&r);
            }
            println!("{}: surface {}, {} options, {} keys undescribed", r.schema.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), r.surface, r.options, r.undescribed.len());
            for f in &r.findings {
                println!("  {f}");
            }
            if r.errors() > 0 {
                bail!("{} error(s)", r.findings.iter().filter(|f| f.severity == Severity::Error).count());
            }
        }
        OptionsAction::Sessions => {
            let list: Vec<SessionDetail> = serde_json::from_value(call(OptionsRequest::Sessions).await?)?;
            let list: Vec<&SessionDetail> = list.iter().filter(|s| s.game_ref() == product_id.0).collect();
            if json {
                return json_out(&list);
            }
            if list.is_empty() {
                println!("{} isn't running", g.name);
            }
            for s in list {
                println!(
                    "{}  profile {}  options r{}  pids {}",
                    s.session.as_deref().unwrap_or("(not launched by quiverd)"),
                    s.profile.as_deref().unwrap_or("?"),
                    s.options_revision.map_or("?".into(), |r| r.to_string()),
                    s.pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
                );
            }
        }
        OptionsAction::Invoke { surface, action, arg } => {
            let r: InvokeResult = serde_json::from_value(call(OptionsRequest::Invoke { game: product_id.into(), surface, action, arg, session: None }).await?)?;
            if json {
                return json_out(&r);
            }
            println!("{}: {} {}{}", g.name, r.surface, r.action, r.detail.map(|d| format!(": {d}")).unwrap_or_default());
        }
    }
    Ok(())
}
