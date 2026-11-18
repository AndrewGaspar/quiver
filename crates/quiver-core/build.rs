//! Compiles in Quiver's built-in mod catalog: every `mods/<game>/<mod id>/quiver-mod.toml`
//! at the repository root becomes an entry of `CATALOG` (see `src/modpkg.rs`). `<game>` is a
//! game's data folder name: a Ubisoft product id (`3539`) or `<store>-<id>` (`steam-292030`).

use std::path::PathBuf;

/// A game folder name: digits (a Ubisoft product), or a lowercase store name, `-` and an id.
fn game_dir(s: &str) -> bool {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    match s.split_once('-') {
        Some((store, id)) => {
            !store.is_empty() && store.bytes().all(|b| b.is_ascii_lowercase()) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        }
        None => false,
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../mods");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut entries = Vec::new();
    if let Ok(products) = std::fs::read_dir(&root) {
        for p in products.flatten() {
            let Some(product) = p.file_name().to_str().filter(|s| game_dir(s)).map(str::to_owned) else { continue };
            for m in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
                let manifest = m.path().join("quiver-mod.toml");
                if manifest.is_file() {
                    let id = m.file_name().to_string_lossy().into_owned();
                    entries.push((product.clone(), id, manifest.canonicalize().unwrap()));
                }
            }
        }
    }
    entries.sort();
    let mut out = String::from("/// (game folder, mod id, manifest text) of every built-in mod.\npub static CATALOG: &[(&str, &str, &str)] = &[\n");
    for (product, id, path) in entries {
        out.push_str(&format!("    ({product:?}, {id:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("catalog.rs"), out).unwrap();

    // Options catalog (design/MOD-CONFIG.md, source b): every `options/<id>.toml`.
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../options");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut schemas: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .map(|e| (e.path().file_stem().unwrap().to_string_lossy().into_owned(), e.path().canonicalize().unwrap()))
        .collect();
    schemas.sort();
    let mut out = String::from("/// (catalog id, schema text) of every options schema in Quiver's catalog.\npub static OPTIONS_CATALOG: &[(&str, &str)] = &[\n");
    for (id, path) in schemas {
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!("    ({id:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("options_catalog.rs"), out).unwrap();
}
