//! `quiver profiles <game> …`: a game's profiles, each a named setup of its mods and game
//! settings (design/PROFILES.md). The requests are the mods ones (`ModsRequest::Profile*`),
//! so `quiver mods <game> profile …` keeps working; this is the profile-first view.

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::mods::{ModsRequest, ModsState, ProfileInfo, ProfileRevision, VANILLA};
use quiver_api::{Client, Request};

use crate::mods::{GameArg, ModsAction, ProfileAction, default_label, entry_label, profile_reply, resolve_installed, saves_line};

#[derive(Subcommand)]
pub enum ProfilesAction {
    /// Every profile with its mods, game settings and saves (the default).
    List {
        #[arg(long)]
        json: bool,
    },
    /// One profile in full (or `vanilla`).
    Show {
        profile: String,
        #[arg(long)]
        json: bool,
    },
    /// A new profile. By default it's a copy of the game's default profile (what Play
    /// runs): its mods, launch overrides, Options values and Tweaks, a snapshot of its game
    /// settings files and its save store. `--from P` copies another (or `vanilla`), `--blank`
    /// starts from vanilla with nothing. `--mods a,b` sets the mods instead (`--mods ''`: none).
    /// It gets its own copy of the game's settings folders unless `--shared-settings`.
    Create {
        name: String,
        #[arg(long, conflicts_with = "blank")]
        from: Option<String>,
        #[arg(long)]
        blank: bool,
        #[arg(long, alias = "layers", value_delimiter = ',')]
        mods: Option<Vec<String>>,
        /// Save store: default, new, or a store's name or id (Ubisoft games).
        #[arg(long)]
        saves: Option<String>,
        /// Share the game's settings files with Vanilla instead of having its own copy.
        #[arg(long)]
        shared_settings: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Change a profile (the keys of `quiver mods <game> profile set`: mods=, saves=,
    /// profile_config=, pinned=, name=, proton=, env.NAME=, dll.NAME=, args=, wrappers=).
    Set {
        profile: String,
        #[arg(required = true)]
        changes: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Game settings: the folders that hold them and which profiles have their own copy.
    /// `settings PROFILE own` gives a profile its own copy (a snapshot of what it sees now),
    /// `settings PROFILE shared` makes it share the prefix's files again (its copy is kept).
    Settings { profile: Option<String>, mode: Option<String> },
    /// The profile Play uses (`vanilla` for none); without a value, shows it.
    Default {
        profile: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Delete a profile (its files are kept; `restore` brings it back).
    Delete { profile: String },
    /// A profile's revisions.
    History { profile: String },
    /// Make an earlier revision current again.
    Restore { profile: String, revision: u32 },
    Pin { profile: String },
    Unpin { profile: String },
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// "own settings (Remedy/AlanWakeRemastered, 4 files)" or "Vanilla's settings files".
fn settings_line(i: &ProfileInfo) -> String {
    if i.config_dirs.is_empty() {
        return "shares Vanilla's game settings files".into();
    }
    let dirs: Vec<String> = i
        .config_dirs
        .iter()
        .map(|d| {
            let shared = if d.shared.is_empty() { String::new() } else { format!(", saves in {} shared", d.shared.join(", ")) };
            format!("{} ({}{shared})", quiver_core_short(&d.dir), plural(d.files.len(), "own file"))
        })
        .collect();
    format!("its own game settings: {}", dirs.join("; "))
}

/// The last two components of a prefix folder.
fn quiver_core_short(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

fn values_line(options: u32, tweaks: u32) -> Option<String> {
    let mut v = Vec::new();
    if options > 0 {
        v.push(plural(options as usize, "Options value"));
    }
    if tweaks > 0 {
        v.push(plural(tweaks as usize, "Tweak"));
    }
    (!v.is_empty()).then(|| v.join(", "))
}

fn print_profile(s: &ModsState, i: &ProfileInfo) {
    let p = &i.profile;
    let mut flags = vec![format!("r{}", i.revision)];
    if s.default_profile.as_deref() == Some(p.id.as_str()) {
        flags.insert(0, "default".into());
    }
    if p.pinned {
        flags.push("pinned".into());
    }
    println!("  {}  {}  [{}]", p.id, p.name, flags.join(", "));
    let mods: Vec<String> = p.layers.iter().map(|e| entry_label(s, e)).collect();
    println!("      mods:      {}", if mods.is_empty() { "none".to_owned() } else { mods.join(" < ") });
    println!("      settings:  {}", settings_line(i));
    if let Some(v) = values_line(i.option_values, i.tweak_values) {
        println!("                 {v} (quiver mods {} options --profile {}, quiver tweaks)", GameArg(s.game_ref()), p.id);
    }
    let overrides = serde_json::to_value(&p.settings).unwrap_or_default();
    if overrides.as_object().is_some_and(|o| !o.is_empty()) {
        println!("                 launch overrides: {overrides}");
    }
    if let Some(saves) = &i.saves {
        println!("      saves:     {}", saves.store_name.clone().unwrap_or_else(|| saves.folder.clone()));
        if let Some(n) = &saves.cloud_note {
            println!("                 {n}");
        }
    }
    for pr in &i.problems {
        println!("      won't launch: {pr}");
    }
    for w in &i.warnings {
        println!("      note: {w}");
    }
}

fn print_vanilla(s: &ModsState) {
    let live_default = s.default_profile.as_deref().filter(|d| s.profiles.iter().any(|i| i.profile.id == *d));
    println!("  {VANILLA}  Vanilla (the base game){}", if live_default.is_none() { "  [default]" } else { "" });
    println!("      mods:      none{}", if s.vanilla_fixes.is_empty() { String::new() } else { format!(" (fixes: {})", s.vanilla_fixes.len()) });
    println!("      settings:  the game's own settings files in its prefix");
    if let Some(v) = values_line(s.vanilla_option_values, s.vanilla_tweak_values) {
        println!("                 {v}");
    }
    if let Some(v) = &s.vanilla_saves {
        println!("      saves:     {}", v.store_name.clone().unwrap_or_else(|| v.folder.clone()));
    }
}

fn print_settings_dirs(s: &ModsState) {
    if s.settings_dirs.is_empty() {
        println!("game settings folders: none known yet (run the game once, or name them: profile set P profile_config=DIR,…)");
        return;
    }
    println!("game settings folders (a profile with its own settings has a copy of each):");
    for d in &s.settings_dirs {
        let users: Vec<&str> =
            s.profiles.iter().filter(|i| i.config_dirs.iter().any(|c| c.dir.eq_ignore_ascii_case(&d.dir))).map(|i| i.profile.name.as_str()).collect();
        println!(
            "  {}  [{}{}]",
            d.dir,
            d.source.label(),
            if d.exists { "" } else { ", not written yet" }
        );
        if !d.shared.is_empty() {
            println!("      saves kept shared: {}", d.shared.join(", "));
        }
        println!("      own copy: {}", if users.is_empty() { "no profile".to_owned() } else { users.join(", ") });
    }
}

async fn load(client: &Client, key: GameArg) -> Result<ModsState> {
    Ok(client.call(Request::Mods(ModsRequest::List { game: key.into() })).await?)
}

pub async fn run(client: &Client, game: &str, action: Option<ProfilesAction>, json: bool) -> Result<()> {
    let g = resolve_installed(client, game).await?;
    let key = GameArg(g.game_ref());
    let call = |req: ModsRequest| client.call::<serde_json::Value>(Request::Mods(req));
    let mods = |action: ProfileAction| crate::mods::run(client, game, ModsAction::Profile { action });
    match action.unwrap_or(ProfilesAction::List { json }) {
        ProfilesAction::List { json: j } => {
            let s = load(client, key).await?;
            if j || json {
                return crate::print_json(&s);
            }
            println!("{} ({key}): profiles (Play uses {})", g.name, default_label(&s));
            print_vanilla(&s);
            s.profiles.iter().for_each(|i| print_profile(&s, i));
            if s.profiles.is_empty() {
                println!("  (no profiles yet: quiver profiles {key} create NAME)");
            }
            if !s.deleted.is_empty() {
                println!("deleted (quiver profiles {key} restore ID REVISION): {}", s.deleted.iter().map(|i| i.profile.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            println!("mods library: {} (quiver mods {key} list)", plural(s.all_mods().len(), "mod"));
        }
        ProfilesAction::Show { profile, json: j } => {
            let s = load(client, key).await?;
            if profile.eq_ignore_ascii_case(VANILLA) {
                if j || json {
                    return crate::print_json(&s.vanilla_saves);
                }
                print_vanilla(&s);
                return Ok(());
            }
            let Some(i) = s.profiles.iter().chain(&s.deleted).find(|i| i.profile.id == profile || i.profile.name.eq_ignore_ascii_case(&profile)) else {
                bail!("{}: no profile '{profile}' (quiver profiles {key})", g.name);
            };
            if j || json {
                return crate::print_json(i);
            }
            print_profile(&s, i);
            for d in &i.config_dirs {
                println!("      own folder: {} -> {}", d.dir, d.upper.display());
            }
            if let Some(saves) = &i.saves {
                println!("      saves detail: {}", saves_line(saves));
            }
        }
        ProfilesAction::Create { name, from, blank, mods: list, saves, shared_settings, message } => {
            let s = load(client, key).await?;
            let from = match (from, blank) {
                (_, true) => None,
                (Some(f), false) => Some(f),
                // The game's default profile, as Play runs it.
                (None, false) => Some(s.default_profile.clone().filter(|d| s.profiles.iter().any(|i| &i.profile.id == d)).unwrap_or_else(|| VANILLA.into())),
            };
            let layers = list.map(|l| l.into_iter().filter(|x| !x.trim().is_empty()).collect::<Vec<_>>());
            let req = ModsRequest::ProfileCreate {
                game: key.into(),
                name,
                layers,
                message,
                saves,
                from: from.clone(),
                own_settings: shared_settings.then_some(false),
            };
            let v = call(req).await?;
            if let Some(c) = v.get("copied").and_then(|c| c.as_array()) {
                for n in c.iter().filter_map(|n| n.as_str()) {
                    eprintln!("copied: {n}");
                }
            }
            let rev = profile_reply(v)?;
            let s = load(client, key).await?;
            println!(
                "{}: created profile {} '{}'{}",
                g.name,
                rev.profile.id,
                rev.profile.name,
                from.map(|f| format!(" from {f}")).unwrap_or_else(|| " (blank)".into())
            );
            if let Some(i) = s.profiles.iter().find(|i| i.profile.id == rev.profile.id) {
                print_profile(&s, i);
            }
        }
        ProfilesAction::Set { profile, changes, message } => mods(ProfileAction::Set { profile, changes, message }).await?,
        ProfilesAction::Settings { profile: None, .. } => {
            let s = load(client, key).await?;
            if json {
                return crate::print_json(&s.settings_dirs);
            }
            println!("{} ({key})", g.name);
            print_settings_dirs(&s);
        }
        ProfilesAction::Settings { profile: Some(p), mode } => {
            let own = match mode.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("own") => true,
                Some("shared" | "share") => false,
                None => {
                    let s = load(client, key).await?;
                    let Some(i) = s.profiles.iter().find(|i| i.profile.id == p || i.profile.name.eq_ignore_ascii_case(&p)) else {
                        bail!("{}: no profile '{p}'", g.name);
                    };
                    println!("{}: {}", i.profile.name, settings_line(i));
                    for d in &i.config_dirs {
                        println!("  {} -> {}", d.dir, d.upper.display());
                    }
                    return Ok(());
                }
                Some(other) => bail!("settings PROFILE own|shared, not '{other}'"),
            };
            let v = call(ModsRequest::ProfileSettings { game: key.into(), profile: p, own, message: None }).await?;
            if let Some(c) = v.get("copied").and_then(|c| c.as_array()) {
                for n in c.iter().filter_map(|n| n.as_str()) {
                    eprintln!("{n}");
                }
            }
            let rev = profile_reply(v)?;
            let s = load(client, key).await?;
            match s.profiles.iter().find(|i| i.profile.id == rev.profile.id) {
                Some(i) => println!("{}: '{}' r{}: {}", g.name, i.profile.name, rev.revision, settings_line(i)),
                None => println!("{}: '{}' r{}", g.name, rev.profile.name, rev.revision),
            }
        }
        ProfilesAction::Default { profile, message } => crate::mods::run(client, game, ModsAction::Default { profile, message }).await?,
        ProfilesAction::Delete { profile } => mods(ProfileAction::Delete { profile }).await?,
        ProfilesAction::History { profile } => {
            let revs: Vec<ProfileRevision> = serde_json::from_value(call(ModsRequest::ProfileHistory { game: key.into(), profile }).await?)?;
            if json {
                return crate::print_json(&revs);
            }
            for r in revs {
                println!(
                    "r{:<3} {}  by {:<10} {}{}",
                    r.revision,
                    r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    r.source,
                    if r.deleted { "(deleted) " } else { "" },
                    r.message.unwrap_or_default()
                );
            }
        }
        ProfilesAction::Restore { profile, revision } => mods(ProfileAction::Restore { profile, revision }).await?,
        ProfilesAction::Pin { profile } => mods(ProfileAction::Pin { profile }).await?,
        ProfilesAction::Unpin { profile } => mods(ProfileAction::Unpin { profile }).await?,
    }
    Ok(())
}

