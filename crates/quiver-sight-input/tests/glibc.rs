//! The preload needs no newer glibc than the Sight layer: both load into every game process,
//! Steam copies' inside pressure-vessel too, so a newer symbol version in the preload alone
//! would keep it (and the panel's input) out of games the layer runs in. Reads the dynamic
//! symbol versions with `objdump -T` (skipped without binutils or without a built layer).

use std::path::Path;
use std::process::Command;

fn newest_glibc(so: &Path) -> Option<(u32, u32)> {
    let out = Command::new("objdump").arg("-T").arg(so).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split(|c: char| c == '(' || c == ')' || c.is_whitespace())
        .filter_map(|w| w.strip_prefix("GLIBC_"))
        .filter_map(|v| {
            let mut it = v.split('.');
            Some((it.next()?.parse().ok()?, it.next().unwrap_or("0").parse().ok()?))
        })
        .max()
}

#[test]
fn no_newer_glibc_than_the_layer() {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let (input, layer) = (dir.join("libquiver_sight_input.so"), dir.join("libquiver_sight_layer.so"));
    if !input.exists() || !layer.exists() {
        eprintln!("skipping: build both first ({} / {})", input.display(), layer.display());
        return;
    }
    let (Some(i), Some(l)) = (newest_glibc(&input), newest_glibc(&layer)) else {
        eprintln!("skipping: no objdump");
        return;
    };
    eprintln!("newest glibc symbol versions: preload {}.{}, layer {}.{}", i.0, i.1, l.0, l.1);
    assert!(i <= l, "the preload needs GLIBC_{}.{}, the layer only {}.{}", i.0, i.1, l.0, l.1);
}
