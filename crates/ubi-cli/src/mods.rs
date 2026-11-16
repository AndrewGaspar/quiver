//! `quiver mods <game> …`: mod layers and profiles. Games are looked up among installed
//! games, so none of this needs a Ubisoft request.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ubi_api::mods::{GameProfiles, Layer, LayerChange, LayerRevision, ModsState, ProfileInfo, ProfileRevision, is_human_source};
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
    /// Mod profiles: named stacks of layers with their own saves.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
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
    /// Change a profile: layers=a,b  cloud_saves=on|off  pinned=on|off  name=…, or a
    /// launch-setting override (proton=…, env.NAME=…, dll.NAME=…, args=…, wrappers=…).
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

fn print_state(name: &str, s: &ModsState) {
    let layer_name = |id: &String| s.layers.iter().find(|l| &l.id == id).map_or(format!("{id} (missing)"), |l| l.name.clone());
    println!("{name} ({})", s.product_id);
    println!("layers:");
    if s.layers.is_empty() {
        println!("  (none; quiver mods {} layer add NAME DIR)", s.product_id);
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
    }
    println!("profiles:");
    println!("  {:<7}  {:<24} the game as installed", ubi_api::mods::VANILLA, "Vanilla");
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
                    serde_json::from_value(call(ModsRequest::ProfileCreate { product_id, name, layers, message }).await?)?
                }
                ProfileAction::Set { profile, changes, message } => {
                    let ops = changes.iter().map(|c| SettingOp::parse(c)).collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::msg)?;
                    serde_json::from_value(call(ModsRequest::ProfileSet { product_id, profile, ops, message }).await?)?
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
                "{}: profile {} '{}' revision {} (layers: {}; cloud saves {}{})",
                g.name,
                p.id,
                p.name,
                rev.revision,
                if names.is_empty() { "none".into() } else { names.join(" < ") },
                if p.cloud_saves { "on" } else { "off" },
                if p.pinned { "; pinned" } else { "" }
            );
        }
    }
    Ok(())
}
