//! `quiver mods <game> …`: mod layers and profiles. Games are looked up among installed
//! games, so none of this needs a Ubisoft request.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_api::mods::{
    CatalogEntry, GameProfiles, LaunchPlan, Layer, LayerChange, LayerRevision, MigrationReport, ModDetail, ModPrefs, ModRecord, ModRevision, ModSdkConnection, ModSdkRequest,
    ModsState, PatchState, PatchStatus, ProfileInfo, ProfileRevision, SettingConflict, VersionChange, VersionMerge, is_human_source,
};
use quiver_api::{Client, GameRef, ModsRequest, Request, SettingOp};

#[derive(Subcommand)]
pub enum ModsAction {
    /// Layers and profiles (vanilla is always there and has no layers).
    List {
        #[arg(long)]
        json: bool,
    },
    /// The profile Play uses when none is named (the app's Play button, `quiver play`,
    /// .desktop entries, Steam shortcuts): a profile's name or id, or `vanilla`. Without a
    /// value, shows it. Recorded as the game's `profile` setting (`quiver history`).
    Default {
        /// Profile name or id, or `vanilla`.
        profile: Option<String>,
        /// Note recorded with this settings revision.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Mod layers: folders of files laid over the install folder.
    Layer {
        #[command(subcommand)]
        action: LayerAction,
    },
    /// Mod profiles: named stacks of mods plus your own overrides.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// A mod's versions: add one (it becomes current, so every profile that tracks the mod
    /// runs it next), list them, roll back or forward (`use`), retire old ones.
    Version {
        #[command(subcommand)]
        action: VersionAction,
    },
    /// Make separate mods versions of one: `merge-versions DLSS DLSS-v2 DLSS-v3`. Versions
    /// are ordered by when each was added and the newest becomes current. Profiles that
    /// stacked one of them stack INTO instead: tracking current if they ran the newest, else
    /// pinned to what they ran, so no launch changes. Recorded as revisions.
    MergeVersions {
        /// The mod that keeps its id and name.
        into: String,
        /// Mods that become its versions.
        #[arg(required = true)]
        from: Vec<String>,
        /// Labels for the versions, oldest first (default: v1, v2, … where a version has no
        /// label of its own).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Quiver's built-in mods for this game (audited recipes; `install ID` to use one).
    Catalog {
        #[arg(long)]
        json: bool,
    },
    /// Install a mod: a built-in one by id, or a package folder (with a quiver-mod.toml;
    /// a plain folder of files installs as a file-only mod). Installing a built-in again
    /// updates it to the catalog's version.
    Install {
        /// Built-in mod id, or a folder.
        what: String,
        /// Name for a plain folder (default: the folder's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove an installed mod; config values it changed go back to the files' own.
    Uninstall {
        #[arg(value_name = "MOD")]
        mod_id: String,
    },
    /// An installed mod in detail: what it declares, where it's used, what it changed.
    Show {
        #[arg(value_name = "MOD")]
        mod_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Change an installed mod: enabled=on|off  save_compatible=on|off|unknown  kind=fix|mod
    /// name=…
    Set {
        #[arg(value_name = "MOD")]
        mod_id: String,
        #[arg(required = true)]
        changes: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// What a launch would use: mods, resolved settings, conflicts, saves, mounts,
    /// per-profile folders (and where their files are), config patches.
    Plan {
        /// Profile name or id, or `vanilla` (default: the game's default profile, as Play).
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put config values Quiver changed back to the files' own now (all mods, or one).
    UndoPatches {
        #[arg(value_name = "MOD")]
        mod_id: Option<String>,
    },
    /// Mod options: list, get, set, reset, preset, diff, history, restore, revert, keys,
    /// schema, infer, lint (design/MOD-CONFIG.md). Values are per profile (--profile,
    /// default: the game's default profile), saved as changes from the defaults; a running
    /// game gets them live where the mod allows it, and every change says what happened.
    Options {
        #[command(subcommand)]
        action: Option<crate::options::OptionsAction>,
        /// Profile name or id, or `vanilla` (default: the game's default profile).
        #[arg(long, global = true)]
        profile: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Give every profile a save store: one with its own saves folder (or separate_saves=on)
    /// gets a store that is that folder, nothing moves; the others use Default. Launches do
    /// this on their own; `--dry-run` shows what it would do.
    Migrate {
        #[arg(long)]
        dry_run: bool,
    },
    /// Mods talking to Quiver through the mod SDK (quiver_mod.h) while the game runs.
    Sdk {
        #[command(subcommand)]
        action: SdkAction,
    },
}

#[derive(Subcommand)]
pub enum SdkAction {
    /// Connected mods: surface, SDK and protocol, capabilities, the options each applies
    /// live, and its metrics with their latest values.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum LayerAction {
    /// Copy a folder into Quiver's mod store (reflinks where possible) as a new layer.
    /// Lay it out like the install folder (e.g. `dxgi.dll`, `DataPC/…`). `--delete PATH`
    /// (relative to the install folder) hides a file or folder of the install in launches
    /// with this layer; the install itself is never changed. Deleting a folder the layer
    /// has files in replaces the install's whole folder with the layer's. DIR may be left
    /// out for a layer that only deletes.
    Add {
        name: String,
        dir: Option<PathBuf>,
        #[arg(long = "delete", value_name = "PATH")]
        delete: Vec<String>,
        /// Add it as a new version of this mod instead (NAME is the version's label); the
        /// same as `version add MOD DIR --label NAME`.
        #[arg(long, value_name = "MOD")]
        version_of: Option<String>,
    },
    /// Delete a layer's files (refused while a profile uses it).
    Remove { layer: String },
    /// Hide more files or folders of the install (paths relative to it) in launches with
    /// this layer. Recorded in the layer's history.
    DeleteFiles {
        layer: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Stop hiding files or folders the layer deletes.
    KeepFiles {
        layer: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// A layer's details: size, where it came from, what it deletes.
    Show { layer: String },
    /// The revisions of a layer's deletions.
    History { layer: String },
    /// Make an earlier revision of a layer's deletions current again.
    Restore { layer: String, revision: u32 },
}

#[derive(Subcommand)]
pub enum VersionAction {
    /// Add a version of MOD from DIR (a folder laid out like the install folder, or a
    /// package with a quiver-mod.toml). A plain folder keeps the current version's manifest
    /// (save compatibility, launch settings, config patches, deletions). It becomes current
    /// unless --no-update or --prerelease; profiles pinned to a version keep theirs.
    Add {
        #[arg(value_name = "MOD")]
        mod_id: String,
        dir: Option<PathBuf>,
        /// Free text, unique within the mod (default: the package's version, else vN).
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Don't make it current.
        #[arg(long)]
        no_update: bool,
        /// A pre-release: not current; pin it to try it (profile set P mods=MOD@LABEL).
        #[arg(long)]
        prerelease: bool,
        /// on, off or unknown (default: as the current version).
        #[arg(long, value_name = "on|off|unknown")]
        save_compatible: Option<String>,
        /// Also hide this install path (relative to the install folder).
        #[arg(long = "delete", value_name = "PATH")]
        delete: Vec<String>,
        /// The version's options schema (default: DIR's quiver-options.toml, else the
        /// current version's carried forward).
        #[arg(long, value_name = "FILE")]
        options: Option<PathBuf>,
    },
    /// A mod's versions, newest first, and which profiles track or pin which.
    List {
        #[arg(value_name = "MOD")]
        mod_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Make VERSION (label or id) current: every profile that tracks the mod runs it next.
    Use {
        #[arg(value_name = "MOD")]
        mod_id: String,
        version: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Hide a version from defaults (it can't be current); its files are kept and profiles
    /// pinned to it still run it.
    Retire {
        #[arg(value_name = "MOD")]
        mod_id: String,
        version: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Bring a retired version back.
    Unretire {
        #[arg(value_name = "MOD")]
        mod_id: String,
        version: String,
    },
    /// The mod's revisions (versions added, current changes, retirements, merges).
    History {
        #[arg(value_name = "MOD")]
        mod_id: String,
    },
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// A new profile; `--layers a,b` stacks layer b on top of a. Its save store is Default
    /// when every mod is save-compatible, else a new one, unless `--saves` says.
    Create {
        name: String,
        #[arg(long, value_delimiter = ',')]
        layers: Vec<String>,
        /// Save store: default, new, or a store's name or id (quiver saves <game> stores).
        #[arg(long)]
        saves: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Change a profile: mods=a,b (bottom to top; also layers=; MOD tracks its current
    /// version, MOD@VERSION pins one, e.g. mods=DLSS@v2,aco-skip-intros)  saves=STORE (default, new,
    /// or a store's name or id; cloud sync is per store: quiver saves <game> store cloud)
    /// pinned=on|off  profile_config=DIR,… (prefix folders the profile keeps its own copy
    /// of)  name=…, or a launch-setting override (proton=…, env.NAME=…, dll.NAME=…, args=…,
    /// wrappers=…).
    Set {
        profile: String,
        #[arg(required = true)]
        changes: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Give the profile its own library tile.
    Pin { profile: String },
    Unpin { profile: String },
    /// Delete a profile. Its saves and writable layer are kept, and it can be restored.
    Delete { profile: String },
    /// A profile's revisions (deleted profiles too).
    History { profile: String },
    /// Make an earlier revision current again (also brings back a deleted profile).
    Restore { profile: String, revision: u32 },
}

/// A game as `quiver mods` names it on the command line and in hints: `3539` for a Ubisoft
/// product (as always), `steam:620` for a Steam game; sent as the request's game.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GameArg(pub GameRef);

impl std::fmt::Display for GameArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.key())
    }
}

impl From<GameArg> for quiver_api::game::GameField {
    fn from(g: GameArg) -> Self {
        g.0.into()
    }
}

/// A game mods can be used with, by ref or unique name match: an installed Ubisoft or Epic
/// game, or any Steam game Steam has installed (`steam:<appid>`, or its name when no Ubisoft game
/// matches; design/STORE-STEAM.md §10).
pub(crate) async fn resolve_installed(client: &Client, game: &str) -> Result<GameProfiles> {
    let games: Vec<GameProfiles> = client.call(Request::Mods(ModsRequest::Games)).await?;
    let steam_game = |name: String, r: GameRef| GameProfiles { product_id: 0, name, profiles: Vec::new(), game: Some(r) };
    if let Some(r) = GameRef::parse_ref(game) {
        if let Some(g) = games.iter().find(|g| g.game_ref() == r) {
            return Ok(g.clone());
        }
        if r.is_ubisoft() {
            bail!("product {} is not installed", r.id);
        }
        if r.store == quiver_api::StoreId::Epic {
            bail!("{r} isn't installed (mods need an Epic game Quiver installed)");
        }
        if r.as_steam().is_none() {
            bail!("{r}: mods for {} games aren't built yet", r.store.label());
        }
        let e = crate::resolve_any(client, game).await.with_context(|| format!("{r} is not installed in Steam"))?;
        return Ok(steam_game(e.name, r));
    }
    let norm = |s: &str| s.to_lowercase().replace(['®', '™', '’', '\''], "");
    let needle = norm(game);
    let matches: Vec<GameProfiles> = games.into_iter().filter(|g| norm(&g.name).contains(&needle)).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => match crate::resolve_any(client, game).await {
            Ok(e) if e.game.as_steam().is_some() => Ok(steam_game(e.name, e.game)),
            _ => bail!("no installed game matches '{game}'"),
        },
        _ => bail!(
            "'{game}' matches several games: {}",
            matches.iter().map(|g| format!("{} ({})", g.name, GameArg(g.game_ref()))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// For a Steam game, what decides whether its mods reach the game: the hook, and the saves.
pub(crate) async fn steam_note(client: &Client, game: GameRef) -> Option<String> {
    let app = game.as_steam()?;
    let hooked = client
        .call::<quiver_api::exports::SteamHook>(Request::SteamHookStatus { app_id: app, steam_user: None })
        .await
        .is_ok_and(|h| h.hooked());
    let saves = "every profile uses the game's real saves, which Steam Cloud syncs (profiles with saves of their own aren't built for Steam games yet)";
    Some(if hooked {
        format!("it starts through quiver steam-run, so Steam's Play runs this profile; {saves}")
    } else {
        format!("Steam starts it without Quiver, so mods don't reach it yet: quiver steam hook {game} on; {saves}")
    })
}

/// For "Ask" actions requested under an agent source: ask the person at the terminal.
/// Without a terminal nothing is confirmed and the daemon refuses.
fn confirm(source: &str, what: &str) -> Result<bool> {
    if is_human_source(source) || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!("'{source}' asks to {what}. Allow? [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// "deletes N files", for layers that delete anything.
fn deletes_note(l: &Layer) -> Option<String> {
    match l.deletes.len() {
        0 => None,
        1 => Some("deletes 1 file".into()),
        n => Some(format!("deletes {n} files")),
    }
}

fn print_change(game: &str, verb: &str, c: &LayerChange) {
    let l = &c.layer;
    println!(
        "{game}: {verb} layer {} '{}' ({} files, {}; {}; revision {})",
        l.id,
        l.name,
        l.files,
        size(l.bytes),
        deletes_note(l).unwrap_or_else(|| "deletes nothing".into()),
        c.revision
    );
    for w in &c.warnings {
        eprintln!("note: {w}");
    }
}

fn print_layer(l: &Layer) {
    println!("{} '{}'", l.id, l.name);
    println!("  {} files, {}", l.files, size(l.bytes));
    println!("  added {} by {}", l.added.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), l.added_by);
    if let Some(o) = &l.origin {
        println!("  copied from {}", o.display());
    }
    match deletes_note(l) {
        None => println!("  deletes nothing"),
        Some(d) => {
            println!("  {d} (hidden in launches with this layer; the install keeps them):");
            for d in &l.deletes {
                println!("    {d}");
            }
        }
    }
}

/// What a mod declares besides files, in a few words.
fn mod_flags(l: &Layer) -> String {
    let mut f = Vec::new();
    f.push(l.kind().as_str().to_owned());
    if l.builtin {
        f.push("built-in".into());
    }
    match l.manifest.as_ref().and_then(|m| m.save_compatible) {
        Some(true) => f.push("save-compatible".into()),
        Some(false) => f.push("not save-compatible".into()),
        None => f.push("save compatibility unknown".into()),
    }
    let launch = l.launch();
    if !launch.is_empty() {
        f.push("launch settings".into());
    }
    if !l.patches().is_empty() {
        f.push(format!("{} config patch(es)", l.patches().len()));
    }
    if !l.enabled() {
        f.push("TURNED OFF".into());
    }
    f.join(", ")
}

/// The profile Play uses when none is named: "DLSS (ptkzxpv)", "vanilla", or a deleted
/// default ("vanilla: the default 'DLSS' (ptkzxpv) was deleted").
pub fn default_label(s: &ModsState) -> String {
    let Some(id) = s.default_profile.as_deref() else { return "vanilla".into() };
    if let Some(i) = s.profiles.iter().find(|i| i.profile.id == id) {
        return format!("{} ({id})", i.profile.name);
    }
    match s.deleted.iter().find(|i| i.profile.id == id) {
        Some(i) => format!("vanilla: the default '{}' ({id}) was deleted", i.profile.name),
        None => format!("vanilla: the default {id} doesn't exist"),
    }
}

/// "current v3 of 3 versions (1 retired)", or "" for a mod with one version.
fn versions_note(r: &ModRecord) -> String {
    if r.versions.len() <= 1 {
        return String::new();
    }
    let retired = r.versions.iter().filter(|v| v.retired).count();
    let pre = r.versions.iter().filter(|v| v.prerelease).count();
    let mut extra = Vec::new();
    if retired > 0 {
        extra.push(format!("{retired} retired"));
    }
    if pre > 0 {
        extra.push(format!("{pre} pre-release"));
    }
    format!(
        "current {} of {} versions{}",
        r.label_of(&r.current),
        r.versions.len(),
        if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) }
    )
}

/// A stack entry in words: "DLSS v3 (current)", "Skip intro videos", "lxyz (missing)".
fn entry_label(s: &ModsState, e: &str) -> String {
    s.resolve(e).map_or(format!("{e} (missing)"), |r| r.describe())
}

fn print_state(name: &str, s: &ModsState) {
    let layer_name = |id: &String| if s.mod_of(id).is_some() { s.display(id) } else { format!("{id} (missing)") };
    let key = GameArg(s.game_ref());
    println!("{name} ({key})");
    println!("mods:");
    let mods = s.all_mods();
    if mods.is_empty() {
        println!("  (none; quiver mods {} catalog, install ID|DIR, or layer add NAME DIR)", key);
    }
    for m in &mods {
        let Some(l) = s.layer(&m.current) else {
            println!("  {}  {:<24} current version {} is missing", m.id, m.name, m.current);
            continue;
        };
        println!(
            "  {}  {:<24} {:>6} files {:>9}  added {} by {}{}",
            m.id,
            m.name,
            l.files,
            size(l.bytes),
            l.added.with_timezone(&chrono::Local).format("%Y-%m-%d"),
            l.added_by,
            deletes_note(l).map(|d| format!("; {d}")).unwrap_or_default()
        );
        let mut flags = mod_flags(l);
        if m.disabled && l.enabled() {
            flags.push_str(", TURNED OFF");
        }
        let versions = versions_note(m);
        println!(
            "           {flags}{}{}",
            // A package's id; a store id (what `set` gave a layer) says nothing new.
            l.mod_id().filter(|x| *x != m.id && !m.has(x)).map(|x| format!(" [{x}]")).unwrap_or_default(),
            if versions.is_empty() { String::new() } else { format!("; {versions} (quiver mods {} version list {})", key, m.id) }
        );
    }
    println!("profiles (Play uses {}; change it: quiver mods {} default PROFILE|vanilla):", default_label(s), key);
    let fixes = match s.fixes_on_vanilla {
        None => "fixes on unmodded games: not decided (quiver fixes-on-vanilla on|off)".to_owned(),
        Some(false) => "fixes on unmodded games: off".to_owned(),
        Some(true) if s.vanilla_fixes.is_empty() => "fixes on: no fix installed".to_owned(),
        Some(true) => format!("with fixes: {}", s.vanilla_fixes.iter().map(layer_name).collect::<Vec<_>>().join(" < ")),
    };
    let live_default = s.default_profile.as_deref().filter(|d| s.profiles.iter().any(|i| i.profile.id == *d));
    println!(
        "  {:<7}  {:<24} the game as installed; {fixes}{}",
        quiver_api::mods::VANILLA,
        "Vanilla",
        if live_default.is_none() { "  [default]" } else { "" }
    );
    if let Some(v) = &s.vanilla_saves {
        println!("           saves: {}", saves_line(v));
    }
    let print_profile = |i: &ProfileInfo| {
        let p = &i.profile;
        let layers = if p.layers.is_empty() { "no layers".into() } else { p.layers.iter().map(|e| entry_label(s, e)).collect::<Vec<_>>().join(" < ") };
        let store = i.saves.as_ref().and_then(|s| s.store_name.clone()).map_or("saves: not migrated".to_owned(), |n| format!("saves: {n}"));
        let mut flags = vec![format!("r{}", i.revision), store];
        if s.default_profile.as_deref() == Some(p.id.as_str()) {
            flags.insert(0, "default".into());
        }
        if p.pinned {
            flags.push("pinned".into());
        }
        println!("  {}  {:<24} {}  [{}]", p.id, p.name, layers, flags.join(", "));
        let overrides = serde_json::to_value(&p.settings).unwrap_or_default();
        if overrides.as_object().is_some_and(|o| !o.is_empty()) {
            println!("           overrides: {overrides}");
        }
        if let Some(saves) = &i.saves {
            println!("           saves: {}", saves_line(saves));
        }
        for d in &i.config_dirs {
            println!("           own folder: {} (files live in {})", d.dir, d.upper.display());
        }
        for c in &i.setting_conflicts {
            println!("           conflict: {}", conflict_line(c, &layer_name));
        }
        for pr in &i.problems {
            println!("           won't launch: {pr}");
        }
        for w in &i.warnings {
            println!("           note: {w}");
        }
        for c in &i.conflicts {
            let by = c
                .layers
                .iter()
                .map(|id| if c.deleted_by.contains(id) { format!("{} (deletes)", layer_name(id)) } else { layer_name(id) })
                .collect::<Vec<_>>();
            let hidden = if c.layers.last().is_some_and(|top| c.deleted_by.contains(top)) { ", hidden" } else { "" };
            println!("           {} from {} (top wins{hidden})", c.path, by.join(" < "));
        }
    };
    s.profiles.iter().for_each(print_profile);
    if !s.deleted.is_empty() {
        println!("deleted (quiver mods {} profile restore ID REVISION):", key);
        s.deleted.iter().for_each(print_profile);
    }
}

/// "DLSS store, savegames/<user>/3539-ptkzxpv (why); note" for a saves plan.
fn saves_line(s: &quiver_api::mods::SavesPlan) -> String {
    let mut out = format!("{} ({})", s.folder, s.reason);
    if let Some(n) = &s.cloud_note {
        out.push_str(&format!("; note: {n}"));
    }
    out
}

fn conflict_line(c: &SettingConflict, name: &dyn Fn(&String) -> String) -> String {
    let by: Vec<String> = c
        .by
        .iter()
        .map(|b| {
            let who = if b.from == "game" || b.from == "profile" { b.from.clone() } else { name(&b.from) };
            format!("{who}: {}", b.value.as_deref().unwrap_or("(deleted)"))
        })
        .collect();
    format!("{} set by {} (the last wins)", c.key, by.join(", "))
}

fn patch_state(s: PatchState) -> &'static str {
    match s {
        PatchState::Applied => "applied",
        PatchState::Unchanged => "already set",
        PatchState::Pending => "pending until first run",
        PatchState::Undone => "undone",
        PatchState::Failed => "FAILED",
        PatchState::Unknown => "applied at launch",
    }
}

fn print_patches(list: &[PatchStatus], name: &dyn Fn(&String) -> String) {
    for s in list {
        println!("  {} [{}] from {}: {}", s.file, s.target, name(&s.mod_id), patch_state(s.state));
        for f in &s.files {
            let whose = if f.scope == "prefix" { "the prefix's" } else { "the profile's own copy" };
            println!("      {} ({whose}: {})", f.rel, f.path.display());
        }
        if !s.detail.is_empty() && s.state != PatchState::Pending {
            println!("      {}", s.detail);
        }
    }
}

fn print_plan(game: &str, p: &LaunchPlan, state: &ModsState) {
    let name = |id: &String| if state.mod_of(id).is_some() { format!("{} ({id})", state.display(id)) } else { id.clone() };
    match &p.profile {
        Some(pr) => println!("{game}: profile '{}' ({}) r{}", pr.name, pr.id, p.revision),
        None => println!("{game}: vanilla"),
    }
    let mods: Vec<String> = if p.resolved.len() == p.mods.len() {
        p.resolved.iter().map(|r| format!("{} ({})", r.describe(), r.version)).collect()
    } else {
        p.mods.iter().map(name).collect()
    };
    println!("mods (bottom to top): {}", if mods.is_empty() { "none".into() } else { mods.join(" < ") });
    println!("settings: {}", serde_json::to_string(&p.settings).unwrap_or_default());
    println!("saves: {}", saves_line(&p.saves));
    for m in &p.mounts {
        let lower: Vec<String> = m.lower.iter().map(|l| l.display().to_string()).collect();
        println!("mount: {} <- [{}] writes to {}", m.target.display(), lower.join(", "), m.upper.display());
    }
    if p.config_dirs.is_empty() && p.profile.is_some() {
        println!("per-profile prefix folders: none (the profile sees the prefix's own settings)");
    }
    for d in &p.config_dirs {
        println!("per-profile folder: {}", d.dir);
        if let Some(pp) = &d.prefix_path {
            println!("    the game sees {} with this profile's changes on top", pp.display());
        }
        println!("    this profile's files: {} ({} file(s))", d.upper.display(), d.files.len());
        for f in &d.files {
            println!("      {f}");
        }
    }
    if !p.patches.is_empty() {
        println!("config patches:");
        print_patches(&p.patches, &name);
    }
    for c in &p.setting_conflicts {
        println!("conflict: {}", conflict_line(c, &name));
    }
    for c in &p.file_conflicts {
        println!("file conflict: {} from {} (the top one wins)", c.path, c.layers.iter().map(name).collect::<Vec<_>>().join(" < "));
    }
    for w in &p.warnings {
        println!("note: {w}");
    }
    for pr in &p.problems {
        println!("WON'T LAUNCH: {pr}");
    }
}

fn print_detail(d: &ModDetail) {
    let l = &d.layer;
    print_layer(l);
    println!("  {}", mod_flags(l));
    if let Some(m) = &l.manifest {
        if let Some(v) = &m.version {
            println!("  version {v}");
        }
        if let Some(desc) = &m.description {
            println!("  {desc}");
        }
        if let Some(s) = &m.source {
            println!("  source: {s}");
        }
        if let Some(a) = &m.audited {
            println!("  audited: {a}");
        }
        if !m.builds.is_empty() {
            println!("  known-good builds: {}", m.builds.join(", "));
        }
        if !m.requires.is_empty() {
            println!("  requires: {}", m.requires.join(", "));
        }
        if !m.conflicts.is_empty() {
            println!("  conflicts with: {}", m.conflicts.join(", "));
        }
        if !m.launch.is_empty() {
            println!("  launch settings: {}", serde_json::to_string(&m.launch).unwrap_or_default());
        }
        for p in &m.patches {
            let set: Vec<String> = p.set.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let del: Vec<String> = p.delete.iter().map(|k| format!("-{k}")).collect();
            println!("  patch {} [{}]: {}", p.file, p.target(), set.into_iter().chain(del).collect::<Vec<_>>().join(" "));
        }
    }
    if let Some(r) = d.record.as_ref().filter(|r| r.versions.len() > 1) {
        println!("  {} (shown: {})", versions_note(r), r.label_of(&l.id));
    }
    let used = if d.used_as.is_empty() { d.used_by.clone() } else { d.used_as.clone() };
    println!("  used by {}{}", if used.is_empty() { "no profile".into() } else { used.join("; ") }, if d.on_vanilla { "; vanilla (fixes on)" } else { "" });
    for k in &d.applied {
        println!(
            "  changed now: {} [{}] {} = {} (the file's own: {}) in {}",
            k.file,
            k.target,
            k.key,
            k.applied.as_deref().unwrap_or("(deleted)"),
            if k.original.is_empty() { "(not set)".into() } else { k.original.join(", ") },
            k.scope
        );
    }
    if let Some(u) = &d.catalog_update {
        println!("  update available: version {} (quiver mods <game> install {})", u.version.as_deref().unwrap_or("?"), u.id);
    }
}

/// Parse the reply to a profile change, printing the daemon's notes about the stack.
fn profile_reply(v: serde_json::Value) -> Result<ProfileRevision> {
    if let Some(notes) = v.get("warnings").and_then(|w| w.as_array()) {
        for n in notes.iter().filter_map(|n| n.as_str()) {
            eprintln!("note: {n}");
        }
    }
    if let Some(a) = v.get("saves_advice").and_then(|s| serde_json::from_value::<quiver_api::savestores::SavesAdvice>(s.clone()).ok()) {
        eprintln!("saves advice: {}", a.reason);
    }
    if let Some(s) = v.get("saves").and_then(|s| serde_json::from_value::<quiver_api::mods::SavesPlan>(s.clone()).ok()) {
        eprintln!("saves: {}", saves_line(&s));
    }
    Ok(serde_json::from_value(v)?)
}

pub async fn run(client: &Client, source: &str, game: &str, action: ModsAction) -> Result<()> {
    let g = resolve_installed(client, game).await?;
    let product_id = GameArg(g.game_ref());
    let call = |req: ModsRequest| client.call::<serde_json::Value>(Request::Mods(req));
    match action {
        ModsAction::List { json } => {
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                print_state(&g.name, &state);
                if let Some(n) = steam_note(client, product_id.0).await {
                    println!("note: {n}");
                }
            }
        }
        ModsAction::Default { profile, message } => {
            if let Some(p) = profile {
                let ops = vec![SettingOp::Set { key: "profile".into(), value: p }];
                let rev: quiver_api::Revision = client.call(Request::SettingsSet { game: product_id.into(), ops, message }).await?;
                eprintln!("{}: settings revision {} (quiver history {product_id} lists every change)", g.name, rev.revision);
            }
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
            if product_id.0.as_steam().is_some() {
                // Its `steam_profile` setting: what Play in Steam runs through quiver steam-run.
                println!("{}: Play in Steam runs {}", g.name, default_label(&state));
                if let Some(n) = steam_note(client, product_id.0).await {
                    println!("note: {n}");
                }
            } else {
                println!("{}: Play uses {}", g.name, default_label(&state));
            }
        }
        ModsAction::Layer { action: LayerAction::Add { name, dir, delete, version_of: Some(of) } } => {
            // "DLSS-v5" as a version of DLSS is "v5".
            let label = match name.get(..of.len()) {
                Some(head) if head.eq_ignore_ascii_case(&of) && name.len() > of.len() => {
                    name[of.len()..].trim_start_matches(['-', '_', ' ']).to_owned()
                }
                _ => name,
            };
            let action = VersionAction::Add {
                mod_id: of,
                dir,
                label: Some(label).filter(|l| !l.is_empty()),
                notes: None,
                no_update: false,
                prerelease: false,
                save_compatible: None,
                delete,
                options: None,
            };
            return run_version(client, &g, action).await;
        }
        ModsAction::Version { action } => return run_version(client, &g, action).await,
        ModsAction::Options { action, profile, json } => return crate::options::run(client, source, game, profile, json, action).await,
        ModsAction::MergeVersions { into, from, labels, dry_run, json } => {
            let confirmed = !dry_run && confirm(source, &format!("make {} versions of mod '{into}' of {} (profiles that stack them are rewritten)", from.join(", "), g.name))?;
            let req = ModsRequest::MergeVersions { game: product_id.into(), into, from, labels, dry_run, confirmed };
            let r: VersionMerge = client.call(Request::Mods(req)).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            println!("{}: {}", g.name, if dry_run { "would merge (dry run; nothing changed)" } else { "merged" });
            for n in &r.notes {
                println!("  {n}");
            }
            if r.profiles.is_empty() {
                println!("  no profile stack changes");
            }
            for (p, before, after, said) in &r.profiles {
                println!("  profile '{p}': {before} -> {after} ({said})");
            }
        }
        ModsAction::Layer { action: LayerAction::Add { name, dir, delete, version_of: None } } => {
            if dir.is_none() && delete.is_empty() {
                bail!("give a folder to copy, --delete PATH, or both");
            }
            // An empty path: a layer with deletions only.
            let path = match dir {
                Some(dir) => {
                    let path = dir.canonicalize().with_context(|| format!("{} not found", dir.display()))?;
                    eprintln!("copying {}…", path.display());
                    path
                }
                None => PathBuf::new(),
            };
            let c: LayerChange = client.call(Request::Mods(ModsRequest::LayerAdd { game: product_id.into(), name, path, deletes: delete })).await?;
            print_change(&g.name, "added", &c);
        }
        ModsAction::Layer { action: LayerAction::DeleteFiles { layer, paths, message } } => {
            let req = ModsRequest::LayerDeletes { game: product_id.into(), layer, delete: paths, keep: Vec::new(), message };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "changed", &c);
        }
        ModsAction::Layer { action: LayerAction::KeepFiles { layer, paths, message } } => {
            let req = ModsRequest::LayerDeletes { game: product_id.into(), layer, delete: Vec::new(), keep: paths, message };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "changed", &c);
        }
        ModsAction::Layer { action: LayerAction::Show { layer } } => {
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
            let found: Vec<&Layer> = match state.layers.iter().find(|l| l.id == layer) {
                Some(l) => vec![l],
                None => state.layers.iter().filter(|l| l.name.trim().to_lowercase() == layer.trim().to_lowercase()).collect(),
            };
            let [l] = found.as_slice() else { bail!("no single layer '{layer}' in {} (quiver mods {product_id} list)", g.name) };
            print_layer(l);
            let users: Vec<&str> =
                state.profiles.iter().filter(|i| i.profile.layers.contains(&l.id)).map(|i| i.profile.name.as_str()).collect();
            println!("  used by {}", if users.is_empty() { "no profile".into() } else { users.join(", ") });
        }
        ModsAction::Layer { action: LayerAction::History { layer } } => {
            let revs: Vec<LayerRevision> = client.call(Request::Mods(ModsRequest::LayerHistory { game: product_id.into(), layer })).await?;
            for r in revs {
                println!(
                    "r{:<3} {}  by {:<8} deletes {:<3} {}  {}",
                    r.revision,
                    r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    r.source,
                    r.deletes.len(),
                    r.deletes.join(", "),
                    r.message.unwrap_or_default()
                );
            }
        }
        ModsAction::Layer { action: LayerAction::Restore { layer, revision } } => {
            let confirmed = confirm(source, &format!("restore the deletions of mod layer '{layer}' of {} to revision {revision}", g.name))?;
            let req = ModsRequest::LayerRestore { game: product_id.into(), layer, revision, confirmed };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "restored", &c);
        }
        ModsAction::Layer { action: LayerAction::Remove { layer } } => {
            let confirmed = confirm(source, &format!("delete mod layer '{layer}' of {}", g.name))?;
            let l: Layer = client.call(Request::Mods(ModsRequest::LayerRemove { game: product_id.into(), layer, confirmed })).await?;
            println!("{}: removed layer '{}'", g.name, l.name);
        }
        ModsAction::Profile { action } => {
            let rev: ProfileRevision = match action {
                ProfileAction::Create { name, layers, saves, message } => {
                    profile_reply(call(ModsRequest::ProfileCreate { game: product_id.into(), name, layers, message, saves }).await?)?
                }
                ProfileAction::Set { profile, changes, message } => {
                    let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
                    profile_reply(call(ModsRequest::ProfileSet { game: product_id.into(), profile, ops, message }).await?)?
                }
                pin @ (ProfileAction::Pin { .. } | ProfileAction::Unpin { .. }) => {
                    let (profile, on) = match pin {
                        ProfileAction::Pin { profile } => (profile, "on"),
                        ProfileAction::Unpin { profile } => (profile, "off"),
                        _ => unreachable!(),
                    };
                    let ops = vec![SettingOp::Set { key: "pinned".into(), value: on.into() }];
                    serde_json::from_value(call(ModsRequest::ProfileSet { game: product_id.into(), profile, ops, message: None }).await?)?
                }
                ProfileAction::Delete { profile } => {
                    let confirmed = confirm(source, &format!("delete mod profile '{profile}' of {}", g.name))?;
                    let rev = profile_reply(call(ModsRequest::ProfileDelete { game: product_id.into(), profile, confirmed }).await?)?;
                    println!(
                        "{}: deleted profile '{}' ({}); its saves are kept. Undo: quiver mods {product_id} profile restore {} {}",
                        g.name,
                        rev.profile.name,
                        rev.profile.id,
                        rev.profile.id,
                        rev.revision - 1
                    );
                    return Ok(());
                }
                ProfileAction::History { profile } => {
                    let revs: Vec<ProfileRevision> =
                        serde_json::from_value(call(ModsRequest::ProfileHistory { game: product_id.into(), profile }).await?)?;
                    for r in revs {
                        println!(
                            "r{:<3} {}  by {:<8} {}{}  {}",
                            r.revision,
                            r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                            r.source,
                            if r.deleted { "(deleted) " } else { "" },
                            serde_json::to_string(&r.profile)?,
                            r.message.unwrap_or_default()
                        );
                    }
                    return Ok(());
                }
                ProfileAction::Restore { profile, revision } => {
                    let confirmed = confirm(source, &format!("restore mod profile '{profile}' of {} to revision {revision}", g.name))?;
                    serde_json::from_value(call(ModsRequest::ProfileRestore { game: product_id.into(), profile, revision, confirmed }).await?)?
                }
            };
            let p = &rev.profile;
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
            let names: Vec<String> = p.layers.iter().map(|e| entry_label(&state, e)).collect();
            let store = p.saves.as_deref().map(|id| {
                state.save_stores.iter().find(|s| s.id == id).map_or(id.to_owned(), |s| format!("{}{}", s.name, if s.cloud { ", synced" } else { ", this PC only" }))
            });
            println!(
                "{}: profile {} '{}' revision {} (mods: {}; saves: {}{}{})",
                g.name,
                p.id,
                p.name,
                rev.revision,
                if names.is_empty() { "none".into() } else { names.join(" < ") },
                store.unwrap_or_else(|| "not migrated yet".into()),
                if p.profile_config.is_empty() { String::new() } else { format!("; own folders: {}", p.profile_config.join(", ")) },
                if p.pinned { "; pinned" } else { "" }
            );
        }
        ModsAction::Catalog { json } => {
            let list: Vec<CatalogEntry> = client.call(Request::Mods(ModsRequest::Catalog { game: product_id.into() })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.is_empty() {
                println!("{}: no built-in mods yet", g.name);
            }
            for e in &list {
                let m = &e.manifest;
                let state = match (&e.installed, e.update) {
                    (Some(id), true) => format!("installed as {id}; update available"),
                    (Some(id), false) => format!("installed as {id}"),
                    (None, _) => "not installed".into(),
                };
                println!("{:<26} {} ({}; {state})", m.id, m.name, m.kind.as_str());
                if let Some(d) = &m.description {
                    println!("    {d}");
                }
                if let Some(s) = &m.source {
                    println!("    source: {s}");
                }
            }
        }
        ModsAction::Install { what, name } => {
            let as_dir = what.contains('/') || std::path::Path::new(&what).is_dir();
            let req = if as_dir {
                let path = PathBuf::from(&what).canonicalize().with_context(|| format!("{what} not found"))?;
                ModsRequest::Install { game: product_id.into(), builtin: None, path: Some(path), name }
            } else {
                ModsRequest::Install { game: product_id.into(), builtin: Some(what), path: None, name }
            };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "installed", &c);
            println!("  {}", mod_flags(&c.layer));
            println!("  add it to a profile: quiver mods {product_id} profile set PROFILE mods=…,{}", c.layer.mod_id().unwrap_or(&c.layer.id));
        }
        ModsAction::Uninstall { mod_id } => {
            let confirmed = confirm(source, &format!("uninstall mod '{mod_id}' of {} (its files, and config values it changed go back)", g.name))?;
            let v: serde_json::Value = call(ModsRequest::Uninstall { game: product_id.into(), mod_id, confirmed }).await?;
            let l: Layer = serde_json::from_value(v["mod"].clone())?;
            let undone: Vec<PatchStatus> = serde_json::from_value(v["undone"].clone()).unwrap_or_default();
            println!("{}: uninstalled '{}' ({})", g.name, l.name, l.id);
            print_patches(&undone, &|id| id.clone());
        }
        ModsAction::Show { mod_id, json } => {
            let d: ModDetail = client.call(Request::Mods(ModsRequest::Show { game: product_id.into(), mod_id })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print_detail(&d);
            }
        }
        ModsAction::Set { mod_id, changes, message } => {
            let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
            let c: LayerChange = client.call(Request::Mods(ModsRequest::ModSet { game: product_id.into(), mod_id, ops, message })).await?;
            print_change(&g.name, "changed", &c);
            println!("  {}", mod_flags(&c.layer));
        }
        ModsAction::Plan { profile, json } => {
            let plan: LaunchPlan = client.call(Request::Mods(ModsRequest::Plan { game: product_id.into(), profile })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
                print_plan(&g.name, &plan, &state);
            }
        }
        ModsAction::UndoPatches { mod_id } => {
            let what = match &mod_id {
                Some(m) => format!("put back the config values mod '{m}' of {} changed", g.name),
                None => format!("put back every config value Quiver changed for {}", g.name),
            };
            let confirmed = confirm(source, &what)?;
            let undone: Vec<PatchStatus> = client.call(Request::Mods(ModsRequest::PatchesUndo { game: product_id.into(), mod_id, confirmed })).await?;
            if undone.is_empty() {
                println!("{}: nothing to undo", g.name);
            }
            print_patches(&undone, &|id| id.clone());
        }
        ModsAction::Sdk { action: SdkAction::Status { json } } => {
            let conns: Vec<ModSdkConnection> = client.call(Request::ModSdk(ModSdkRequest::Status { product_id: product_id.0.as_ubisoft() })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&conns)?);
            } else {
                print_sdk(&g.name, &conns);
            }
        }
        ModsAction::Migrate { dry_run } => {
            let r: MigrationReport = client.call(Request::Mods(ModsRequest::Migrate { game: product_id.into(), dry_run })).await?;
            println!("{}: {}", g.name, if dry_run { "would migrate (dry run)" } else { "migrated" });
            println!("  file-only mods (no change needed): {}", if r.file_only_mods.is_empty() { "none".into() } else { r.file_only_mods.join(", ") });
            if r.notes.is_empty() {
                println!("  every profile already has a save store");
            }
            for n in &r.notes {
                println!("  {n}");
            }
        }
    }
    Ok(())
}

fn print_sdk(game: &str, conns: &[ModSdkConnection]) {
    if conns.is_empty() {
        println!("{game}: no mod is connected through the mod SDK (the game isn't running, or none of its mods uses quiver_mod.h)");
        return;
    }
    println!("{game}: {} mod connection(s) through the mod SDK", conns.len());
    for c in conns {
        let since = c.connected.with_timezone(&chrono::Local).format("%H:%M:%S");
        let mut who = vec![format!("{} protocol {}", if c.sdk.is_empty() { "SDK" } else { &c.sdk }, c.proto)];
        if let Some(v) = &c.version {
            who.push(format!("version {v}"));
        }
        if let Some(m) = &c.module {
            who.push(m.clone());
        }
        if let Some(p) = c.pid.or(c.client_pid) {
            who.push(format!("pid {p}"));
        }
        println!("  {} (#{}, since {since}): {}", c.surface, c.conn, who.join(", "));
        println!("    capabilities: {}", if c.caps.is_empty() { "none".into() } else { c.caps.join(", ") });
        if let Some(s) = &c.session {
            println!("    session {s}{}", c.profile.as_deref().map(|p| format!(", profile {p}")).unwrap_or_default());
        }
        if !c.options.is_empty() {
            println!("    applies live: {}", c.options.join(", "));
        }
        for m in &c.metrics {
            let value = match &m.value {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => format!("{v}{}", if m.unit.is_empty() { String::new() } else { format!(" {}", m.unit) }),
                None => "—".into(),
            };
            let marks = if m.marks > 0 { format!(" ({} marks)", m.marks) } else { String::new() };
            println!("    {:<32} {:<8} {value}{marks}", m.id, m.kind);
        }
        if c.refused > 0 {
            println!("    {} message(s) refused (malformed or over a rate limit)", c.refused);
        }
    }
}

fn print_version_change(game: &str, verb: &str, c: &VersionChange) {
    let r = &c.record;
    let label = r.label_of(&c.version.id);
    let current = r.current == c.version.id;
    println!(
        "{game}: {verb} {} {label} ({}; {} files, {}){}",
        r.name,
        c.version.id,
        c.version.files,
        size(c.version.bytes),
        if c.revision > 0 { format!("; mod revision {}", c.revision) } else { String::new() }
    );
    println!("  {}", mod_flags(&c.version));
    println!("  current version: {}{}", r.label_of(&r.current), if current { String::new() } else { format!(" (not {label})") });
    if !c.tracking.is_empty() {
        println!("  tracks current (runs {} next launch): {}", r.label_of(&r.current), c.tracking.join(", "));
    }
    if !c.pinned.is_empty() {
        println!("  pinned: {}", c.pinned.join(", "));
    }
    for w in &c.warnings {
        eprintln!("note: {w}");
    }
}

/// `quiver mods <game> version …`.
async fn run_version(client: &Client, g: &GameProfiles, action: VersionAction) -> Result<()> {
    let product_id = GameArg(g.game_ref());
    let change = |req: ModsRequest| client.call::<VersionChange>(Request::Mods(req));
    match action {
        VersionAction::Add { mod_id, dir, label, notes, no_update, prerelease, save_compatible, delete, options } => {
            let options = options.map(|f| f.canonicalize().with_context(|| format!("{} not found", f.display()))).transpose()?;
            let path = match dir {
                Some(dir) => {
                    let path = dir.canonicalize().with_context(|| format!("{} not found", dir.display()))?;
                    eprintln!("copying {}…", path.display());
                    path
                }
                None => PathBuf::new(),
            };
            let req = ModsRequest::VersionAdd { game: product_id.into(), mod_id, path, label, notes, no_update, prerelease, save_compatible, deletes: delete, options };
            print_version_change(&g.name, "added", &change(req).await?);
        }
        VersionAction::Use { mod_id, version, message } => {
            print_version_change(&g.name, "now current:", &change(ModsRequest::VersionUse { game: product_id.into(), mod_id, version, message }).await?);
        }
        VersionAction::Retire { mod_id, version, message } => {
            let req = ModsRequest::VersionRetire { game: product_id.into(), mod_id, version, retired: true, message };
            print_version_change(&g.name, "retired", &change(req).await?);
        }
        VersionAction::Unretire { mod_id, version } => {
            let req = ModsRequest::VersionRetire { game: product_id.into(), mod_id, version, retired: false, message: None };
            print_version_change(&g.name, "brought back", &change(req).await?);
        }
        VersionAction::History { mod_id } => {
            let revs: Vec<ModRevision> = client.call(Request::Mods(ModsRequest::ModHistory { game: product_id.into(), mod_id: mod_id.clone() })).await?;
            if revs.is_empty() {
                println!("{}: '{mod_id}' has one version and no history yet (it gets one with a second version)", g.name);
            }
            for r in revs {
                println!(
                    "r{:<3} {}  by {:<8} {}{}  {}",
                    r.revision,
                    r.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    r.source,
                    if r.deleted { "(merged away/removed) " } else { "" },
                    r.changes.join("; "),
                    r.message.unwrap_or_default()
                );
            }
        }
        VersionAction::List { mod_id, json } => {
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { game: product_id.into() })).await?;
            let d: ModDetail = client.call(Request::Mods(ModsRequest::Show { game: product_id.into(), mod_id })).await?;
            let r = d.record.clone().or_else(|| state.mod_of(&d.layer.id)).context("the daemon didn't say which mod that is")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            println!("{} ({}): {} version(s), current {}{}", r.name, r.id, r.versions.len(), r.label_of(&r.current), if r.disabled { "; TURNED OFF" } else { "" });
            let users = state.users_of(&r.id);
            for v in r.versions.iter().rev() {
                let l = state.layer(&v.id);
                let mut marks = Vec::new();
                if v.id == r.current {
                    marks.push("current".to_owned());
                }
                if v.retired {
                    marks.push("retired".to_owned());
                }
                if v.prerelease {
                    marks.push("pre-release".to_owned());
                }
                let pins: Vec<&str> = users.iter().filter(|(_, x)| x.pinned && x.version == v.id).map(|(p, _)| p.profile.name.as_str()).collect();
                if !pins.is_empty() {
                    marks.push(format!("pinned by {}", pins.join(", ")));
                }
                println!(
                    "  {} {:<12} {}  {}  by {:<8} {}{}",
                    if v.id == r.current { "*" } else { " " },
                    v.label,
                    v.id,
                    v.added.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                    v.added_by,
                    l.map(|l| format!("{} files, {}; {}", l.files, size(l.bytes), mod_flags(l))).unwrap_or_else(|| "(missing)".into()),
                    if marks.is_empty() { String::new() } else { format!("  [{}]", marks.join(", ")) }
                );
                if let Some(n) = &v.notes {
                    println!("      {n}");
                }
            }
            let tracking: Vec<&str> = users.iter().filter(|(_, x)| !x.pinned).map(|(p, _)| p.profile.name.as_str()).collect();
            println!("tracks current: {}", if tracking.is_empty() { "no profile".into() } else { tracking.join(", ") });
        }
    }
    Ok(())
}

/// `quiver fixes-on-vanilla [on|off]`: the app-level Mods v2 setting.
pub async fn fixes_on_vanilla(client: &Client, value: Option<String>) -> Result<()> {
    let set = match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => None,
        Some("on" | "true" | "yes") => Some(true),
        Some("off" | "false" | "no") => Some(false),
        Some(o) => bail!("expected on or off, got '{o}'"),
    };
    let p: ModPrefs = client.call(Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: set })).await?;
    println!(
        "fixes on unmodded games: {}",
        match p.fixes_on_vanilla {
            None => "not decided yet (quiver fixes-on-vanilla on|off)",
            Some(true) => "on (installed, enabled built-in fixes also apply to vanilla launches)",
            Some(false) => "off (vanilla launches run the game as installed)",
        }
    );
    Ok(())
}
