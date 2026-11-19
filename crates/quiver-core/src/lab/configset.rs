//! `quiver lab set`: key edits to an instance's config files (generalised from the Pharos
//! harness's `ini-set`). One assignment is `FILE:TARGET=VALUE`:
//!
//! - `FILE` is an alias from the manifest's `[files]`, `install:<rel>`, `lab:<rel>` or a
//!   path relative to the prefix;
//! - `TARGET` for INI files is `[section]key` (any section name, Unreal's
//!   `[/Script/Engine.RendererSettings]` included; `[]key` before any section) or
//!   `section.key` (split at the first dot); for XML files (`.xml`) `<element/path>@attr`;
//! - `VALUE` may hold `{width}`, `{height}` and `{refresh}`, filled in from the manifest,
//!   so a graphics profile follows the instance's display.
//!
//! The files are edited in place, byte for byte except the changed values
//! (`confpatch::edit_file`). A live mod that reloads its file picks the change up; `--wait`
//! waits for the manifest's `reload_pattern` in its log.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use quiver_api::mods::PatchFormat;
use serde::{Deserialize, Serialize};

use super::{Inst, Manifest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assign {
    pub file: String,
    /// INI section or XML element path.
    pub target: String,
    /// INI key or XML attribute.
    pub key: String,
    pub value: String,
}

impl Assign {
    pub fn parse(s: &str) -> Result<Assign> {
        let bad = || format!("'{s}': expected FILE:[section]key=value, FILE:section.key=value or FILE:<path>@attr=value");
        let (file, rest) = s.split_once(':').with_context(bad)?;
        let (file, rest) = if matches!(file, "install" | "lab" | "prefix") {
            // `install:pharos.ini:[reflex]fps=5`
            let (f, r) = rest.split_once(':').with_context(bad)?;
            (format!("{file}:{f}"), r)
        } else {
            (file.to_owned(), rest)
        };
        let (target, key, value) = if let Some(r) = rest.strip_prefix('[') {
            let (sec, r) = r.split_once(']').with_context(bad)?;
            let (k, v) = r.split_once('=').with_context(bad)?;
            (sec.to_owned(), k.to_owned(), v.to_owned())
        } else if let Some(r) = rest.strip_prefix('<') {
            let (path, r) = r.split_once('>').with_context(bad)?;
            let r = r.strip_prefix('@').with_context(bad)?;
            let (k, v) = r.split_once('=').with_context(bad)?;
            (path.to_owned(), k.to_owned(), v.to_owned())
        } else {
            let (lhs, v) = rest.split_once('=').with_context(bad)?;
            let (sec, k) = lhs.split_once('.').with_context(bad)?;
            (sec.to_owned(), k.to_owned(), v.to_owned())
        };
        if file.is_empty() || key.trim().is_empty() || value.contains('\n') {
            bail!(bad());
        }
        Ok(Assign { file, target, key: key.trim().to_owned(), value })
    }

    pub fn format(&self, m: &Manifest) -> PatchFormat {
        let spec = m.files.get(&self.file).cloned().unwrap_or_else(|| self.file.clone());
        if spec.to_ascii_lowercase().ends_with(".xml") { PatchFormat::Xml } else { PatchFormat::Ini }
    }

    /// The value with `{width}`, `{height}`, `{refresh}` filled in.
    pub fn value_for(&self, m: &Manifest) -> String {
        self.value
            .replace("{width}", &m.display.width.to_string())
            .replace("{height}", &m.display.height.to_string())
            .replace("{refresh}", &m.display.refresh.to_string())
    }
}

/// One edit as done: the file, the old values (empty: wasn't set) and the new one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub path: PathBuf,
    pub file: String,
    pub target: String,
    pub key: String,
    pub old: Vec<String>,
    pub new: String,
}

impl Done {
    /// The assignment that undoes it (the first old value; none means it stays set, which
    /// `restore` reports).
    pub fn undo(&self) -> Option<Assign> {
        self.old.first().map(|v| Assign { file: self.file.clone(), target: self.target.clone(), key: self.key.clone(), value: v.clone() })
    }
}

/// Apply assignments to the instance's files, in order. Files must exist (a game writes
/// its config at first run; a missing file is an error, not a new file).
pub fn apply(inst: &Inst, m: &Manifest, assigns: &[Assign]) -> Result<Vec<Done>> {
    let mut out = Vec::new();
    for a in assigns {
        let path = m.resolve_file(inst, &a.file)?;
        if !path.is_file() {
            bail!("{} doesn't exist (has the game run once in this instance?)", path.display());
        }
        let value = a.value_for(m);
        let old = crate::confpatch::edit_file(&path, a.format(m), |doc| {
            let old = doc.get(&a.target, &a.key)?;
            let anchor = doc.anchor(&a.target, &a.key)?;
            doc.set(&a.target, &a.key, std::slice::from_ref(&value), anchor.as_ref())?;
            Ok(old)
        })?;
        out.push(Done { path, file: a.file.clone(), target: a.target.clone(), key: a.key.clone(), old, new: value });
    }
    Ok(out)
}

/// Read lines of assignments (a graphics profile, `<name>.set`): one per line, `#`
/// comments and blank lines skipped.
pub fn parse_lines(text: &str) -> Result<Vec<Assign>> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(Assign::parse)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let a = Assign::parse("pharos:[reflex]fps_limit=5").unwrap();
        assert_eq!((a.file.as_str(), a.target.as_str(), a.key.as_str(), a.value.as_str()), ("pharos", "reflex", "fps_limit", "5"));
        let a = Assign::parse("pharos:reflex.fps_limit=5").unwrap();
        assert_eq!((a.target.as_str(), a.key.as_str()), ("reflex", "fps_limit"));
        let a = Assign::parse("engine:[/Script/Engine.RendererSettings]r.DefaultFeature.AntiAliasing=2").unwrap();
        assert_eq!((a.target.as_str(), a.key.as_str(), a.value.as_str()), ("/Script/Engine.RendererSettings", "r.DefaultFeature.AntiAliasing", "2"));
        let a = Assign::parse("install:pharos.ini:[fg]show=real").unwrap();
        assert_eq!(a.file, "install:pharos.ini");
        let a = Assign::parse("gp:<GamerProfile/ProfileSpecificGameProfile>@MouseAcceleration=0").unwrap();
        assert_eq!((a.target.as_str(), a.key.as_str()), ("GamerProfile/ProfileSpecificGameProfile", "MouseAcceleration"));
        assert!(Assign::parse("nofile=1").is_err() && Assign::parse("f:[s]=1").is_err() && Assign::parse("f:nodot=1").is_err());
        assert_eq!(parse_lines("# ultra\n\naco:[Graphics]FullscreenWidth={width}\n").unwrap().len(), 1);
    }

    #[test]
    fn applies_to_the_instances_files() {
        let t = tempfile::tempdir().unwrap();
        let lab = super::super::Lab::new(t.path().into());
        let inst = lab.instance("t1");
        let mut m = super::super::manifest::tests::sample();
        m.files.insert("aco".into(), "drive_c/users/steamuser/Documents/ACO.ini".into());
        let ini = inst.prefix().join("drive_c/users/steamuser/Documents/ACO.ini");
        std::fs::create_dir_all(ini.parent().unwrap()).unwrap();
        std::fs::write(&ini, "; comment\r\n[Graphics]\r\nFullscreenWidth=800\r\nVSync=1\r\n").unwrap();
        let done = apply(&inst, &m, &parse_lines("aco:[Graphics]FullscreenWidth={width}\naco:Graphics.Extra=7").unwrap()).unwrap();
        assert_eq!(done[0].old, ["800"]);
        assert_eq!(done[0].new, "2560");
        assert!(done[1].old.is_empty() && done[1].undo().is_none());
        let text = std::fs::read_to_string(&ini).unwrap();
        assert!(text.starts_with("; comment\r\n[Graphics]\r\nFullscreenWidth=2560\r\n"), "{text:?}");
        assert!(text.contains("Extra=7") && text.contains("VSync=1"));
        // Undo puts the old value back.
        apply(&inst, &m, &[done[0].undo().unwrap()]).unwrap();
        assert!(std::fs::read_to_string(&ini).unwrap().contains("FullscreenWidth=800"));
        assert!(apply(&inst, &m, &[Assign::parse("pharos:[a]b=1").unwrap()]).is_err(), "missing file");
    }
}
