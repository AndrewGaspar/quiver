//! `quiver lab fixture`: save fixtures (design/LAB.md §2, §8). A fixture is
//! `fixtures/<game>/<name>/prefix/<save folder>/…` (the instance's save folders, paths relative
//! to the prefix), a `README.md` with its note, and `fixture.png` when the game was running;
//! read-only once made. `save` takes them from an instance, `import` from save files, a folder
//! or an archive someone has, `install` copies one into an instance.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_core::lab::{Inst, Lab, Manifest, facts};

use super::{create, inst_of, ops, read_state};

#[derive(Subcommand)]
pub enum FixtureAction {
    Save {
        instance: String,
        name: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Make a fixture from saves you have: save files (`1.save 2.save …`), a folder of them,
    /// or an archive (zip, tar, tar.gz/xz/zst/bz2, 7z; read with bsdtar). They go into the
    /// instance's save folder (the manifest's `saves`, or `--into`), as `save` stores them. A
    /// folder or archive holding one folder is unwrapped; one holding `prefix/` (a fixture
    /// copied from another lab) is taken as it is.
    Import {
        instance: String,
        name: String,
        #[arg(required = true)]
        from: Vec<PathBuf>,
        /// The save folder, relative to the prefix (default: the manifest's only save folder).
        #[arg(long)]
        into: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    Install { instance: String, name: String },
    List { instance: String },
}

/// Fixture names: lowercase letters, digits, - and _.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        bail!("fixture names: lowercase letters, digits, - and _");
    }
    Ok(())
}

/// Copy a tree over another (making read-only files writable first, as an install over an
/// earlier one needs).
fn copy_tree(from: &Path, to: &Path) -> Result<usize> {
    let mut n = 0;
    for (rel, is_dir) in facts::walk(from, 16, 100_000) {
        if is_dir {
            continue;
        }
        let dst = to.join(&rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        if dst.exists() {
            let mut p = std::fs::metadata(&dst)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            std::fs::set_permissions(&dst, p)?;
        }
        std::fs::copy(from.join(&rel), &dst)?;
        n += 1;
    }
    Ok(n)
}

/// Every file in a fixture read-only.
fn seal(dir: &Path) -> Result<()> {
    for (rel, _) in facts::walk(dir, 16, 100_000) {
        let p = dir.join(rel);
        if p.is_file() {
            let mut perm = std::fs::metadata(&p)?.permissions();
            perm.set_readonly(true);
            std::fs::set_permissions(&p, perm)?;
        }
    }
    Ok(())
}

pub fn run(lab: &Lab, action: FixtureAction) -> Result<()> {
    match action {
        FixtureAction::List { instance } => {
            let (_, m) = inst_of(lab, &instance)?;
            let dir = lab.fixtures(&m.game.dir_name());
            println!("fixtures in {}:", dir.display());
            let mut rows: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().flatten().filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.')).collect();
            rows.sort_by_key(|e| e.file_name());
            for e in rows {
                let note = std::fs::read_to_string(e.path().join("README.md")).unwrap_or_default();
                println!("  {:<24} {}", e.file_name().to_string_lossy(), note.lines().find(|l| l.starts_with("Note:")).unwrap_or(""));
            }
        }
        FixtureAction::Save { instance, name, note } => {
            let (inst, m) = inst_of(lab, &instance)?;
            check_name(&name)?;
            let dir = lab.fixtures(&m.game.dir_name()).join(&name);
            if dir.exists() {
                bail!("fixture {name} exists ({})", dir.display());
            }
            let folders: Vec<String> = m.saves.iter().flat_map(|p| create::expand_saves(&inst.prefix(), p)).collect();
            if folders.is_empty() {
                bail!("the manifest names no save folder that exists (quiver lab edit {instance} … or add `saves` to {})", inst.manifest_path().display());
            }
            let mut n = 0;
            for f in &folders {
                n += copy_tree(&inst.prefix().join(f), &dir.join("prefix").join(f))?;
            }
            let st = read_state(&inst);
            let running = st.run.as_ref().is_some_and(|r| r.phase != quiver_core::lab::ready::Phase::Exited);
            if running {
                let _ = ops::shot(&inst, &dir.join("fixture.png"));
            }
            let readme = format!(
                "# Save fixture: {name}\n\nNote: {note}\n\nSaved {} from instance {instance} ({}), {n} file(s): {}.\n{}\nInstall: `quiver lab fixture install <instance> {name}` (game stopped).\n",
                chrono::Utc::now().to_rfc3339(),
                m.game,
                folders.join(", "),
                if running { "The game was running: files are as of its last save. fixture.png is the screen then.\n" } else { "" }
            );
            std::fs::write(dir.join("README.md"), readme)?;
            seal(&dir)?;
            println!("saved fixture {name} ({n} files) in {}", dir.display());
        }
        FixtureAction::Import { instance, name, from, into, note } => {
            let (inst, m) = inst_of(lab, &instance)?;
            let (dir, n, into) = import(lab, &inst, &m, &name, &from, into.as_deref(), &note)?;
            println!("imported fixture {name} ({n} files, into {into}) in {}", dir.display());
            println!("next: quiver lab fixture install {instance} {name} (game stopped)");
        }
        FixtureAction::Install { instance, name } => {
            let (inst, m) = inst_of(lab, &instance)?;
            let st = read_state(&inst);
            if st.run.as_ref().is_some_and(|r| r.phase != quiver_core::lab::ready::Phase::Exited) {
                bail!("stop the game first (quiver lab kill {instance}): it rewrites its saves on exit");
            }
            let n = install(lab, &inst, &m, &name)?;
            println!("installed fixture {name} ({n} files) in {instance}");
        }
    }
    Ok(())
}

/// Copy fixture `name` into the instance's prefix (its own saves kept once first, in
/// `saves-orig`).
fn install(lab: &Lab, inst: &Inst, m: &Manifest, name: &str) -> Result<usize> {
    let dir = lab.fixtures(&m.game.dir_name()).join(name).join("prefix");
    if !dir.is_dir() {
        bail!("no fixture {name} ({})", dir.display());
    }
    // The instance's own saves, once, before any fixture replaces them.
    let backup = inst.dir.join("saves-orig");
    if !backup.exists() {
        for f in m.saves.iter().flat_map(|p| create::expand_saves(&inst.prefix(), p)) {
            copy_tree(&inst.prefix().join(&f), &backup.join(&f))?;
        }
    }
    copy_tree(&dir, &inst.prefix())
}

/// Where imported saves go, relative to the prefix: `--into`, else the manifest's one save
/// folder (as found in the instance's prefix, `*` expanded).
fn save_folder(inst: &Inst, m: &Manifest, into: Option<&str>) -> Result<String> {
    if let Some(i) = into {
        let rel = i.trim_matches('/');
        if rel.is_empty() || Path::new(rel).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            bail!("--into '{i}': a folder relative to the prefix, without '..'");
        }
        return Ok(rel.to_owned());
    }
    let found: Vec<String> = m.saves.iter().flat_map(|p| create::expand_saves(&inst.prefix(), p)).collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => match m.saves.as_slice() {
            [one] if !one.contains('*') => Ok(one.clone()),
            _ => bail!(
                "the manifest names no save folder that exists in {} (saves: {:?}); say where with --into <folder relative to the prefix>",
                inst.name,
                m.saves
            ),
        },
        many => bail!("{} has {} save folders ({}); say which with --into", inst.name, many.len(), many.join(", ")),
    }
}

/// Archives by their names (bsdtar reads them all).
fn is_archive(p: &Path) -> bool {
    let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    [".zip", ".7z", ".tar", ".tgz", ".tbz2", ".txz", ".tzst", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"].iter().any(|e| n.ends_with(e))
}

/// Unpack an archive into the empty folder `to` with bsdtar (libarchive refuses absolute
/// paths and `..`, and won't write through symlinks), owner and permissions not kept.
fn unpack(archive: &Path, to: &Path) -> Result<()> {
    let out = std::process::Command::new("bsdtar")
        .args(["-x", "--no-same-owner", "--no-same-permissions", "--no-xattrs", "--no-acls", "--no-fflags", "-f"])
        .arg(archive)
        .arg("-C")
        .arg(to)
        .output()
        .context("unpack with bsdtar (libarchive's tar, for zip and tar archives): is it installed?")?;
    if !out.status.success() {
        bail!("bsdtar couldn't unpack {}: {}", archive.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Copy a tree of plain files and folders, refusing anything else (symlinks, devices): what
/// someone handed over goes into a fixture as data only. Returns the files copied.
fn copy_plain(from: &Path, to: &Path, depth: usize) -> Result<usize> {
    if depth > 32 {
        bail!("{}: folders nested too deep", from.display());
    }
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    let mut entries: Vec<_> = std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ft = std::fs::symlink_metadata(&src)?.file_type();
        if ft.is_dir() {
            n += copy_plain(&src, &dst, depth + 1)?;
        } else if ft.is_file() {
            std::fs::copy(&src, &dst).with_context(|| format!("copy {}", src.display()))?;
            n += 1;
        } else {
            bail!("{} is a symlink or a special file: only plain files and folders are imported", src.display());
        }
    }
    Ok(n)
}

/// A folder holding nothing but one folder is that folder (an archive of `3539/`, say).
fn unwrap(mut dir: PathBuf) -> Result<PathBuf> {
    for _ in 0..8 {
        let entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        match entries.as_slice() {
            [one] if one.file_name() != "prefix" && std::fs::symlink_metadata(one.path())?.is_dir() => dir = one.path(),
            _ => break,
        }
    }
    Ok(dir)
}

/// `quiver lab fixture import`: returns the fixture's folder, the files in it and the save
/// folder they went to. Built in a hidden folder next to it and renamed into place, so a
/// failure leaves no fixture behind.
fn import(lab: &Lab, inst: &Inst, m: &Manifest, name: &str, from: &[PathBuf], into: Option<&str>, note: &str) -> Result<(PathBuf, usize, String)> {
    check_name(name)?;
    let fixtures = lab.fixtures(&m.game.dir_name());
    let dir = fixtures.join(name);
    if dir.exists() {
        bail!("fixture {name} exists ({})", dir.display());
    }
    // The paths given are followed (they're the caller's); nothing inside them is.
    for f in from {
        std::fs::metadata(f).with_context(|| format!("{} not found", f.display()))?;
    }
    let folder = save_folder(inst, m, into);
    std::fs::create_dir_all(&fixtures)?;
    let stage = fixtures.join(format!(".{name}.import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let made = (|| -> Result<(usize, String)> {
        // One folder or archive: what's in it (one wrapping folder unwrapped).
        let tree = match from {
            [one] if is_archive(one) => {
                let unpacked = stage.join(".unpacked");
                std::fs::create_dir_all(&unpacked)?;
                unpack(one, &unpacked)?;
                Some(unwrap(unpacked)?)
            }
            [one] if std::fs::metadata(one)?.is_dir() => Some(unwrap(one.clone())?),
            _ => None,
        };
        let (n, at) = match tree {
            // A fixture (from another lab): its prefix tree as it is.
            Some(t) if into.is_none() && t.join("prefix").is_dir() => (copy_plain(&t.join("prefix"), &stage.join("prefix"), 0)?, "(the fixture's own folders)".to_owned()),
            Some(t) => {
                let folder = folder?;
                (copy_plain(&t, &stage.join("prefix").join(&folder), 0)?, folder)
            }
            None => {
                let folder = folder?;
                let to = stage.join("prefix").join(&folder);
                std::fs::create_dir_all(&to)?;
                let mut n = 0;
                for f in from {
                    if !std::fs::metadata(f)?.is_file() {
                        bail!("{}: give save files, or one folder or archive", f.display());
                    }
                    let file = f.file_name().context("a file name")?;
                    if to.join(file).exists() {
                        bail!("two files named {}", file.to_string_lossy());
                    }
                    std::fs::copy(f, to.join(file)).with_context(|| format!("copy {}", f.display()))?;
                    n += 1;
                }
                (n, folder)
            }
        };
        let _ = std::fs::remove_dir_all(stage.join(".unpacked"));
        if n == 0 {
            bail!("nothing to import: no files in {}", from.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", "));
        }
        let readme = format!(
            "# Save fixture: {name}\n\nNote: {note}\n\nImported {} for instance {} ({}), {n} file(s) into {at}, from {}.\n\nInstall: `quiver lab fixture install <instance> {name}` (game stopped).\n",
            chrono::Utc::now().to_rfc3339(),
            inst.name,
            m.game,
            from.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")
        );
        std::fs::write(stage.join("README.md"), readme)?;
        seal(&stage)?;
        Ok((n, at))
    })();
    match made {
        Ok((n, at)) => {
            std::fs::rename(&stage, &dir).with_context(|| format!("move the fixture into {}", dir.display()))?;
            Ok((dir, n, at))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const SAVE: &str = "drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames/*/3539";

    /// A lab with one instance whose prefix has a save folder for user `u1`.
    fn lab() -> (tempfile::TempDir, Lab, Inst, Manifest) {
        let t = tempfile::tempdir().unwrap();
        let lab = Lab::new(t.path().join("lab"));
        let inst = lab.instance("t1");
        let mut m: Manifest = toml::from_str(
            "name='t1'\ngame='ubisoft:3539'\ntitle='x'\nlicence='ubisoft-bridge'\ncreated='2026-11-18T10:00:00Z'\n[display]\nwidth=1280\nheight=720\nrefresh=60\n[runtime]\nproton='p'\nproton_path='/p'\nimage='i'\n[paths]\ninstall='/g'\nprefix='/pf'\nexe='g.exe'\n",
        )
        .unwrap();
        m.saves = vec![SAVE.into()];
        std::fs::create_dir_all(inst.prefix().join(SAVE.replace('*', "u1"))).unwrap();
        m.save(&inst.manifest_path()).unwrap();
        (t, lab, inst, m)
    }

    fn saves(dir: &Path, n: u32) -> Vec<PathBuf> {
        std::fs::create_dir_all(dir).unwrap();
        (1..=n)
            .map(|i| {
                let p = dir.join(format!("{i}.save"));
                std::fs::write(&p, format!("save {i}")).unwrap();
                p
            })
            .collect()
    }

    fn bsdtar() -> bool {
        std::process::Command::new("bsdtar").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn save_files_become_a_fixture_like_a_saved_one() {
        let (t, lab, inst, m) = lab();
        let files = saves(&t.path().join("from"), 6);
        let (dir, n, into) = import(&lab, &inst, &m, "late-game", &files, None, "Siwa, level 40").unwrap();
        let folder = SAVE.replace('*', "u1");
        assert_eq!((n, into.as_str()), (6, folder.as_str()));
        assert_eq!(dir, lab.fixtures(&m.game.dir_name()).join("late-game"));
        for i in 1..=6 {
            let f = dir.join("prefix").join(&folder).join(format!("{i}.save"));
            assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("save {i}"));
            assert!(std::fs::metadata(&f).unwrap().permissions().readonly(), "read-only, as saved fixtures are");
        }
        let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
        assert!(readme.lines().any(|l| l == "Note: Siwa, level 40"), "{readme}");
        // Nothing left of the staging folder; the name is taken now.
        assert_eq!(std::fs::read_dir(dir.parent().unwrap()).unwrap().count(), 1);
        assert!(import(&lab, &inst, &m, "late-game", &files, None, "").unwrap_err().to_string().contains("exists"));
        // And it installs like any other.
        assert_eq!(install(&lab, &inst, &m, "late-game").unwrap(), 6);
        assert_eq!(std::fs::read_to_string(inst.prefix().join(&folder).join("3.save")).unwrap(), "save 3");
    }

    #[test]
    fn folders_and_archives() {
        let (t, lab, inst, m) = lab();
        let folder = SAVE.replace('*', "u1");
        // A folder holding one folder of saves: unwrapped.
        saves(&t.path().join("pack/3539"), 3);
        let (dir, n, _) = import(&lab, &inst, &m, "from-dir", &[t.path().join("pack")], None, "").unwrap();
        assert_eq!(n, 3);
        assert!(dir.join("prefix").join(&folder).join("2.save").is_file());
        if !bsdtar() {
            eprintln!("skipping the archive cases: no bsdtar");
            return;
        }
        for (name, archive) in [("zipped", "saves.zip"), ("tarred", "saves.tar.gz")] {
            let a = t.path().join(archive);
            let st = std::process::Command::new("bsdtar").arg("-a").arg("-cf").arg(&a).arg("-C").arg(t.path().join("pack")).arg("3539").status().unwrap();
            assert!(st.success());
            let (dir, n, _) = import(&lab, &inst, &m, name, &[a], None, "").unwrap();
            assert_eq!(n, 3, "{archive}");
            assert_eq!(std::fs::read_to_string(dir.join("prefix").join(&folder).join("1.save")).unwrap(), "save 1");
            assert!(!dir.join(".unpacked").exists());
        }
        // A fixture copied from another lab: its prefix tree as it is, whatever its folders.
        let other = t.path().join("other-fixture");
        saves(&other.join("prefix/drive_c/elsewhere"), 2);
        std::fs::write(other.join("README.md"), "# Save fixture: x\n").unwrap();
        let (dir, n, _) = import(&lab, &inst, &m, "as-is", &[other], None, "").unwrap();
        assert_eq!(n, 2);
        assert!(dir.join("prefix/drive_c/elsewhere/2.save").is_file());
    }

    #[test]
    fn what_isnt_imported() {
        let (t, lab, inst, mut m) = lab();
        let files = saves(&t.path().join("from"), 2);
        assert!(import(&lab, &inst, &m, "Bad Name", &files, None, "").is_err());
        assert!(import(&lab, &inst, &m, "x", &[t.path().join("missing.save")], None, "").unwrap_err().to_string().contains("not found"));
        assert!(import(&lab, &inst, &m, "x", &files, Some("../escape"), "").unwrap_err().to_string().contains("'..'"));
        // A symlink in a folder (it could point anywhere): refused, and nothing is left.
        let d = t.path().join("linky");
        saves(&d, 1);
        std::os::unix::fs::symlink("/etc/passwd", d.join("2.save")).unwrap();
        let e = import(&lab, &inst, &m, "linky", std::slice::from_ref(&d), None, "").unwrap_err().to_string();
        assert!(e.contains("symlink"), "{e}");
        assert!(!lab.fixtures(&m.game.dir_name()).join("linky").exists());
        assert_eq!(std::fs::read_dir(lab.fixtures(&m.game.dir_name())).unwrap().count(), 0, "no staging folder left");
        if bsdtar() {
            // The same in an archive.
            let a = t.path().join("linky.tar");
            assert!(std::process::Command::new("bsdtar").arg("-cf").arg(&a).arg("-C").arg(&d).arg(".").status().unwrap().success());
            let e = import(&lab, &inst, &m, "linky", &[a], None, "").unwrap_err().to_string();
            assert!(e.contains("symlink"), "{e}");
            assert_eq!(std::fs::read_dir(lab.fixtures(&m.game.dir_name())).unwrap().count(), 0);
        }
        // Two save folders: say which.
        std::fs::create_dir_all(inst.prefix().join(SAVE.replace('*', "u2"))).unwrap();
        let e = import(&lab, &inst, &m, "two", &files, None, "").unwrap_err().to_string();
        assert!(e.contains("--into"), "{e}");
        let (dir, _, into) = import(&lab, &inst, &m, "two", &files, Some(&SAVE.replace('*', "u2")), "").unwrap();
        assert!(dir.join("prefix").join(&into).join("1.save").is_file() && into.contains("/u2/"));
        // No save folder in the prefix yet, but a plain one in the manifest: that one.
        m.saves = vec!["drive_c/saves".into()];
        let (_, _, into) = import(&lab, &inst, &m, "fresh", &files, None, "").unwrap();
        assert_eq!(into, "drive_c/saves");
        // Read-only for everyone.
        let f = lab.fixtures(&m.game.dir_name()).join("fresh/prefix/drive_c/saves/1.save");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o222, 0);
    }
}
