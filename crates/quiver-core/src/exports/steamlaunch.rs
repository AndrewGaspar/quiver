//! Starting a game Steam installed, from outside Steam: `steam steam://rungameid/<appid>`.
//!
//! With Steam running, the `steam` command hands the URL to it and exits. Without, it starts
//! Steam, which then starts the game. Started by quiverd (a systemd user service), Steam
//! would land in quiverd's cgroup and be killed whenever quiverd stops or restarts, so then
//! it runs in a scope of its own (`systemd-run --user --scope`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::steam::Steam;

const FLATPAK_APP: &str = "com.valvesoftware.Steam";

pub fn rungameid_url(app_id: u32) -> String {
    format!("steam://rungameid/{app_id}")
}

/// Steam's Downloads page, where its update queue is (start, pause, reorder). Steam has
/// no URL that starts one game's update, so Quiver opens this one for "Update in Steam".
pub const DOWNLOADS_URL: &str = "steam://open/downloads";

/// How to reach the Steam client: `flatpak run …`, `steam` on `PATH`, or `steam.sh`.
fn steam_argv(steam: &Steam, steam_on_path: Option<&Path>) -> Vec<String> {
    if steam.flatpak {
        vec!["flatpak".into(), "run".into(), FLATPAK_APP.into()]
    } else if let Some(p) = steam_on_path {
        vec![p.to_string_lossy().into_owned()]
    } else {
        vec![steam.root.join("steam.sh").to_string_lossy().into_owned()]
    }
}

/// The command line that starts `app_id`. `own_scope`: Steam isn't running and the caller
/// lives in a systemd service, so a newly started Steam gets its own scope.
pub fn launch_argv(steam: &Steam, app_id: u32, steam_on_path: Option<&Path>, own_scope: Option<&Path>) -> Vec<String> {
    url_argv(steam, &rungameid_url(app_id), steam_on_path, own_scope)
}

/// The command line that hands `url` (a `steam://` URL) to Steam.
pub fn url_argv(steam: &Steam, url: &str, steam_on_path: Option<&Path>, own_scope: Option<&Path>) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(systemd_run) = own_scope {
        argv.extend([systemd_run.to_string_lossy().into_owned(), "--user".into(), "--scope".into(), "--collect".into(), "--quiet".into(), "--".into()]);
    }
    argv.extend(steam_argv(steam, steam_on_path));
    argv.push(url.to_owned());
    argv
}

/// Ask Steam to start `app_id`. Returns the command line used. The process is reaped on a
/// thread of its own; its exit status isn't waited for.
pub fn launch(steam: &Steam, app_id: u32) -> Result<Vec<String>> {
    open(steam, &rungameid_url(app_id))
}

/// Hand a `steam://` URL to Steam (starting it, in a scope of its own, when it isn't
/// running). Never restarts or stops Steam, and writes none of its files.
pub fn open(steam: &Steam, url: &str) -> Result<Vec<String>> {
    let in_service = std::env::var_os("INVOCATION_ID").is_some();
    let systemd_run: Option<PathBuf> = (in_service && !super::steam::is_running()).then(|| super::find_in_path("systemd-run")).flatten();
    let argv = url_argv(steam, url, super::find_in_path("steam").as_deref(), systemd_run.as_deref());
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", argv[0]))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv() {
        let native = Steam { root: "/h/.local/share/Steam".into(), flatpak: false };
        assert_eq!(
            launch_argv(&native, 3751950, Some(Path::new("/usr/bin/steam")), None),
            ["/usr/bin/steam", "steam://rungameid/3751950"]
        );
        assert_eq!(launch_argv(&native, 1, None, None), ["/h/.local/share/Steam/steam.sh", "steam://rungameid/1"]);
        assert_eq!(
            launch_argv(&native, 1, Some(Path::new("/usr/bin/steam")), Some(Path::new("/usr/bin/systemd-run"))),
            ["/usr/bin/systemd-run", "--user", "--scope", "--collect", "--quiet", "--", "/usr/bin/steam", "steam://rungameid/1"]
        );
        let flatpak = Steam { root: "/h/.var/app/com.valvesoftware.Steam/data/Steam".into(), flatpak: true };
        assert_eq!(
            launch_argv(&flatpak, 2, Some(Path::new("/usr/bin/steam")), None),
            ["flatpak", "run", "com.valvesoftware.Steam", "steam://rungameid/2"]
        );
    }
}
