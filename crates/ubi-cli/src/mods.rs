//! `quiver mods <game> …`: mod layers and profiles. Games are looked up among installed
//! games, so none of this needs a Ubisoft request.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ubi_api::mods::{
    CatalogEntry, GameProfiles, LaunchPlan, Layer, LayerChange, LayerRevision, MigrationReport, ModDetail, ModPrefs, ModsState,
    PatchState, PatchStatus, ProfileInfo, ProfileRevision, SettingConflict, is_human_source,
};
use ubi_api::{Client, ModsRequest, Request, SettingOp};

#[derive(Subcommand)]
pub enum ModsAction {
    /// Layers and profiles (vanilla is always there and has no layers).
    List {
        #[arg(long)]
        json: bool,
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
        /// Profile name or id (default: vanilla).
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put config values Quiver changed back to the files' own now (all mods, or one).
    UndoPatches {
        #[arg(value_name = "MOD")]
        mod_id: Option<String>,
    },
    /// Make Mods v1 data explicit: profiles that have their own saves folder get
    /// separate_saves=on. Launches do this on their own; `--dry-run` shows what it would do.
    Migrate {
        #[arg(long)]
        dry_run: bool,
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
pub enum ProfileAction {
    /// A new profile; `--layers a,b` stacks layer b on top of a.
    Create {
        name: String,
        #[arg(long, value_delimiter = ',')]
        layers: Vec<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Change a profile: mods=a,b (bottom to top; also layers=)  cloud_saves=on|off
    /// pinned=on|off  separate_saves=on|off|auto  profile_config=DIR,… (prefix folders the
    /// profile keeps its own copy of)  name=…, or a launch-setting override (proton=…,
    /// env.NAME=…, dll.NAME=…, args=…, wrappers=…).
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

/// An installed game by product id or unique name match.
async fn resolve_installed(client: &Client, game: &str) -> Result<GameProfiles> {
    let games: Vec<GameProfiles> = client.call(Request::Mods(ModsRequest::Games)).await?;
    if let Ok(id) = game.parse::<u32>() {
        return games.into_iter().find(|g| g.product_id == id).with_context(|| format!("product {id} is not installed"));
    }
    let norm = |s: &str| s.to_lowercase().replace(['®', '™', '’', '\''], "");
    let needle = norm(game);
    let matches: Vec<GameProfiles> = games.into_iter().filter(|g| norm(&g.name).contains(&needle)).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no installed game matches '{game}'"),
        _ => bail!(
            "'{game}' matches several games: {}",
            matches.iter().map(|g| format!("{} ({})", g.name, g.product_id)).collect::<Vec<_>>().join(", ")
        ),
    }
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

fn print_state(name: &str, s: &ModsState) {
    let layer_name = |id: &String| s.layers.iter().find(|l| &l.id == id).map_or(format!("{id} (missing)"), |l| l.name.clone());
    println!("{name} ({})", s.product_id);
    println!("mods:");
    if s.layers.is_empty() {
        println!("  (none; quiver mods {} catalog, install ID|DIR, or layer add NAME DIR)", s.product_id);
    }
    for l in &s.layers {
        println!(
            "  {}  {:<24} {:>6} files {:>9}  added {} by {}{}",
            l.id,
            l.name,
            l.files,
            size(l.bytes),
            l.added.with_timezone(&chrono::Local).format("%Y-%m-%d"),
            l.added_by,
            deletes_note(l).map(|d| format!("; {d}")).unwrap_or_default()
        );
        println!("           {}{}", mod_flags(l), l.mod_id().map(|m| format!(" [{m}]")).unwrap_or_default());
    }
    println!("profiles:");
    let fixes = match s.fixes_on_vanilla {
        None => "fixes on unmodded games: not decided (quiver fixes-on-vanilla on|off)".to_owned(),
        Some(false) => "fixes on unmodded games: off".to_owned(),
        Some(true) if s.vanilla_fixes.is_empty() => "fixes on: no fix installed".to_owned(),
        Some(true) => format!("with fixes: {}", s.vanilla_fixes.iter().map(layer_name).collect::<Vec<_>>().join(" < ")),
    };
    println!("  {:<7}  {:<24} the game as installed; {fixes}", ubi_api::mods::VANILLA, "Vanilla");
    let print_profile = |i: &ProfileInfo| {
        let p = &i.profile;
        let layers = if p.layers.is_empty() { "no layers".into() } else { p.layers.iter().map(layer_name).collect::<Vec<_>>().join(" < ") };
        let mut flags = vec![format!("r{}", i.revision), format!("cloud saves {}", if p.cloud_saves { "on" } else { "off" })];
        if p.pinned {
            flags.push("pinned".into());
        }
        println!("  {}  {:<24} {}  [{}]", p.id, p.name, layers, flags.join(", "));
        let overrides = serde_json::to_value(&p.settings).unwrap_or_default();
        if overrides.as_object().is_some_and(|o| !o.is_empty()) {
            println!("           overrides: {overrides}");
        }
        if let Some(saves) = &i.saves {
            println!("           saves: {} ({})", saves.folder, saves.reason);
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
        println!("deleted (quiver mods {} profile restore ID REVISION):", s.product_id);
        s.deleted.iter().for_each(print_profile);
    }
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

fn print_plan(game: &str, p: &LaunchPlan, mods: &[Layer]) {
    let name = |id: &String| mods.iter().find(|l| &l.id == id).map_or(id.clone(), |l| format!("{} ({id})", l.name));
    match &p.profile {
        Some(pr) => println!("{game}: profile '{}' ({}) r{}", pr.name, pr.id, p.revision),
        None => println!("{game}: vanilla"),
    }
    println!("mods (bottom to top): {}", if p.mods.is_empty() { "none".into() } else { p.mods.iter().map(name).collect::<Vec<_>>().join(" < ") });
    println!("settings: {}", serde_json::to_string(&p.settings).unwrap_or_default());
    println!("saves: {} ({})", p.saves.folder, p.saves.reason);
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
    println!("  used by {}{}", if d.used_by.is_empty() { "no profile".into() } else { d.used_by.join(", ") }, if d.on_vanilla { "; vanilla (fixes on)" } else { "" });
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
    if let Some(s) = v.get("saves").and_then(|s| serde_json::from_value::<ubi_api::mods::SavesPlan>(s.clone()).ok()) {
        eprintln!("saves: {} ({})", s.folder, s.reason);
    }
    Ok(serde_json::from_value(v)?)
}

pub async fn run(client: &Client, source: &str, game: &str, action: ModsAction) -> Result<()> {
    let g = resolve_installed(client, game).await?;
    let product_id = g.product_id;
    let call = |req: ModsRequest| client.call::<serde_json::Value>(Request::Mods(req));
    match action {
        ModsAction::List { json } => {
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { product_id })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                print_state(&g.name, &state);
            }
        }
        ModsAction::Layer { action: LayerAction::Add { name, dir, delete } } => {
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
            let c: LayerChange = client.call(Request::Mods(ModsRequest::LayerAdd { product_id, name, path, deletes: delete })).await?;
            print_change(&g.name, "added", &c);
        }
        ModsAction::Layer { action: LayerAction::DeleteFiles { layer, paths, message } } => {
            let req = ModsRequest::LayerDeletes { product_id, layer, delete: paths, keep: Vec::new(), message };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "changed", &c);
        }
        ModsAction::Layer { action: LayerAction::KeepFiles { layer, paths, message } } => {
            let req = ModsRequest::LayerDeletes { product_id, layer, delete: Vec::new(), keep: paths, message };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "changed", &c);
        }
        ModsAction::Layer { action: LayerAction::Show { layer } } => {
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { product_id })).await?;
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
            let revs: Vec<LayerRevision> = client.call(Request::Mods(ModsRequest::LayerHistory { product_id, layer })).await?;
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
            let req = ModsRequest::LayerRestore { product_id, layer, revision, confirmed };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "restored", &c);
        }
        ModsAction::Layer { action: LayerAction::Remove { layer } } => {
            let confirmed = confirm(source, &format!("delete mod layer '{layer}' of {}", g.name))?;
            let l: Layer = client.call(Request::Mods(ModsRequest::LayerRemove { product_id, layer, confirmed })).await?;
            println!("{}: removed layer '{}'", g.name, l.name);
        }
        ModsAction::Profile { action } => {
            let rev: ProfileRevision = match action {
                ProfileAction::Create { name, layers, message } => {
                    profile_reply(call(ModsRequest::ProfileCreate { product_id, name, layers, message }).await?)?
                }
                ProfileAction::Set { profile, changes, message } => {
                    let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
                    profile_reply(call(ModsRequest::ProfileSet { product_id, profile, ops, message }).await?)?
                }
                pin @ (ProfileAction::Pin { .. } | ProfileAction::Unpin { .. }) => {
                    let (profile, on) = match pin {
                        ProfileAction::Pin { profile } => (profile, "on"),
                        ProfileAction::Unpin { profile } => (profile, "off"),
                        _ => unreachable!(),
                    };
                    let ops = vec![SettingOp::Set { key: "pinned".into(), value: on.into() }];
                    serde_json::from_value(call(ModsRequest::ProfileSet { product_id, profile, ops, message: None }).await?)?
                }
                ProfileAction::Delete { profile } => {
                    let confirmed = confirm(source, &format!("delete mod profile '{profile}' of {}", g.name))?;
                    let rev: ProfileRevision =
                        serde_json::from_value(call(ModsRequest::ProfileDelete { product_id, profile, confirmed }).await?)?;
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
                        serde_json::from_value(call(ModsRequest::ProfileHistory { product_id, profile }).await?)?;
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
                    serde_json::from_value(call(ModsRequest::ProfileRestore { product_id, profile, revision, confirmed }).await?)?
                }
            };
            let p = &rev.profile;
            let state: ModsState = client.call(Request::Mods(ModsRequest::List { product_id })).await?;
            let names: Vec<String> = p
                .layers
                .iter()
                .map(|id| state.layers.iter().find(|l| &l.id == id).map_or(id.clone(), |l| l.name.clone()))
                .collect();
            println!(
                "{}: profile {} '{}' revision {} (mods: {}; cloud saves {}{}{}{})",
                g.name,
                p.id,
                p.name,
                rev.revision,
                if names.is_empty() { "none".into() } else { names.join(" < ") },
                if p.cloud_saves { "on" } else { "off" },
                match p.separate_saves {
                    Some(true) => "; separate saves",
                    Some(false) => "; shares vanilla's saves",
                    None => "",
                },
                if p.profile_config.is_empty() { String::new() } else { format!("; own folders: {}", p.profile_config.join(", ")) },
                if p.pinned { "; pinned" } else { "" }
            );
        }
        ModsAction::Catalog { json } => {
            let list: Vec<CatalogEntry> = client.call(Request::Mods(ModsRequest::Catalog { product_id })).await?;
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
                ModsRequest::Install { product_id, builtin: None, path: Some(path), name }
            } else {
                ModsRequest::Install { product_id, builtin: Some(what), path: None, name }
            };
            let c: LayerChange = client.call(Request::Mods(req)).await?;
            print_change(&g.name, "installed", &c);
            println!("  {}", mod_flags(&c.layer));
            println!("  add it to a profile: quiver mods {product_id} profile set PROFILE mods=…,{}", c.layer.mod_id().unwrap_or(&c.layer.id));
        }
        ModsAction::Uninstall { mod_id } => {
            let confirmed = confirm(source, &format!("uninstall mod '{mod_id}' of {} (its files, and config values it changed go back)", g.name))?;
            let v: serde_json::Value = call(ModsRequest::Uninstall { product_id, mod_id, confirmed }).await?;
            let l: Layer = serde_json::from_value(v["mod"].clone())?;
            let undone: Vec<PatchStatus> = serde_json::from_value(v["undone"].clone()).unwrap_or_default();
            println!("{}: uninstalled '{}' ({})", g.name, l.name, l.id);
            print_patches(&undone, &|id| id.clone());
        }
        ModsAction::Show { mod_id, json } => {
            let d: ModDetail = client.call(Request::Mods(ModsRequest::Show { product_id, mod_id })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print_detail(&d);
            }
        }
        ModsAction::Set { mod_id, changes, message } => {
            let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
            let c: LayerChange = client.call(Request::Mods(ModsRequest::ModSet { product_id, mod_id, ops, message })).await?;
            print_change(&g.name, "changed", &c);
            println!("  {}", mod_flags(&c.layer));
        }
        ModsAction::Plan { profile, json } => {
            let plan: LaunchPlan = client.call(Request::Mods(ModsRequest::Plan { product_id, profile })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                let state: ModsState = client.call(Request::Mods(ModsRequest::List { product_id })).await?;
                print_plan(&g.name, &plan, &state.layers);
            }
        }
        ModsAction::UndoPatches { mod_id } => {
            let what = match &mod_id {
                Some(m) => format!("put back the config values mod '{m}' of {} changed", g.name),
                None => format!("put back every config value Quiver changed for {}", g.name),
            };
            let confirmed = confirm(source, &what)?;
            let undone: Vec<PatchStatus> = client.call(Request::Mods(ModsRequest::PatchesUndo { product_id, mod_id, confirmed })).await?;
            if undone.is_empty() {
                println!("{}: nothing to undo", g.name);
            }
            print_patches(&undone, &|id| id.clone());
        }
        ModsAction::Migrate { dry_run } => {
            let r: MigrationReport = client.call(Request::Mods(ModsRequest::Migrate { product_id, dry_run })).await?;
            println!("{}: {}", g.name, if dry_run { "would migrate (dry run)" } else { "migrated" });
            println!("  file-only mods (no change needed): {}", if r.file_only_mods.is_empty() { "none".into() } else { r.file_only_mods.join(", ") });
            if r.kept_separate_saves.is_empty() {
                println!("  no profile needed separate_saves set");
            }
            for n in &r.notes {
                println!("  {n}");
            }
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
