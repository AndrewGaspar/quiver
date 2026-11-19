//! `quiver omarchy …`: Quiver's Omarchy shell plugin, `quiver.downloads` (the download
//! queue in the Omarchy bar; source in contrib/omarchy/quiver.downloads). The install puts
//! the plugin at `<libdir>/omarchy/quiver.downloads`; `install-plugin` links it into
//! `~/.config/omarchy/plugins` and places it in the bar through Omarchy's own commands,
//! after backing up `shell.json`. Nothing here touches the bar unless asked to, and every
//! step is skipped when it is already done.

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
        /// Place it after this widget (default: after omarchy.tray, when the section has it).
        #[arg(long)]
        after: Option<String>,
        /// Only link the plugin; leave the bar as it is.
        #[arg(long)]
        no_bar: bool,
    },
    /// Take the widget out of the bar (shell.json is backed up first) and remove the link.
    RemovePlugin,
    /// Where the plugin is installed, whether it is linked, and where the bar shows it.
    Status,
}

pub fn run(action: OmarchyAction) -> Result<()> {
    match action {
        OmarchyAction::InstallPlugin { section, after, no_bar } => install(&section, after.as_deref(), no_bar),
        OmarchyAction::RemovePlugin => remove(),
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

/// Where `id` sits in the bar layout of a shell config (`omarchy-shell shell listShellConfig`).
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

/// Arguments for `omarchy-plugin-enable <id> …`: the section, and `--after` a widget only
/// when that section has it (default: after the tray, so it sits by the other status icons).
pub fn placement_args(config: &Value, section: &str, after: Option<&str>) -> Vec<String> {
    let mut args = vec!["--section".to_owned(), section.to_owned()];
    let anchor = after.unwrap_or("omarchy.tray");
    let in_section = config
        .get("bar")
        .and_then(|b| b.get("layout"))
        .and_then(|l| l.get(section))
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|e| e.get("id").and_then(Value::as_str) == Some(anchor)));
    if in_section {
        args.push("--after".to_owned());
        args.push(anchor.to_owned());
    }
    args
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}

/// `omarchy-shell <args>`: its answer, trimmed.
fn omarchy_shell(args: &[&str]) -> Result<String> {
    let out = Command::new("omarchy-shell").args(args).output().context("running omarchy-shell")?;
    if !out.status.success() {
        bail!(
            "omarchy-shell {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn shell_config() -> Result<Value> {
    let text = omarchy_shell(&["shell", "listShellConfig"])?;
    serde_json::from_str(&text).context("reading the shell's config (omarchy-shell shell listShellConfig)")
}

/// The shell knows the plugin (after a rescan, which runs in the background).
fn wait_for_plugin() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let known = omarchy_shell(&["shell", "listPlugins"])
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .is_some_and(|list| list.iter().any(|p| p.get("id").and_then(Value::as_str) == Some(PLUGIN_ID)));
        if known {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("the shell didn't pick up {PLUGIN_ID} (try: omarchy-shell shell rescanPlugins)");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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

fn install(section: &str, after: Option<&str>, no_bar: bool) -> Result<()> {
    if !SECTIONS.contains(&section) {
        bail!("section must be one of {}", SECTIONS.join(", "));
    }
    let src = plugin_source()?;
    let plugins = config_dir()?.join("plugins");
    std::fs::create_dir_all(&plugins).with_context(|| format!("creating {}", plugins.display()))?;
    let link = plugins.join(PLUGIN_ID);
    match std::fs::symlink_metadata(&link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(&src, &link).with_context(|| format!("linking {}", link.display()))?;
            println!("linked {} -> {}", link.display(), src.display());
        }
        Err(e) => return Err(e).with_context(|| format!("looking at {}", link.display())),
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(&link)?;
            if target == src {
                println!("{} already links to {}", link.display(), src.display());
            } else if is_quiver_link(&target) {
                std::fs::remove_file(&link)?;
                std::os::unix::fs::symlink(&src, &link)?;
                println!("relinked {} -> {} (was {})", link.display(), src.display(), target.display());
            } else {
                bail!("{} links to {}; remove it first (omarchy plugin remove {PLUGIN_ID})", link.display(), target.display());
            }
        }
        Ok(_) => bail!(
            "{} exists and isn't Quiver's link (a copy or a clone?); remove it first (omarchy plugin remove {PLUGIN_ID})",
            link.display()
        ),
    }
    if !on_path("omarchy-shell") {
        println!("omarchy-shell isn't on PATH, so the bar is left alone; in Omarchy: omarchy plugin enable {PLUGIN_ID} --section {section}");
        return Ok(());
    }
    // A plugin directory changing behind a link isn't seen by the shell's file watcher.
    omarchy_shell(&["shell", "rescanPlugins"])?;
    wait_for_plugin()?;
    if no_bar {
        println!("the bar is left as it is; to add it: omarchy plugin enable {PLUGIN_ID} --section {section}");
        return Ok(());
    }
    let config = shell_config()?;
    if let Some((s, i)) = bar_location(&config, PLUGIN_ID) {
        println!("already in the bar ({s}, position {})", i + 1);
        return Ok(());
    }
    backup_shell_json()?;
    let args = placement_args(&config, section, after);
    let out = Command::new("omarchy-plugin-enable")
        .arg(PLUGIN_ID)
        .args(&args)
        .output()
        .context("running omarchy-plugin-enable")?;
    if !out.status.success() {
        bail!("omarchy plugin enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    print!("{}", String::from_utf8_lossy(&out.stdout));
    match bar_location(&shell_config()?, PLUGIN_ID) {
        Some((s, i)) => println!("in the bar: {s}, position {}", i + 1),
        None => bail!("omarchy plugin enable answered, but {PLUGIN_ID} isn't in the bar"),
    }
    Ok(())
}

fn remove() -> Result<()> {
    if on_path("omarchy-shell") {
        let config = shell_config()?;
        if bar_location(&config, PLUGIN_ID).is_some() {
            backup_shell_json()?;
            let answer = omarchy_shell(&["shell", "setPluginEnabled", PLUGIN_ID, "false"])?;
            if answer != "ok" {
                bail!("omarchy-shell couldn't take {PLUGIN_ID} out of the bar: {answer}");
            }
            println!("took {PLUGIN_ID} out of the bar");
        } else {
            println!("{PLUGIN_ID} isn't in the bar");
        }
    }
    let link = config_dir()?.join("plugins").join(PLUGIN_ID);
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(&link)?;
            if is_quiver_link(&target) {
                std::fs::remove_file(&link)?;
                println!("removed the link {}", link.display());
            } else {
                println!("{} links to {}, not to Quiver's plugin; left alone", link.display(), target.display());
            }
        }
        Ok(_) => println!("{} isn't Quiver's link; left alone", link.display()),
        Err(_) => println!("{} isn't there", link.display()),
    }
    if on_path("omarchy-shell") {
        let _ = omarchy_shell(&["shell", "rescanPlugins"]);
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
            "right": [{ "id": "omarchy.tray" }, { "id": "omarchy.audio" }, { "id": PLUGIN_ID }],
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
    fn places_after_the_tray_only_when_the_section_has_it() {
        let c = config();
        assert_eq!(placement_args(&c, "right", None), ["--section", "right", "--after", "omarchy.tray"]);
        assert_eq!(placement_args(&c, "left", None), ["--section", "left"]);
        assert_eq!(placement_args(&c, "right", Some("omarchy.audio")), ["--section", "right", "--after", "omarchy.audio"]);
        assert_eq!(placement_args(&c, "right", Some("omarchy.weather")), ["--section", "right"]);
    }

    #[test]
    fn recognises_its_own_links() {
        assert!(is_quiver_link(Path::new("/home/u/.local/lib/quiver/omarchy/quiver.downloads")));
        assert!(is_quiver_link(Path::new("/usr/lib/quiver/omarchy/quiver.downloads")));
        assert!(!is_quiver_link(Path::new("/home/u/code/my-fork/quiver.downloads")));
    }
}
