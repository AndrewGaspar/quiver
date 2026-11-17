//! `quiver tweaks …`: Quiver Tweaks (design/TWEAKS.md), platform-level overrides for frame
//! rate, sync, latency, DLSS and frame generation that Quiver applies to any game it launches.
//! Values live in three scopes: `--global` (every game), a game (`quiver tweaks set origins …`)
//! and a profile (`--profile DLSS`); each is an Options-style history with revisions.
//!
//! ```sh
//! quiver tweaks                                   # the global scope, with the machine's capabilities
//! quiver tweaks list origins --profile DLSS       # a game's tweaks: where each value comes from, what the next launch does
//! quiver tweaks set --global fps.cap=vrr -m "VRR panel"
//! quiver tweaks set origins dlss.sr.model=K
//! quiver tweaks prefer origins fps.cap quiver --profile DLSS
//! quiver tweaks explain origins fps.cap           # bindings tried, capabilities, what gets written
//! quiver tweaks plan origins --profile DLSS       # the environment of the next launch
//! quiver tweaks status                            # running sessions: effects with evidence
//! quiver tweaks preset --global competitive
//! quiver tweaks history --global
//! quiver tweaks reset origins sync.vsync
//! ```

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::Value;
use quiver_api::mods::is_human_source;
use quiver_api::options::{Level, OptionOp, OptionState, OptionsChange, OptionsRevision, SurfaceState, ValueSource};
use quiver_api::tweaks::{Capabilities, Effect, Planned, Prefer, Scope, SessionTweaks, TweaksPlan, TweaksRequest};
use quiver_api::{Client, GameRef, Request};

#[derive(Subcommand)]
pub enum TweaksAction {
    /// Tweaks at a scope (the default: global): value, where it comes from, what the next
    /// launch does, the last session's effect.
    List {
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        /// Advanced tweaks too.
        #[arg(long)]
        all: bool,
        /// Only tweaks set somewhere.
        #[arg(long)]
        changed: bool,
    },
    /// One tweak in detail.
    Get {
        /// `[GAME] ID`
        #[arg(required = true, num_args = 1..=2)]
        args: Vec<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        global: bool,
    },
    /// Set tweaks: `[GAME] id=value…` (`id=` resets). --global for every game, --profile for
    /// one profile of GAME; otherwise GAME's own scope.
    Set {
        #[arg(required = true, value_name = "[GAME] ID=VALUE")]
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(short, long, alias = "reason")]
        message: Option<String>,
        /// Confirm without asking (global and danger tweaks).
        #[arg(long)]
        yes: bool,
    },
    /// Back to the lower scope's value: `[GAME] ID…`, or --all.
    Reset {
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Apply a preset (competitive, vrr, quiet, dlss-check) as one change.
    Preset {
        #[arg(required = true, value_name = "[GAME] PRESET")]
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Who implements a tweak a mod also provides: `GAME ID provider|quiver` (or --global).
    Prefer {
        #[arg(required = true, num_args = 2..=3)]
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Why each tweak does what it does at GAME's next launch: the capabilities, every
    /// binding considered. With ID, only that tweak.
    Explain {
        game: String,
        id: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
    /// The exact environment and DXVK_CONFIG GAME's next launch gets from Tweaks.
    Plan {
        game: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Running sessions: each tweak's effect, with evidence.
    Status,
    /// GAME's recorded sessions and their effects.
    Sessions {
        game: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// A scope's revisions (--global, or GAME with or without --profile).
    History {
        game: Option<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Make an earlier revision of a scope current again (rN; r0: nothing set).
    Restore {
        #[arg(required = true, value_name = "[GAME] rN")]
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// The capability set: GPU, driver, Proton build, NGX pieces.
    Caps { game: Option<String> },
}

fn json_out<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// A game argument: a ref (`3539`, `steam:620`) as is (no library needed, so it works
/// signed out), else a name looked up in the library.
async fn game_ref(client: &Client, game: &str) -> Result<(GameRef, String)> {
    if let Some(r) = GameRef::parse_ref(game) {
        return Ok((r, r.to_string()));
    }
    let e = crate::resolve_any(client, game).await?;
    Ok((e.game, e.name))
}

/// The scope a command means: --global, or GAME (with --profile: one of its profiles).
async fn scope_of(client: &Client, game: Option<&str>, global: bool, profile: Option<String>) -> Result<(Scope, String)> {
    match (game, global) {
        (_, true) if profile.is_some() => bail!("--global and --profile don't go together"),
        (Some(g), true) => bail!("--global sets every game's tweaks; leave out '{g}'"),
        (None, true) | (None, false) if profile.is_none() => Ok((Scope::Global, "every game".into())),
        (None, _) => bail!("--profile needs a game"),
        (Some(g), false) => {
            let (r, name) = game_ref(client, g).await?;
            Ok(match profile {
                Some(p) => (Scope::Profile { game: r, profile: Some(p.clone()) }, format!("{name}, profile {p}")),
                None => (Scope::Game { game: r }, name),
            })
        }
    }
}

/// `[GAME] rest…`: with --global everything is `rest`.
fn split_game(args: &[String], global: bool, min_rest: usize) -> (Option<&str>, &[String]) {
    if global || args.len() <= min_rest {
        (None, args)
    } else {
        (Some(args[0].as_str()), &args[1..])
    }
}

fn ask(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt} Allow? [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// A change request, asking at the terminal when the daemon wants a confirmation.
async fn change(client: &Client, who: &str, mk: impl Fn(bool) -> TweaksRequest, yes: bool) -> Result<OptionsChange> {
    match client.call::<OptionsChange>(Request::Tweaks(mk(yes))).await {
        Err(e) if !yes && e.to_string().contains("confirmation") => {
            let what = e.to_string();
            let prompt = if is_human_source(who) { what.clone() } else { format!("'{who}' asks: {what}") };
            if ask(&prompt)? { client.call::<OptionsChange>(Request::Tweaks(mk(true))).await } else { Err(e) }
        }
        r => r,
    }
}

fn print_change(c: &OptionsChange) {
    if c.revision > 0 {
        println!("r{}: {}", c.revision, c.changes.join(", "));
    } else {
        println!("nothing changed");
    }
    for o in &c.outcomes {
        if !matches!(o.outcome, quiver_api::options::Outcome::SavedRestart { .. }) {
            println!("  {}: {}", o.id, o.outcome.describe());
        }
    }
    for w in &c.warnings {
        eprintln!("note: {w}");
    }
}

fn parse_change(s: &str) -> Result<OptionOp> {
    let (id, value) = s.split_once('=').with_context(|| format!("expected id=value, got '{s}'"))?;
    let id = id.trim().to_owned();
    Ok(if value.is_empty() { OptionOp::Reset { surface: "quiver".into(), id } } else { OptionOp::Set { surface: "quiver".into(), id, value: Value::String(value.to_owned()) } })
}

fn from_text(o: &OptionState) -> String {
    match &o.from {
        ValueSource::Default => "default".into(),
        ValueSource::Scope { scope, revision, .. } => format!("{}, r{revision}", scope.label().to_lowercase()),
        other => format!("{other:?}"),
    }
}

/// A value as people read it (labels for sentinels and enum values).
fn show_value(def: &quiver_api::options::OptionDef, v: &Value) -> String {
    if let Some(sp) = def.special_of(v) {
        sp.label.clone()
    } else if let Some(l) = def.values.iter().find(|e| quiver_api::options::same_value(&e.value, v)).and_then(|e| e.label.clone()) {
        l
    } else {
        def.display(v)
    }
}

/// The row's value as people read it, `vrr` resolved.
fn show(o: &OptionState) -> String {
    let base = show_value(&o.def, &o.value);
    match o.text.split_once(" (") {
        Some((_, rest)) if o.value.as_str() == Some("vrr") => format!("{base} ({rest}"),
        _ => base,
    }
}

fn effect_short(e: &Effect) -> String {
    match e {
        Effect::Confirmed { evidence, .. } => format!("confirmed ({evidence})"),
        other => other.describe(),
    }
}

fn print_state(s: &SurfaceState, title: &str, all: bool, changed: bool) {
    println!("{} · {title} · {}", s.title, s.schema_note);
    let width = 18;
    for p in &s.pages {
        let mut lines = Vec::new();
        for id in p.groups.iter().flat_map(|g| g.options.iter()) {
            let Some(o) = s.options.get(id) else { continue };
            if !all && o.def.level > Level::Basic {
                continue;
            }
            if changed && matches!(o.from, ValueSource::Default) {
                continue;
            }
            let row = o.tweak.as_deref();
            let val = show(o);
            let mut tail = String::new();
            if let Some(p) = row.and_then(|r| r.planned.as_ref()).filter(|p| **p != Planned::Default) {
                tail.push_str(&p.describe());
            } else if let Some(r) = row
                && !r.chips.is_empty()
                && matches!(o.from, ValueSource::Default)
            {
                tail.push_str(&format!("[{}]", r.chips.join(", ")));
            }
            if let Some(by) = row.and_then(|r| r.overridden_by) {
                tail.push_str(&format!("  (overridden: {})", by.label()));
            }
            if let Some(d) = &o.dormant {
                tail.push_str(&format!("  [{d}]"));
            }
            lines.push(format!("    {id:<width$} {val:<22} {:<18} {tail}", from_text(o)));
            if let Some(last) = row.and_then(|r| r.last.as_ref()) {
                lines.push(format!("    {:<width$} last session: {}", "", effect_short(last)));
            }
        }
        if !lines.is_empty() {
            println!("  {}", p.title);
            for l in lines {
                println!("{l}");
            }
        }
    }
    if !s.presets.is_empty() {
        println!("  presets: {}", s.presets.iter().map(|p| format!("{} ({})", p.id, p.label)).collect::<Vec<_>>().join(", "));
    }
    for w in &s.lint {
        println!("  warning: {w}");
    }
}

fn print_caps(c: &Capabilities) {
    println!("{}", c.summary());
    if let Some(p) = &c.proton {
        println!(
            "  Proton {}: DXVK {}, vkd3d-proton {}, dxvk-nvapi {}{}",
            p.name,
            p.dxvk.as_deref().unwrap_or("none"),
            p.vkd3d.as_deref().unwrap_or("none"),
            p.nvapi.as_deref().unwrap_or("none"),
            if p.copies_updater { "; copies nvngx_update.exe" } else { "" }
        );
        if !p.has.is_empty() {
            println!("    knows: {}", p.has.join(", "));
        }
    }
    match &c.ngx.dir {
        Some(d) => println!("  NGX: {} (core {}, updater {})", d.display(), if c.ngx.core { "yes" } else { "no" }, if c.ngx.updater { "yes" } else { "no: nvngx_update.exe isn't installed" }),
        None => println!("  NGX: the driver's NGX files aren't installed"),
    }
    if let Some(d) = &c.display {
        println!("  display: {} {:.0} Hz{}", d.name, d.refresh_hz, if d.vrr { ", VRR" } else { "" });
    }
    println!("  route: {}; game API last seen: {}", if c.route.is_empty() { "quiver" } else { &c.route }, c.api);
    for n in &c.notes {
        println!("  note: {n}");
    }
}

fn print_plan(p: &TweaksPlan, explain: Option<&str>, full: bool) {
    print_caps(&p.caps);
    println!("Revisions: global r{}, game r{}, profile r{}", p.revisions.global, p.revisions.game, p.revisions.profile);
    for e in p.entries.iter().filter(|e| explain.is_none_or(|id| id == e.id)) {
        println!("  {} = {}: {}", e.id, e.display, e.planned.describe());
        if full {
            for c in &e.considered {
                println!("      {} {}{}", if c.chosen { "✓" } else { "·" }, c.mech, if c.why.is_empty() { String::new() } else { format!(": {}", c.why) });
            }
        }
    }
    if p.entries.is_empty() {
        println!("  no tweak is set for this game: the launch gets nothing from Tweaks");
    }
    if explain.is_none() {
        if !p.env.is_empty() {
            println!("Environment:");
            for (k, v) in &p.env {
                println!("  {k}={v}");
            }
        }
        for b in &p.bound {
            println!("Into {}: {} = {} (from {})", b.surface, b.option, quiver_api::options::scalar_text(&b.value), b.tweak);
        }
        for f in &p.files {
            println!("File: {f}");
        }
    }
    for n in &p.notes {
        println!("note: {n}");
    }
    for w in &p.warnings {
        println!("warning: {w}");
    }
}

fn print_session(s: &SessionTweaks) {
    let end = s.ended.map(|e| format!("–{}", e.with_timezone(&chrono::Local).format("%H:%M"))).unwrap_or_else(|| " (running)".into());
    println!("{} · {} · {}{end}", s.session, s.game, s.started.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"));
    for e in &s.effects {
        println!("  {:<18} {:<22} {}", e.id, e.value, effect_short(&e.effect));
    }
    if let Some(d) = &s.log_dir {
        println!("  logs: {}", d.display());
    }
}

pub async fn run(client: &Client, who: &str, json: bool, action: Option<TweaksAction>) -> Result<()> {
    let action = action.unwrap_or(TweaksAction::List { game: None, profile: None, all: false, changed: false });
    match action {
        TweaksAction::List { game, profile, all, changed } => {
            let (scope, title) = scope_of(client, game.as_deref(), false, profile).await?;
            let s: SurfaceState = client.call(Request::Tweaks(TweaksRequest::List { scope, level: None })).await?;
            if json {
                return json_out(&s);
            }
            print_state(&s, &title, all, changed);
        }
        TweaksAction::Get { args, profile, global } => {
            let (game, rest) = split_game(&args, global, 1);
            let id = rest.first().context("which tweak?")?;
            let (scope, title) = scope_of(client, game, global, profile).await?;
            let s: SurfaceState = client.call(Request::Tweaks(TweaksRequest::List { scope, level: None })).await?;
            let o = s.options.get(id).with_context(|| format!("no tweak {id} (tweaks: {})", s.options.keys().cloned().collect::<Vec<_>>().join(", ")))?;
            if json {
                return json_out(o);
            }
            println!("{id} ({title}): {}", show(o));
            println!("  {}", o.def.label_or(id));
            if let Some(h) = &o.def.help {
                println!("  {h}");
            }
            println!("  from: {}; below this scope: {}", from_text(o), show_value(&o.def, &o.default));
            if let Some(r) = &o.tweak {
                for (k, v) in [("global", &r.global), ("game", &r.game), ("profile", &r.profile)] {
                    if let Some(v) = v {
                        println!("  {k}: {}", show_value(&o.def, v));
                    }
                }
                if let Some(p) = &r.planned {
                    println!("  next launch: {}", p.describe());
                }
                if !r.chips.is_empty() {
                    println!("  works on: {}", r.chips.join(", "));
                }
                if let Some(l) = &r.last {
                    println!("  last session: {}", effect_short(l));
                }
            }
        }
        TweaksAction::Set { args, global, profile, message, yes } => {
            let first_is_change = args.first().is_some_and(|a| a.contains('='));
            let (game, rest) = if global || first_is_change { (None, &args[..]) } else { (Some(args[0].as_str()), &args[1..]) };
            let ops = rest.iter().map(|c| parse_change(c)).collect::<Result<Vec<_>>>()?;
            if ops.is_empty() {
                bail!("give id=value");
            }
            let (scope, _) = scope_of(client, game, global || game.is_none(), profile).await?;
            let c = change(client, who, |confirmed| TweaksRequest::Set { scope: scope.clone(), ops: ops.clone(), message: message.clone(), confirmed }, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&c);
        }
        TweaksAction::Reset { args, global, profile, all, message, yes } => {
            let (game, rest) = split_game(&args, global, if all { 0 } else { 1 });
            let ops: Vec<OptionOp> = if all {
                vec![OptionOp::ResetAll { surface: "quiver".into() }]
            } else {
                rest.iter().map(|id| OptionOp::Reset { surface: "quiver".into(), id: id.clone() }).collect()
            };
            if ops.is_empty() {
                bail!("which tweaks? (or --all)");
            }
            let (scope, _) = scope_of(client, game, global || game.is_none(), profile).await?;
            let c = change(client, who, |confirmed| TweaksRequest::Set { scope: scope.clone(), ops: ops.clone(), message: message.clone(), confirmed }, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&c);
        }
        TweaksAction::Preset { args, global, profile, message, yes } => {
            let (game, rest) = split_game(&args, global, 1);
            let id = rest.first().context("which preset?")?.clone();
            let (scope, _) = scope_of(client, game, global || game.is_none(), profile).await?;
            let ops = vec![OptionOp::Preset { surface: "quiver".into(), id }];
            let c = change(client, who, |confirmed| TweaksRequest::Set { scope: scope.clone(), ops: ops.clone(), message: message.clone(), confirmed }, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&c);
        }
        TweaksAction::Prefer { args, global, profile, yes } => {
            let (game, rest) = split_game(&args, global, 2);
            let [id, who_impl] = rest else { bail!("give ID provider|quiver") };
            let prefer = match who_impl.to_lowercase().as_str() {
                "provider" | "mod" => Prefer::Provider,
                "quiver" => Prefer::Quiver,
                o => bail!("prefer 'provider' or 'quiver', not '{o}'"),
            };
            let (scope, _) = scope_of(client, game, global || game.is_none(), profile).await?;
            let c = change(client, who, |confirmed| TweaksRequest::Prefer { scope: scope.clone(), id: id.clone(), prefer, message: None, confirmed }, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&c);
        }
        TweaksAction::Explain { game, id, profile } => {
            let (g, _) = game_ref(client, &game).await?;
            let p: TweaksPlan = client.call(Request::Tweaks(TweaksRequest::Plan { game: g, profile })).await?;
            if json {
                return json_out(&p);
            }
            print_plan(&p, id.as_deref(), true);
        }
        TweaksAction::Plan { game, profile } => {
            let (g, _) = game_ref(client, &game).await?;
            let p: TweaksPlan = client.call(Request::Tweaks(TweaksRequest::Plan { game: g, profile })).await?;
            if json {
                return json_out(&p);
            }
            print_plan(&p, None, false);
        }
        TweaksAction::Status => {
            let s: Vec<SessionTweaks> = client.call(Request::Tweaks(TweaksRequest::Status)).await?;
            if json {
                return json_out(&s);
            }
            if s.is_empty() {
                println!("no running session has tweaks");
            }
            s.iter().for_each(print_session);
        }
        TweaksAction::Sessions { game, limit } => {
            let (g, _) = game_ref(client, &game).await?;
            let s: Vec<SessionTweaks> = client.call(Request::Tweaks(TweaksRequest::Sessions { game: g, limit: Some(limit) })).await?;
            if json {
                return json_out(&s);
            }
            if s.is_empty() {
                println!("no recorded session with tweaks");
            }
            s.iter().for_each(print_session);
        }
        TweaksAction::History { game, global, profile } => {
            let (scope, title) = scope_of(client, game.as_deref(), global, profile).await?;
            let h: Vec<OptionsRevision> = client.call(Request::Tweaks(TweaksRequest::History { scope })).await?;
            if json {
                return json_out(&h);
            }
            if h.is_empty() {
                println!("{title}: no tweaks set yet");
            }
            for r in h.iter().rev() {
                println!("r{} {} {}{}", r.revision, r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), r.source, r.message.as_ref().map(|m| format!(" — {m}")).unwrap_or_default());
                for c in &r.changes {
                    println!("    {c}");
                }
            }
        }
        TweaksAction::Restore { args, global, profile, yes } => {
            let (game, rest) = split_game(&args, global, 1);
            let rev = rest.first().context("which revision? (rN)")?;
            let revision: u32 = rev.trim_start_matches('r').parse().with_context(|| format!("'{rev}' isn't a revision (rN)"))?;
            let (scope, _) = scope_of(client, game, global || game.is_none(), profile).await?;
            let c = change(client, who, |confirmed| TweaksRequest::Restore { scope: scope.clone(), revision, confirmed }, yes).await?;
            if json {
                return json_out(&c);
            }
            print_change(&c);
        }
        TweaksAction::Caps { game } => {
            let g = match &game {
                Some(g) => Some(game_ref(client, g).await?.0),
                None => None,
            };
            let c: Capabilities = client.call(Request::Tweaks(TweaksRequest::Capabilities { game: g })).await?;
            if json {
                return json_out(&c);
            }
            print_caps(&c);
        }
    }
    Ok(())
}
