//! Mod packages (Mods v2): reading and checking a package's manifest, and Quiver's
//! built-in catalog.
//!
//! A package is a folder with a manifest, `quiver-mod.toml` (or `quiver-mod.json` with the
//! same fields; see [`quiver_api::mods::ModManifest`]), and optionally a folder of files laid
//! out like the install folder (`files/`, or the manifest's `files.dir`).
//!
//! The **catalog** is every `mods/<game>/<mod id>/quiver-mod.toml` in the repository,
//! compiled in (`build.rs`); `<game>` is the game's data folder name
//! ([`quiver_api::GameRef::dir_name`]): a Ubisoft product id (`mods/3539/`), or a store and
//! id (`mods/steam-292030/`). Built-in mods are recipes (flags, deletions, config patches),
//! each audited when translated from its source (PCGamingWiki and the like), with the link
//! and an `audited` note in the manifest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use quiver_api::GameRef;
use quiver_api::mods::{MANIFEST_FILE, MANIFEST_FILE_JSON, ModKind, ModManifest};

include!(concat!(env!("OUT_DIR"), "/catalog.rs"));

/// Parse a manifest (`json`: the JSON form) and check it.
pub fn parse(text: &str, json: bool) -> Result<ModManifest> {
    let m: ModManifest = if json {
        serde_json::from_str(text).context("bad quiver-mod.json")?
    } else {
        toml::from_str(text).map_err(|e| anyhow::anyhow!("bad quiver-mod.toml: {e}"))?
    };
    validate(&m)?;
    Ok(m)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
}

/// What every manifest must satisfy.
pub fn validate(m: &ModManifest) -> Result<()> {
    if !valid_id(&m.id) {
        bail!("mod id {:?}: use lowercase letters, digits and '-' (at most 64)", m.id);
    }
    if m.name.trim().is_empty() {
        bail!("{}: the mod needs a name", m.id);
    }
    if m.kind == ModKind::Fix && m.save_compatible != Some(true) {
        bail!("{}: a fix must be save-compatible (save_compatible = true); otherwise make it kind = \"mod\"", m.id);
    }
    if let Some(s) = &m.source
        && !(s.starts_with("https://") || s.starts_with("http://"))
    {
        bail!("{}: source should be a link (https://…)", m.id);
    }
    for d in &m.files.delete {
        crate::mods::normalize_path(d, None).with_context(|| format!("{}: files.delete", m.id))?;
    }
    if let Some(dir) = &m.files.dir {
        crate::mods::normalize_path(dir, None).with_context(|| format!("{}: files.dir", m.id))?;
    }
    for (k, v) in m.launch.env.iter().chain(&m.launch.dll) {
        if k.trim().is_empty() || k.contains(['=', '\0']) || v.contains('\0') {
            bail!("{}: bad launch setting {k:?}", m.id);
        }
    }
    if m.launch.wrappers.iter().chain(&m.launch.args).any(|w| w.trim().is_empty()) {
        bail!("{}: empty argument or wrapper", m.id);
    }
    for p in &m.patches {
        crate::confpatch::validate(p).with_context(|| format!("{}: patch", m.id))?;
    }
    for r in m.requires.iter().chain(&m.conflicts) {
        if r.eq_ignore_ascii_case(&m.id) {
            bail!("{}: a mod can't require or conflict with itself", m.id);
        }
    }
    Ok(())
}

/// A package folder: its manifest and its files folder (if it has one). `Ok(None)` for a
/// folder without a manifest (a plain folder of files, as v1 layers were).
pub fn read_package(dir: &Path) -> Result<Option<(ModManifest, Option<PathBuf>)>> {
    let (text, json) = match std::fs::read_to_string(dir.join(MANIFEST_FILE)) {
        Ok(t) => (t, false),
        Err(_) => match std::fs::read_to_string(dir.join(MANIFEST_FILE_JSON)) {
            Ok(t) => (t, true),
            Err(_) => return Ok(None),
        },
    };
    let m = parse(&text, json).with_context(|| dir.display().to_string())?;
    let files = dir.join(m.files.dir.as_deref().unwrap_or("files"));
    if m.files.dir.is_some() && !files.is_dir() {
        bail!("{}: files.dir {} is not a folder", m.id, files.display());
    }
    Ok(Some((m.clone(), files.is_dir().then_some(files))))
}

/// The built-in mods for a game, by id.
pub fn catalog(game: &GameRef) -> Result<Vec<ModManifest>> {
    catalog_in(CATALOG, game)
}

/// [`catalog`] over a table of (folder, mod id, manifest text).
fn catalog_in(table: &[(&str, &str, &str)], game: &GameRef) -> Result<Vec<ModManifest>> {
    let dir = game.dir_name();
    table
        .iter()
        .filter(|(d, _, _)| *d == dir)
        .map(|(d, id, text)| {
            let m = parse(text, false).with_context(|| format!("built-in mod mods/{d}/{id}"))?;
            if m.id != *id {
                bail!("built-in mod mods/{d}/{id} has id {}", m.id);
            }
            check_game(&m, game).with_context(|| format!("built-in mod mods/{d}/{id}"))?;
            Ok(m)
        })
        .collect()
}

/// Whether a manifest is for `game`: a Ubisoft product it names in `products`, or a game it
/// names in `games`. A manifest that names neither isn't checked.
pub fn check_game(m: &ModManifest, game: &GameRef) -> Result<()> {
    if m.products.is_empty() && m.games.is_empty() {
        return Ok(());
    }
    let ok = match game.as_ubisoft() {
        Some(p) => m.products.contains(&p) || m.games.contains(game),
        None => m.games.contains(game),
    };
    if !ok {
        let mut names: Vec<String> = m.products.iter().map(|p| format!("product {p}")).collect();
        names.extend(m.games.iter().map(|g| g.to_string()));
        bail!("{} is for {}, not {game}", m.id, names.join(", "));
    }
    Ok(())
}

/// A built-in mod for a game, by id.
pub fn builtin(game: &GameRef, id: &str) -> Result<ModManifest> {
    let all = catalog(game)?;
    let found = all.iter().find(|m| m.id.eq_ignore_ascii_case(id.trim())).cloned();
    found.with_context(|| {
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
        format!("no built-in mod '{id}' for this game (built-in: {})", if ids.is_empty() { "none".into() } else { ids.join(", ") })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_parse_and_validate() {
        let m = parse(
            r#"
id = "x-mod"
name = "X"
kind = "fix"
save_compatible = true
source = "https://example.org/x"
requires = ["y"]
[launch]
args = ["-a"]
env = { A = "1" }
dll = { d3d11 = "n,b" }
wrappers = ["gamemoderun"]
[files]
delete = ["videos\\intro.bik"]
[[patch]]
file = "drive_c/a.ini"
section = "S"
set = { K = "1" }
delete = ["J"]
[[patch]]
file = "drive_c/b.xml"
format = "xml"
element = "Root/Child"
set = { a = "b" }
"#,
            false,
        )
        .unwrap();
        assert_eq!((m.kind, m.save_compatible), (ModKind::Fix, Some(true)));
        assert_eq!(m.launch.dll["d3d11"], "n,b");
        assert_eq!(m.patches.len(), 2);
        assert_eq!(m.patches[1].target(), "Root/Child");
        // The JSON form is the same.
        let back = parse(&serde_json::to_string(&m).unwrap(), true).unwrap();
        assert_eq!(back, m);
        let bad = |s: &str| parse(s, false).unwrap_err().to_string();
        assert!(bad(r#"id = "X"
name = "n""#).contains("lowercase"));
        assert!(bad(r#"id = "a"
name = "n"
kind = "fix""#).contains("save-compatible"));
        assert!(bad(r#"id = "a"
name = "n"
colour = "red""#).contains("colour"), "unknown fields are refused");
        assert!(bad(r#"id = "a"
name = "n"
[files]
delete = ["../x"]"#).contains("files.delete"));
        assert!(format!("{:#}", parse(r#"id = "a"
name = "n"
[[patch]]
file = "/etc/x"
section = "S"
set = { a = "1" }"#, false).unwrap_err()).contains("relative to the prefix"));
        assert!(format!("{:#}", parse(r#"id = "a"
name = "n"
[[patch]]
file = "drive_c/x.xml"
format = "xml"
set = { a = "1" }"#, false).unwrap_err()).contains("element"));
    }

    #[test]
    fn packages() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert!(read_package(d).unwrap().is_none(), "no manifest: a plain folder");
        std::fs::write(d.join(MANIFEST_FILE), "id = \"p\"\nname = \"P\"\n").unwrap();
        let (m, files) = read_package(d).unwrap().unwrap();
        assert_eq!((m.id.as_str(), files), ("p", None));
        std::fs::create_dir(d.join("files")).unwrap();
        assert_eq!(read_package(d).unwrap().unwrap().1, Some(d.join("files")));
    }

    /// Every built-in mod parses, is audited, links its source and names its product.
    #[test]
    fn catalog_loads() {
        assert!(!CATALOG.is_empty());
        let mut seen = std::collections::BTreeSet::new();
        for (dir, id, _) in CATALOG {
            let product: u32 = dir.parse().expect("built-in mods are Ubisoft products so far");
            let m = builtin(&GameRef::ubisoft(product), id).unwrap();
            assert!(m.source.as_deref().is_some_and(|s| s.starts_with("https://www.pcgamingwiki.com/")), "{id}: source");
            assert!(m.audited.as_deref().is_some_and(|a| a.len() > 20), "{id}: audited note");
            assert_eq!(m.products, [product], "{id}: products");
            assert!(m.save_compatible.is_some(), "{id}: says whether it is save-compatible");
            assert!(seen.insert(m.id.clone()), "{id}: unique");
        }
        // The recipes translated from the hand-made edits.
        let fc4 = catalog(&GameRef::ubisoft(420)).unwrap();
        let skip = fc4.iter().find(|m| m.id == "fc4-skip-intro").unwrap();
        assert_eq!(skip.launch.args, ["-skipintro"]);
        let mouse = fc4.iter().find(|m| m.id == "fc4-raw-mouse").unwrap();
        assert_eq!(mouse.patches[0].set.len(), 3);
        let china = catalog(&GameRef::ubisoft(1651)).unwrap();
        assert_eq!(china.iter().find(|m| m.id == "acc-china-skip-intro").unwrap().patches[0].delete, ["StartupMovies"]);
        let af = china.iter().find(|m| m.id == "acc-china-anisotropy-16").unwrap();
        assert_eq!(af.kind, ModKind::Mod, "the quality mod is optional, not a fix");
        assert_eq!(catalog(&GameRef::ubisoft(1847)).unwrap().len(), 1);
        assert_eq!(builtin(&GameRef::ubisoft(3539), "ACO-skip-intros").unwrap().files.delete.len(), 4);
        assert!(builtin(&GameRef::ubisoft(3539), "nope").unwrap_err().to_string().contains("aco-skip-intros"));
        assert!(catalog(&GameRef::ubisoft(999_999)).unwrap().is_empty());
        assert!(catalog(&GameRef::steam(3539)).unwrap().is_empty(), "a Steam app with a Ubisoft product's number has none of its mods");
    }

    /// Entries keyed by store id (`mods/steam-<appid>/<mod>/`) next to product ones.
    #[test]
    fn catalog_by_store_id() {
        let steam = "id = \"w3-x\"\nname = \"X\"\nkind = \"fix\"\nsave_compatible = true\ngames = [\"steam:292030\"]\n[launch]\nargs = [\"-x\"]\n";
        let ubi = "id = \"u\"\nname = \"U\"\nproducts = [3539]\n";
        let table = [("steam-292030", "w3-x", steam), ("3539", "u", ubi)];
        let w3 = catalog_in(&table, &GameRef::steam(292030)).unwrap();
        assert_eq!((w3.len(), w3[0].games.as_slice()), (1, &[GameRef::steam(292030)][..]));
        assert_eq!(catalog_in(&table, &GameRef::ubisoft(3539)).unwrap()[0].id, "u");
        assert!(catalog_in(&table, &GameRef::steam(1)).unwrap().is_empty());
        // A manifest in the wrong folder fails loudly.
        let wrong = [("steam-1", "w3-x", steam)];
        assert!(format!("{:#}", catalog_in(&wrong, &GameRef::steam(1)).unwrap_err()).contains("not steam:1"));
        // Installing checks the same.
        let m = parse(steam, false).unwrap();
        assert!(check_game(&m, &GameRef::steam(292030)).is_ok());
        assert!(check_game(&m, &GameRef::ubisoft(292030)).is_err());
        let u = parse(ubi, false).unwrap();
        assert!(check_game(&u, &GameRef::steam(3539)).is_err() && check_game(&u, &GameRef::ubisoft(3539)).is_ok());
        assert!(check_game(&parse("id = \"a\"\nname = \"A\"\n", false).unwrap(), &GameRef::steam(5)).is_ok());
    }
}
