//! Compiles in Quiver's built-in mod catalog: every `mods/<product id>/<mod id>/quiver-mod.toml`
//! at the repository root becomes an entry of `CATALOG` (see `src/modpkg.rs`).

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../mods");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut entries = Vec::new();
    if let Ok(products) = std::fs::read_dir(&root) {
        for p in products.flatten() {
            let Some(product) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            for m in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
                let manifest = m.path().join("quiver-mod.toml");
                if manifest.is_file() {
                    let id = m.file_name().to_string_lossy().into_owned();
                    entries.push((product, id, manifest.canonicalize().unwrap()));
                }
            }
        }
    }
    entries.sort();
    let mut out = String::from("/// (product id, mod id, manifest text) of every built-in mod.\npub static CATALOG: &[(u32, &str, &str)] = &[\n");
    for (product, id, path) in entries {
        out.push_str(&format!("    ({product}, {id:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("catalog.rs"), out).unwrap();
}
