//! Keeping Ubisoft Connect out of an instance, and the instance's saves out of the cloud.
//!
//! Ubisoft's "keep me logged in" tokens are single use, with reuse detection that revokes
//! the whole token family, live sessions included. So a token must never exist in a copy of
//! a prefix, and Connect must never start in one (it would log in, or present a copied
//! token): there is one live login per account, and quiverd holds it. This is made
//! impossible rather than discouraged:
//!
//! 1. the gold copy and every instance lose Connect's credentials ([`strip_credentials`]);
//! 2. Connect's executables are renamed away ([`disable_connect`]), checked again before
//!    every launch ([`enforce`]);
//! 3. every launch disables them in `WINEDLLOVERRIDES` too
//!    (`steamrun::dll_overrides_without_connect`);
//! 4. the supervisor kills a run in which a Connect process appears (`procs::connect_in`).
//!
//! Saves: the bridge never uploads; Connect's own `syncsavegames` is forced off in case a
//! settings file is there ([`harden_settings`]).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::steamrun::CONNECT_EXES;

/// Connect's credential files (per Wine user).
pub const CREDENTIAL_FILES: &[&str] = &["ConnectSecureStorage.dat", "user.vdat"];

/// What a disabled Connect executable is renamed to (`upc.exe.quiver-lab-off`).
pub const DISABLED_SUFFIX: &str = ".quiver-lab-off";

/// Connect's per-user data folders in a prefix (`drive_c/users/*/AppData/Local/Ubisoft Game Launcher`).
fn connect_appdata(prefix: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(prefix.join("drive_c/users"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|u| u.path().join("AppData/Local/Ubisoft Game Launcher"))
        .filter(|p| p.is_dir())
        .collect()
}

/// Folders Connect's executables may be in.
fn connect_dirs(prefix: &Path) -> Vec<PathBuf> {
    ["drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher", "drive_c/Program Files/Ubisoft/Ubisoft Game Launcher"]
        .iter()
        .map(|d| prefix.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

/// Delete Connect's stored login. Returns what was deleted.
pub fn strip_credentials(prefix: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in connect_appdata(prefix) {
        for f in CREDENTIAL_FILES {
            let p = dir.join(f);
            if p.symlink_metadata().is_ok() {
                std::fs::remove_file(&p)?;
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// Rename Connect's executables away (case-insensitively, as Windows finds them).
pub fn disable_connect(prefix: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in connect_dirs(prefix) {
        for e in std::fs::read_dir(&dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if CONNECT_EXES.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                let to = dir.join(format!("{name}{DISABLED_SUFFIX}"));
                std::fs::rename(e.path(), &to)?;
                out.push(e.path());
            }
        }
    }
    Ok(out)
}

/// Force Connect's `syncsavegames` off in its settings files, if there are any.
pub fn harden_settings(prefix: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in connect_appdata(prefix) {
        let f = dir.join("settings.yaml");
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let fixed = force_no_sync(&text);
        if fixed != text {
            std::fs::write(&f, &fixed)?;
            out.push(f);
        }
    }
    Ok(out)
}

/// `syncsavegames: false` on every line that sets it (keeping indentation).
fn force_no_sync(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("syncsavegames:")
            && rest.trim() != "false"
        {
            let indent = &line[..line.len() - trimmed.len()];
            out.push(format!("{indent}syncsavegames: false"));
        } else {
            out.push(line.to_owned());
        }
    }
    out.join("\n")
}

/// What is wrong with a prefix, if anything (empty: safe to launch in).
pub fn check(prefix: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    for dir in connect_dirs(prefix) {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if CONNECT_EXES.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                bad.push(format!("Connect executable {} is runnable", e.path().display()));
            }
        }
    }
    for dir in connect_appdata(prefix) {
        for f in CREDENTIAL_FILES {
            if dir.join(f).symlink_metadata().is_ok() {
                bad.push(format!("Connect credentials {} are present", dir.join(f).display()));
            }
        }
        if let Ok(t) = std::fs::read_to_string(dir.join("settings.yaml"))
            && t.lines().any(|l| l.trim_start().strip_prefix("syncsavegames:").is_some_and(|v| v.trim() != "false"))
        {
            bad.push(format!("Connect's save sync is on in {}", dir.join("settings.yaml").display()));
        }
    }
    bad
}

/// Make a prefix safe (all of the above), then check it; an error if anything is left.
pub fn enforce(prefix: &Path) -> Result<Vec<String>> {
    let mut did = Vec::new();
    did.extend(strip_credentials(prefix)?.into_iter().map(|p| format!("deleted {}", p.display())));
    did.extend(disable_connect(prefix)?.into_iter().map(|p| format!("disabled {}", p.display())));
    did.extend(harden_settings(prefix)?.into_iter().map(|p| format!("save sync off in {}", p.display())));
    let left = check(prefix);
    if !left.is_empty() {
        bail!("the instance's prefix isn't safe to launch in: {}", left.join("; "));
    }
    Ok(did)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_prefix_is_made_safe() {
        let t = tempfile::tempdir().unwrap();
        let pfx = t.path();
        let upc = pfx.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher");
        let app = pfx.join("drive_c/users/steamuser/AppData/Local/Ubisoft Game Launcher");
        std::fs::create_dir_all(&upc).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        for f in ["upc.exe", "UbisoftConnect.exe", "uplay_r164.dll"] {
            std::fs::write(upc.join(f), b"").unwrap();
        }
        std::fs::write(app.join("ConnectSecureStorage.dat"), b"token").unwrap();
        std::fs::write(app.join("settings.yaml"), "overlay:\n  enabled: true\nmisc:\n  syncsavegames: true\n").unwrap();
        assert_eq!(check(pfx).len(), 4);
        let did = enforce(pfx).unwrap();
        assert_eq!(did.len(), 4, "{did:?}");
        assert!(check(pfx).is_empty());
        assert!(upc.join("upc.exe.quiver-lab-off").is_file() && upc.join("uplay_r164.dll").is_file());
        assert!(!app.join("ConnectSecureStorage.dat").exists());
        assert!(std::fs::read_to_string(app.join("settings.yaml")).unwrap().contains("  syncsavegames: false\n"));
        // Idempotent.
        assert!(enforce(pfx).unwrap().is_empty());
    }

    #[test]
    fn a_quiver_prefix_has_nothing_to_do() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher")).unwrap();
        assert!(enforce(t.path()).unwrap().is_empty());
    }
}
