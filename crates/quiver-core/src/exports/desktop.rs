//! `.desktop` entries: `$XDG_DATA_HOME/applications/quiver-<product>[-<profile>].desktop`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{ExportKey, ExportKind, ExportRecord, Paths, Target, art, hash, lock, remove_record, upsert_record, write_atomic};

pub fn path(paths: &Paths, key: &ExportKey) -> PathBuf {
    paths.applications().join(format!("{}.desktop", key.file_stem()))
}

/// Escape a string value (Desktop Entry spec, "Possible value types").
fn escape_value(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Quote one `Exec` argument (spec, "The Exec key"): quoted if it has reserved
/// characters, with `"`, `` ` ``, `$` and `\` backslash-escaped inside the quotes; `%`
/// doubled. The whole line is then escaped as a string value.
fn exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    let reserved = |c: char| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c);
    if !arg.is_empty() && !arg.chars().any(reserved) {
        return arg;
    }
    let mut out = String::from('"');
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn render(target: &Target, icon: Option<&Path>) -> String {
    let exec: Vec<String> = std::iter::once(target.quiver_exe.to_string_lossy().into_owned())
        .chain(target.launch_args())
        .map(|a| exec_arg(&a))
        .collect();
    let mut s = String::from("[Desktop Entry]\nType=Application\nVersion=1.5\n");
    s += &format!("Name={}\n", escape_value(&target.name));
    s += "Comment=Play with Quiver\n";
    s += &format!("Exec={}\n", escape_value(&exec.join(" ")));
    s += &format!(
        "Icon={}\n",
        escape_value(&icon.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "applications-games".into()))
    );
    s += "Terminal=false\nCategories=Game;\n";
    if let Some(class) = &target.wm_class {
        s += &format!("StartupWMClass={}\n", escape_value(class));
    }
    s += &format!("X-Quiver-Id={}\n", target.key);
    s
}

/// Write (or rewrite) the entry. The icon is fetched and converted on first use
/// ([`art::export_icon`]); without one (no art, or the CDN unreachable) the entry uses a
/// generic games icon.
pub async fn export(paths: &Paths, target: &Target) -> Result<ExportRecord> {
    let icon = match art::export_icon(&paths.icons(), &paths.art(), &target.art).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("icon for {}: {e:#}", target.key);
            None
        }
    };
    let text = render(target, icon.as_deref());
    let file = path(paths, &target.key);
    let _lock = lock(paths)?;
    write_atomic(&file, text.as_bytes())?;
    let record = ExportRecord {
        key: target.key.to_string(),
        kind: ExportKind::Desktop,
        game: target.key.game,
        profile_id: target.key.profile_id.clone(),
        name: target.name.clone(),
        path: file,
        appid: None,
        steam_user: None,
        written_at: chrono::Utc::now(),
        hash: hash(text.as_bytes()),
        staged: false,
    };
    upsert_record(paths, record.clone())?;
    update_database(&paths.applications());
    Ok(record)
}

/// Delete the entry. Returns whether there was one. The cached icon is kept.
pub fn remove(paths: &Paths, key: &ExportKey) -> Result<bool> {
    let file = path(paths, key);
    let _lock = lock(paths)?;
    let existed = match std::fs::remove_file(&file) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    remove_record(paths, &key.to_string(), ExportKind::Desktop)?;
    if existed {
        update_database(&paths.applications());
    }
    Ok(existed)
}

/// Refresh the desktop database if `update-desktop-database` is installed.
fn update_database(dir: &Path) {
    let Some(tool) = super::find_in_path("update-desktop-database") else { return };
    match std::process::Command::new(tool).arg(dir).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
        Ok(s) if !s.success() => tracing::debug!("update-desktop-database exited with {s}"),
        Err(e) => tracing::debug!("update-desktop-database: {e}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::{ExportState, list};

    fn target(name: &str, icon: Option<&str>) -> Target {
        Target {
            key: ExportKey::game(1651),
            name: name.into(),
            art: quiver_api::Art { icon: icon.map(Into::into), ..Default::default() },
            wm_class: Some("accgame-win32-shipping.exe".into()),
            quiver_exe: "/opt/my games/quiver".into(),
        }
    }

    #[test]
    fn escaping() {
        assert_eq!(exec_arg("play"), "play");
        assert_eq!(exec_arg("/opt/my games/q"), "\"/opt/my games/q\"");
        assert_eq!(exec_arg("a$b\"c"), "\"a\\$b\\\"c\"");
        assert_eq!(exec_arg("100%"), "100%%");
        let t = target("Assassin's Creed® Chronicles China", None);
        let text = render(&t, None);
        assert!(text.contains("Name=Assassin's Creed® Chronicles China\n"), "{text}");
        // Quoted for the space; the line as a string value doubles nothing else here.
        assert!(text.contains("Exec=\"/opt/my games/quiver\" play 1651\n"), "{text}");
        assert!(text.contains("Icon=applications-games\n"));
        assert!(text.contains("StartupWMClass=accgame-win32-shipping.exe\n"));
        assert!(text.contains("X-Quiver-Id=quiver:1651\n"));
        let t = Target { key: ExportKey { game: quiver_api::GameRef::ubisoft(1651), profile_id: Some("p7f3k2q".into()) }, ..t };
        assert!(render(&t, None).contains(" play 1651 --profile-id p7f3k2q\n"));
        assert_eq!(path(&Paths { data_home: "/d".into(), home: "/h".into() }, &t.key), Path::new("/d/applications/quiver-1651-p7f3k2q.desktop"));
    }

    #[tokio::test]
    async fn epic_game_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths { data_home: tmp.path().join("data"), home: tmp.path().join("home") };
        let t = crate::exports::steam::tests::epic_target(&paths);
        let rec = export(&paths, &t).await.unwrap();
        let file = paths.data_home.join("applications/quiver-epic-Hornbill.desktop");
        assert_eq!((rec.path.as_path(), rec.key.as_str(), rec.game), (file.as_path(), "quiver:epic:Hornbill", t.key.game));
        let icon = paths.icons().join(format!("{}-icon.png", art::url_stem("https://cdn1.epicgames.com/offer/fixture/tall")));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!(
                "[Desktop Entry]\nType=Application\nVersion=1.5\nName=The Alto Collection\nComment=Play with Quiver\n\
                 Exec=/home/u/.local/bin/quiver play epic:Hornbill\nIcon={}\nTerminal=false\nCategories=Game;\n\
                 StartupWMClass=alto.exe\nX-Quiver-Id=quiver:epic:Hornbill\n",
                icon.display()
            )
        );
        assert!(icon.is_file());
        assert_eq!(list(&paths)[0].state, ExportState::Ok);
        assert!(remove(&paths, &t.key).unwrap());
        assert!(!file.exists() && list(&paths).is_empty());
    }

    #[tokio::test]
    async fn write_list_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths { data_home: tmp.path().join("data"), home: tmp.path().join("home") };
        // Pre-cache the icon (art.json names) so nothing is fetched.
        let icon = "48edec3d91f23947b02be534159b74d1.ico";
        std::fs::create_dir_all(paths.icons()).unwrap();
        std::fs::write(paths.icons().join(icon), crate::exports::art::tests::fixture_ico()).unwrap();

        let t = target("Assassin's Creed® Chronicles China", Some(icon));
        let rec = export(&paths, &t).await.unwrap();
        let file = paths.data_home.join("applications/quiver-1651.desktop");
        assert_eq!(rec.path, file);
        let text = std::fs::read_to_string(&file).unwrap();
        let png = paths.icons().join("48edec3d91f23947b02be534159b74d1.png");
        assert!(text.contains(&format!("Icon={}\n", png.display())), "{text}");
        assert!(png.is_file());
        assert_eq!(list(&paths)[0].state, ExportState::Ok);

        // Rename rewrites the same file.
        export(&paths, &target("AC Chronicles China", Some(icon))).await.unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("Name=AC Chronicles China\n"));
        assert_eq!(list(&paths).len(), 1);

        std::fs::write(&file, "edited").unwrap();
        assert_eq!(list(&paths)[0].state, ExportState::Changed);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(list(&paths)[0].state, ExportState::Missing);

        export(&paths, &t).await.unwrap();
        assert!(remove(&paths, &t.key).unwrap());
        assert!(!file.exists());
        assert!(list(&paths).is_empty());
        assert!(!remove(&paths, &t.key).unwrap());
    }
}
