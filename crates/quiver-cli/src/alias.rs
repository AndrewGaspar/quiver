//! `ubi`: the pre-Quiver name of the CLI. Runs `quiver` (next to this executable, else from
//! PATH) with the same arguments; `UBI_SOURCE` is passed on as `QUIVER_SOURCE`.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;

fn main() {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quiver")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("quiver"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(std::env::args_os().skip(1));
    if let (Some(src), None) = (std::env::var_os("UBI_SOURCE"), std::env::var_os("QUIVER_SOURCE")) {
        cmd.env("QUIVER_SOURCE", src);
    }
    let err = cmd.exec();
    eprintln!("ubi: could not run {}: {err}", exe.display());
    std::process::exit(127);
}
