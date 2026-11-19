//! `quiver lab create`: a game's gold copy (once) and an instance cloned from it.
//!
//! The gold copy is a reflink clone of the game's prefix and install as the person plays
//! it, with Connect's login and executables taken out (`lab::guard`). Instances are
//! reflink clones of the gold copy: free on btrfs, and nothing ever points back at the
//! person's own folders.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use quiver_api::{Client, GameRef, LibraryEntry, Request, StoreId};
use quiver_core::lab::manifest::{Display, GamePaths, Licence, Runtime, layer_versions};
use quiver_core::lab::{self, Inst, Lab, Manifest, facts, guard, procs};

use super::sup::licence_for;

/// Where a game is, as the lab sees it.
pub struct Source {
    pub game: GameRef,
    pub title: String,
    pub install: PathBuf,
    /// The person's prefix (Ubisoft, Epic), or none (a fresh one is made: Steam).
    pub prefix: Option<PathBuf>,
    pub exe: String,
    pub args: Vec<String>,
    pub proton: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub dll_overrides: std::collections::BTreeMap<String, String>,
}

/// Find an installed game through quiverd's library.
pub async fn source(client: &Client, game: &str, exe: Option<&str>, need_exe: bool) -> Result<Source> {
    let e: LibraryEntry = crate::resolve_any(client, game).await?;
    let settings: serde_json::Value = client.call(Request::SettingsGet { game: e.game.into() }).await.unwrap_or_default();
    let s = &settings["settings"];
    let map = |v: &serde_json::Value| -> std::collections::BTreeMap<String, String> {
        v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()
    };
    let proton = s["proton"].as_str().map(str::to_owned);
    let (env, dll_overrides) = (map(&s["env"]), map(&s["dll_overrides"]));
    let args: Vec<String> = s["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect();
    if let Some(g) = &e.installed {
        let mut a: Vec<String> = g.args.as_deref().map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
        a.extend(args);
        return Ok(Source {
            game: e.game,
            title: e.name.clone(),
            install: g.install_dir.clone(),
            prefix: g.prefix().map(Path::to_path_buf),
            exe: exe.map(str::to_owned).unwrap_or_else(|| g.exe.clone()),
            args: a,
            proton,
            env,
            dll_overrides,
        });
    }
    if let Some(c) = e.client.as_ref().filter(|_| e.game.store == StoreId::Steam) {
        let exe = match exe {
            Some(e) => e,
            None if !need_exe => "",
            None => bail!("Steam doesn't say which executable to run outside Steam: give it with --exe <path relative to the install>"),
        };
        return Ok(Source { game: e.game, title: e.name.clone(), install: c.dir.clone(), prefix: None, exe: exe.to_owned(), args, proton, env, dll_overrides });
    }
    bail!("{} isn't installed (quiver install {})", e.name, e.game)
}

/// The gold copy's paths.
fn gold_paths(lab: &Lab, game: &GameRef) -> (PathBuf, PathBuf, PathBuf) {
    let g = lab.gold(&game.dir_name());
    (g.join("prefix"), g.join("install"), g)
}

/// A path the container mounts a Steam game's fresh prefix at.
pub const FRESH_PREFIX: &str = "/quiver-lab/prefix";

/// Make the gold copy, if there isn't one (or `again`).
pub fn ensure_gold(lab: &Lab, src: &Source, again: bool) -> Result<Vec<String>> {
    let (gp, gi, gdir) = gold_paths(lab, &src.game);
    let mut notes = Vec::new();
    if gdir.join("gold.json").is_file() && !again {
        return Ok(notes);
    }
    if again && gdir.exists() {
        let old = gdir.with_extension(format!("old-{}", Utc::now().timestamp()));
        std::fs::rename(&gdir, &old)?;
        notes.push(format!("previous gold copy kept at {}", old.display()));
    }
    std::fs::create_dir_all(&gdir)?;
    match &src.prefix {
        Some(p) => {
            // A consistent snapshot: nothing may run in the person's prefix meanwhile.
            // Proton sets WINEPREFIX to <compat>/pfx/, which Quiver links to the prefix itself.
            let norm = |s: &str| s.trim_end_matches('/').trim_end_matches("/pfx").trim_end_matches('/').to_owned();
            let p_s = norm(&p.display().to_string());
            let busy: Vec<String> = procs::scan()
                .into_iter()
                .filter(|q| q.container.is_none() && q.wineprefix.as_deref().is_some_and(|w| norm(w) == p_s))
                .map(|q| format!("{} (pid {})", q.program(), q.pid))
                .collect();
            if !busy.is_empty() {
                bail!("the game's prefix is in use ({}); make the gold copy once the game is closed", busy.join(", "));
            }
            lab::clone_tree(p, &gp).context("clone the game's prefix")?;
            let _ = std::fs::remove_file(gp.join("pfx.lock"));
            notes.push(format!("gold prefix cloned from {}", p.display()));
        }
        None => {
            std::fs::create_dir_all(&gp)?;
            std::os::unix::fs::symlink(".", gp.join("pfx"))?;
            notes.push("gold prefix: fresh (Proton sets it up at the first launch)".into());
        }
    }
    if !src.prefix.as_ref().is_some_and(|p| src.install.starts_with(p)) {
        lab::clone_tree(&src.install, &gi).context("clone the game's install")?;
        notes.push(format!("gold install cloned from {}", src.install.display()));
    }
    notes.extend(guard::enforce(&gp)?);
    let meta = serde_json::json!({ "game": src.game, "title": src.title, "prefix": src.prefix, "install": src.install, "made": Utc::now() });
    lab::write_atomic(&gdir.join("gold.json"), &serde_json::to_vec_pretty(&meta)?)?;
    Ok(notes)
}

pub struct CreateOpts {
    pub name: String,
    pub display: Display,
    pub proton: Option<String>,
    pub mod_dir: Option<PathBuf>,
    pub graphics: Option<String>,
    pub created_by: String,
    pub regold: bool,
}

/// Make an instance of `src` named `o.name`.
pub fn create(lab: &Lab, src: &Source, o: &CreateOpts) -> Result<(Inst, Vec<String>)> {
    lab::check_name(&o.name)?;
    let inst = lab.instance(&o.name);
    if inst.dir.exists() {
        bail!("instance '{}' exists (quiver lab rm {} first)", o.name, o.name);
    }
    let f = facts::gather(&src.install, &src.exe, None);
    let licence = licence_for(src.game.store, f.steam.drm_free()).map_err(|why| anyhow::anyhow!("{} can't run in the lab: {why}", src.title))?;
    if let Some(ac) = &f.anticheat {
        // Not refused: single-player work offline is fine, but say so.
        eprintln!("note: {} ships an anti-cheat ({ac}); keep the instance's work offline and single-player", src.title);
    }
    let proton_name = o.proton.clone().or(src.proton.clone());
    let proton_path = match &proton_name {
        Some(n) => quiver_core::launcher::resolve_proton(n)?,
        None => quiver_core::launcher::Config::load()?.proton,
    };
    let (dxvk, vkd3d, nvapi) = layer_versions(&proton_path);
    let mut notes = ensure_gold(lab, src, o.regold)?;
    let (gp, gi, _) = gold_paths(lab, &src.game);
    std::fs::create_dir_all(&inst.dir)?;
    let made = (|| -> Result<()> {
        lab::clone_tree(&gp, &inst.prefix())?;
        if gi.exists() {
            lab::clone_tree(&gi, &inst.install())?;
        }
        notes.extend(guard::enforce(&inst.prefix())?);
        for d in [inst.lab(), inst.runs(), inst.shots(), inst.lab().join("req"), inst.lab().join("rsp"), inst.originals()] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&inst.dir);
        return Err(e);
    }
    let prefix_path = src.prefix.clone().unwrap_or_else(|| PathBuf::from(FRESH_PREFIX));
    let saves = default_saves(src);
    let m = Manifest {
        name: o.name.clone(),
        game: src.game,
        title: src.title.clone(),
        licence,
        created: Utc::now(),
        created_by: o.created_by.clone(),
        display: o.display.clone(),
        runtime: Runtime {
            proton: proton_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            proton_path,
            dxvk,
            vkd3d,
            nvapi,
            image: lab::IMAGE.into(),
        },
        paths: GamePaths { install: src.install.clone(), prefix: prefix_path, exe: src.exe.clone(), args: src.args.clone() },
        // Quiver's environment for every game (PROTON_ENABLE_NVAPI, …), then the game's own.
        env: quiver_core::launcher::Config::load().map(|c| c.env).unwrap_or_default().into_iter().chain(src.env.clone()).collect(),
        dll_overrides: src.dll_overrides.clone(),
        mod_dir: o.mod_dir.clone(),
        graphics: o.graphics.clone(),
        fixture: None,
        saves,
        files: Default::default(),
        throttle: Vec::new(),
        reload_pattern: None,
        log_file: None,
        notes: String::new(),
    };
    m.save(&inst.manifest_path())?;
    Ok((inst, notes))
}

/// The save folders fixtures copy, when the store says where they are.
fn default_saves(src: &Source) -> Vec<String> {
    match src.game.as_ubisoft() {
        // Each account's folder for the product: `savegames/<user>/<product>`.
        Some(id) => vec![format!("{}/savegames/*/{id}", quiver_core::launcher::UPC_REL)],
        None => Vec::new(),
    }
}

/// Expand `*` in a prefix-relative save pattern against the instance's prefix.
pub fn expand_saves(prefix: &Path, pattern: &str) -> Vec<String> {
    let mut found = vec![String::new()];
    for part in pattern.split('/') {
        let mut next = Vec::new();
        for base in &found {
            if part == "*" {
                for e in std::fs::read_dir(prefix.join(base)).into_iter().flatten().flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        next.push(join(base, &e.file_name().to_string_lossy()));
                    }
                }
            } else {
                next.push(join(base, part));
            }
        }
        found = next;
    }
    found.into_iter().filter(|r| prefix.join(r).is_dir()).collect()
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() { b.to_owned() } else { format!("{a}/{b}") }
}

/// Licence the brief describes (`Err`: why the lab won't run it).
pub fn brief_licence(store: StoreId, install: &Path, exe: &str) -> std::result::Result<Licence, String> {
    licence_for(store, facts::gather(install, exe, None).steam.drm_free())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_patterns() {
        let t = tempfile::tempdir().unwrap();
        for d in ["s/u1/3539", "s/u2/3539", "s/u2/420"] {
            std::fs::create_dir_all(t.path().join(d)).unwrap();
        }
        let mut v = expand_saves(t.path(), "s/*/3539");
        v.sort();
        assert_eq!(v, ["s/u1/3539", "s/u2/3539"]);
        assert!(expand_saves(t.path(), "nope/*").is_empty());
    }
}
