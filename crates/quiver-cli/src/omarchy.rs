//! `quiver omarchy …`: Quiver's Omarchy shell plugin, `quiver.downloads` (the download
//! queue in the Omarchy bar; source in contrib/omarchy/quiver.downloads).
//!
//! The install (tools/install-user.sh, packaging/layout.sh) puts the plugin at
//! `<libdir>/omarchy/quiver.downloads`. Omarchy's shell only finds plugins at the top level
//! of `~/.config/omarchy/plugins/` (its scan is `for sub in "$dir"/*/`, which follows a link
//! to a directory), so `install-plugin` links it there and puts the widget in the bar
//! with `omarchy bar put`, after backing up `shell.json`. `uninstall-plugin` takes it out of
//! the bar (`omarchy plugin disable`) and removes the link. Both are safe to run again: a
//! step that is already done is skipped. Nothing here runs unless asked to.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::Value;

pub const PLUGIN_ID: &str = "quiver.downloads";
const SECTIONS: [&str; 5] = ["left", "centerBegin", "center", "centerEnd", "right"];

#[derive(Subcommand)]
pub enum OmarchyAction {
    /// Link the downloads widget into ~/.config/omarchy/plugins and put it in the bar
    /// (shell.json is backed up first). Safe to run again: it only does what isn't done.
    InstallPlugin {
        /// Bar section: left, centerBegin, center, centerEnd or right.
        #[arg(long, default_value = "right")]
        section: String,
        /// Place it after this widget when the bar has it (else where Omarchy puts
        /// a widget in that section).
        #[arg(long, default_value = "hyprpad.status")]
        after: String,
        /// Only link the plugin; leave the bar as it is.
        #[arg(long)]
        no_bar: bool,
    },
    /// Take the widget out of the bar (shell.json is backed up first) and remove the link.
    #[command(alias = "remove-plugin")]
    UninstallPlugin,
    /// Where the plugin is installed, whether it is linked, and where the bar shows it.
    Status,
}

pub fn run(action: OmarchyAction) -> Result<()> {
    match action {
        OmarchyAction::InstallPlugin { section, after, no_bar } => install(&section, &after, no_bar),
        OmarchyAction::UninstallPlugin => uninstall(),
        OmarchyAction::Status => status(),
    }
}

/// The installed plugin: `$QUIVER_OMARCHY_PLUGIN`, else `<this exe>/../lib/quiver/omarchy/
/// quiver.downloads` (where install-user.sh and the package put it).
fn plugin_source() -> Result<PathBuf> {
    let dir = match std::env::var_os("QUIVER_OMARCHY_PLUGIN") {
        Some(d) => PathBuf::from(d),
        None => {
            let exe = std::env::current_exe().context("finding this executable")?;
            let exe = exe.canonicalize().unwrap_or(exe);
            exe.parent()
                .and_then(Path::parent)
                .map(|p| p.join("lib/quiver/omarchy").join(PLUGIN_ID))
                .context("finding Quiver's lib directory")?
        }
    };
    let manifest = dir.join("manifest.json");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("the plugin isn't installed ({} is missing; run tools/install-user.sh)", manifest.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    if v.get("id").and_then(Value::as_str) != Some(PLUGIN_ID) {
        bail!("{} isn't the {PLUGIN_ID} plugin", manifest.display());
    }
    Ok(dir)
}

fn config_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d).join("omarchy"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/omarchy"))
}

/// A link of ours: to the plugin as some Quiver install lays it out.
fn is_quiver_link(target: &Path) -> bool {
    target.ends_with(Path::new("omarchy").join(PLUGIN_ID))
}

/// Where `id` sits in the bar layout of a shell config: (section, index).
pub fn bar_location(config: &Value, id: &str) -> Option<(String, usize)> {
    let layout = config.get("bar")?.get("layout")?.as_object()?;
    for (section, list) in layout {
        let Some(list) = list.as_array() else { continue };
        if let Some(i) = list.iter().position(|e| e.get("id").and_then(Value::as_str) == Some(id) || e.as_str() == Some(id)) {
            return Some((section.clone(), i));
        }
    }
    None
}

/// Arguments for `omarchy bar put <id> …`: the section, and `--after` the anchor only when
/// that section has it (so the anchor missing never fails the put).
pub fn placement_args(config: &Value, section: &str, after: &str) -> Vec<String> {
    let mut args = vec!["--section".to_owned(), section.to_owned()];
    if bar_location(config, after).is_some_and(|(s, _)| s == section) {
        args.push("--after".to_owned());
        args.push(after.to_owned());
    }
    args
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}

/// Run an Omarchy command; its stdout, trimmed.
fn run_cmd(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn omarchy_shell(args: &[&str]) -> Result<String> {
    run_cmd("omarchy-shell", args)
}

/// The shell's effective config (what it has in memory, which it writes to shell.json).
fn shell_config() -> Result<Value> {
    let text = omarchy_shell(&["shell", "listShellConfig"])?;
    serde_json::from_str(&text).context("reading the shell's config (omarchy-shell shell listShellConfig)")
}

/// Wait for `done` to hold, up to `secs`.
fn wait_until(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if done() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn shell_knows_plugin() -> bool {
    omarchy_shell(&["shell", "listPlugins"])
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|list| list.iter().any(|p| p.get("id").and_then(Value::as_str) == Some(PLUGIN_ID)))
}

/// Copy shell.json aside before the bar changes: `shell.json.quiver-<time>.bak`.
fn backup_shell_json() -> Result<()> {
    let path = config_dir()?.join("shell.json");
    if !path.is_file() {
        return Ok(());
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = path.with_file_name(format!("shell.json.quiver-{stamp}.bak"));
    std::fs::copy(&path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    println!("backed up {} to {}", path.display(), backup.display());
    Ok(())
}

/// Link `<plugins>/quiver.downloads` to `src`: nothing when it already does, relinked when it
/// points at another Quiver install's copy, refused when something else is there.
fn link_plugin(src: &Path, link: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(src, link).with_context(|| format!("linking {}", link.display()))?;
            println!("linked {} -> {}", link.display(), src.display());
            Ok(true)
        }
        Err(e) => Err(e).with_context(|| format!("looking at {}", link.display())),
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(link)?;
            if target == src {
                println!("{} already links to {}", link.display(), src.display());
                Ok(false)
            } else if is_quiver_link(&target) {
                std::fs::remove_file(link)?;
                std::os::unix::fs::symlink(src, link)?;
                println!("relinked {} -> {} (was {})", link.display(), src.display(), target.display());
                Ok(true)
            } else {
                bail!("{} links to {}, not to Quiver's plugin; remove it first", link.display(), target.display());
            }
        }
        Ok(_) => bail!("{} exists and isn't Quiver's link (a copy or a clone?); remove it first", link.display()),
    }
}

fn install(section: &str, after: &str, no_bar: bool) -> Result<()> {
    if !SECTIONS.contains(&section) {
        bail!("section must be one of {}", SECTIONS.join(", "));
    }
    let src = plugin_source()?;
    let plugins = config_dir()?.join("plugins");
    std::fs::create_dir_all(&plugins).with_context(|| format!("creating {}", plugins.display()))?;
    let linked = link_plugin(&src, &plugins.join(PLUGIN_ID))?;
    if !on_path("omarchy-shell") || !on_path("omarchy") {
        println!("Omarchy's commands aren't on PATH, so the bar is left alone");
        return Ok(());
    }
    // The shell's file watcher doesn't look through the link: scan for the plugin.
    if linked || !shell_knows_plugin() {
        omarchy_shell(&["shell", "rescanPlugins"])?;
        if !wait_until(10, shell_knows_plugin) {
            bail!("the shell didn't pick up {PLUGIN_ID} (is omarchy-shell running? try: omarchy-shell shell rescanPlugins)");
        }
    }
    if no_bar {
        println!("the bar is left as it is; to add the widget: quiver omarchy install-plugin");
        return Ok(());
    }
    let config = shell_config()?;
    if let Some((s, i)) = bar_location(&config, PLUGIN_ID) {
        println!("already in the bar ({s}, position {})", i + 1);
        return Ok(());
    }
    backup_shell_json()?;
    let mut args = vec!["bar", "put", PLUGIN_ID];
    let placement = placement_args(&config, section, after);
    args.extend(placement.iter().map(String::as_str));
    run_cmd("omarchy", &args)?;
    // `omarchy bar put` answers success when it couldn't reach the shell: look.
    let mut at = None;
    wait_until(3, || {
        at = shell_config().ok().and_then(|c| bar_location(&c, PLUGIN_ID));
        at.is_some()
    });
    match at {
        Some((s, i)) => println!("in the bar: {s}, position {}", i + 1),
        None => bail!("omarchy bar put answered, but {PLUGIN_ID} isn't in the bar"),
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let shell = on_path("omarchy-shell") && on_path("omarchy");
    if shell {
        let config = shell_config()?;
        if bar_location(&config, PLUGIN_ID).is_some() {
            backup_shell_json()?;
            run_cmd("omarchy", &["plugin", "disable", PLUGIN_ID])?;
            if !wait_until(3, || shell_config().ok().is_some_and(|c| bar_location(&c, PLUGIN_ID).is_none())) {
                bail!("omarchy plugin disable answered, but {PLUGIN_ID} is still in the bar");
            }
            println!("took {PLUGIN_ID} out of the bar");
        } else {
            println!("{PLUGIN_ID} isn't in the bar");
        }
    } else {
        println!("Omarchy's commands aren't on PATH, so the bar is left alone");
    }
    let link = config_dir()?.join("plugins").join(PLUGIN_ID);
    let mut removed = false;
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(&link)?;
            if is_quiver_link(&target) {
                std::fs::remove_file(&link)?;
                removed = true;
                println!("removed the link {}", link.display());
            } else {
                println!("{} links to {}, not to Quiver's plugin; left alone", link.display(), target.display());
            }
        }
        Ok(_) => println!("{} isn't Quiver's link; left alone", link.display()),
        Err(_) => println!("{} isn't there", link.display()),
    }
    if removed && shell {
        omarchy_shell(&["shell", "rescanPlugins"])?;
    }
    Ok(())
}

fn status() -> Result<()> {
    match plugin_source() {
        Ok(src) => println!("plugin: {}", src.display()),
        Err(e) => println!("plugin: {e:#}"),
    }
    let link = config_dir()?.join("plugins").join(PLUGIN_ID);
    match std::fs::read_link(&link) {
        Ok(target) => println!("link: {} -> {}", link.display(), target.display()),
        Err(_) if link.exists() => println!("link: {} is a directory, not Quiver's link", link.display()),
        Err(_) => println!("link: none (quiver omarchy install-plugin)"),
    }
    if on_path("omarchy-shell") {
        match shell_config().map(|c| bar_location(&c, PLUGIN_ID)) {
            Ok(Some((s, i))) => println!("bar: {s}, position {}", i + 1),
            Ok(None) => println!("bar: not in the bar"),
            Err(e) => println!("bar: {e:#}"),
        }
    } else {
        println!("bar: omarchy-shell isn't on PATH");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Value {
        json!({ "bar": { "layout": {
            "left": [{ "id": "omarchy.menu" }],
            "center": [{ "id": "omarchy.clock", "format": "HH:mm" }],
            "right": [{ "id": "omarchy.tray" }, { "id": "hyprpad.status" }, { "id": PLUGIN_ID }],
            "tray": []
        } } })
    }

    #[test]
    fn finds_the_widget_in_the_layout() {
        assert_eq!(bar_location(&config(), PLUGIN_ID), Some(("right".to_owned(), 2)));
        assert_eq!(bar_location(&config(), "omarchy.clock"), Some(("center".to_owned(), 0)));
        assert_eq!(bar_location(&config(), "nope"), None);
        assert_eq!(bar_location(&json!({}), PLUGIN_ID), None);
        assert_eq!(bar_location(&json!({ "bar": { "layout": { "right": "junk" } } }), PLUGIN_ID), None);
    }

    #[test]
    fn places_after_the_anchor_only_when_the_section_has_it() {
        let c = config();
        assert_eq!(placement_args(&c, "right", "hyprpad.status"), ["--section", "right", "--after", "hyprpad.status"]);
        assert_eq!(placement_args(&c, "left", "hyprpad.status"), ["--section", "left"]);
        assert_eq!(placement_args(&c, "right", "omarchy.weather"), ["--section", "right"]);
        assert_eq!(placement_args(&json!({}), "right", "hyprpad.status"), ["--section", "right"]);
    }

    #[test]
    fn recognises_its_own_links() {
        assert!(is_quiver_link(Path::new("/home/u/.local/lib/quiver/omarchy/quiver.downloads")));
        assert!(is_quiver_link(Path::new("/usr/lib/quiver/omarchy/quiver.downloads")));
        assert!(!is_quiver_link(Path::new("/home/u/code/my-fork/quiver.downloads")));
    }

    #[test]
    fn links_once_and_refuses_what_isnt_ours() {
        let dir = std::env::temp_dir().join(format!("quiver-omarchy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("lib/quiver/omarchy").join(PLUGIN_ID);
        let other = dir.join("lib2/quiver/omarchy").join(PLUGIN_ID);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        let link = dir.join("plugins").join(PLUGIN_ID);
        assert!(link_plugin(&src, &link).unwrap(), "first run links");
        assert!(!link_plugin(&src, &link).unwrap(), "second run is a no-op");
        assert!(link_plugin(&other, &link).unwrap(), "another Quiver install's link is replaced");
        assert_eq!(std::fs::read_link(&link).unwrap(), other);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &link).unwrap();
        assert!(link_plugin(&src, &link).is_err(), "a foreign link is left alone");
        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir(&link).unwrap();
        assert!(link_plugin(&src, &link).is_err(), "a real directory is left alone");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
