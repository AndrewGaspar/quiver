//! Steam copies of Ubisoft games, run against Quiver instead of Ubisoft Connect.
//!
//! Set a Steam game's launch options to `quiver steam-run %command%`. Steam then runs
//! `quiver` with its whole Proton command line (reaper, Steam Linux Runtime entry point,
//! `proton waitforexitandrun <game.exe> <args>`) and its compat environment
//! (`STEAM_COMPAT_DATA_PATH`, `SteamAppId`, …). `steam-run` keeps all of that and swaps only
//! the game executable for `quiver-bridge.exe --run <context> -- <game.exe> <args>`, then execs
//! the result. So Steam's own Proton, container, prefix, overlay and Steam Input run the
//! bridge; the bridge owns the SDK pipes and starts the game as its child with
//! `UPLAY_ARGUMENTS`. Nothing in the prefix changes.
//!
//! Why this is enough for both SDK generations (Connect 13416's DLLs, which Steam's install
//! script puts in the prefix): `uplay_r164.dll` and `uplay_r264.dll` embed the same
//! `uplaydll` protocol and pipe names. At startup the R2 DLL tries to connect to
//! `uplay_api_process_ipc_pipe_` *first*; only when nothing listens and there is no
//! `UPLAY_ARGUMENTS` does it start `UbisoftGameLauncher.exe … [-uplay_steam_mode]
//! -upc_game_starter` (with `UPLAY_ARGUMENTS` set it fails instead of starting Connect).
//! As a second guard the launch disables Connect's executables with `WINEDLLOVERRIDES`
//! (`UbisoftGameLauncher.exe=d`, …), which only affects this launch.
//!
//! This module is the pure part: environment, product mapping, command rewriting,
//! registry text reads/edits. `quiver steam-run` (quiver-cli) does the I/O.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::exports::textvdf;

/// Connect's install directory as games see it (and as Steam's install script installs it).
pub const CONNECT_WIN: &str = r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher";
/// `HKLM\Software\Wow6432Node\Ubisoft\Launcher` as written in `system.reg`.
pub const LAUNCHER_KEY: &str = r"Software\Wow6432Node\Ubisoft\Launcher";

/// Executables that would start (or be) Ubisoft Connect, disabled for the launch.
pub const CONNECT_EXES: &[&str] = &[
    "UbisoftGameLauncher.exe",
    "UbisoftGameLauncher64.exe",
    "UbisoftConnect.exe",
    "upc.exe",
    "UplayService.exe",
    "UplayWebCore.exe",
    "UbisoftExtension.exe",
];

/// Steam app id -> Ubisoft product id, for games whose install script doesn't say.
/// Black Flag Resynced's does (`…\Launcher\Installs\66088`); it's here as a check.
const KNOWN: &[(u32, u32)] = &[(3751950, 66088)];

/// What Steam tells a launch through its environment.
#[derive(Debug, Clone, PartialEq)]
pub struct SteamEnv {
    pub app_id: u32,
    /// `steamapps/compatdata/<appid>` (the Wine prefix is its `pfx/`).
    pub compat_data: PathBuf,
    /// `steamapps/common/<game>`.
    pub install: Option<PathBuf>,
}

impl SteamEnv {
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let compat_data = var("STEAM_COMPAT_DATA_PATH")
            .filter(|s| !s.is_empty())
            .context("STEAM_COMPAT_DATA_PATH is not set: run this from a Steam game's launch options, as `quiver steam-run %command%`, with a Proton compatibility tool")?;
        let app_id = ["SteamAppId", "STEAM_COMPAT_APP_ID", "SteamGameId"]
            .iter()
            .find_map(|k| var(k).and_then(|v| v.parse::<u32>().ok()).filter(|&n| n != 0))
            .or_else(|| Path::new(&compat_data).file_name()?.to_str()?.parse().ok())
            .context("no Steam app id (SteamAppId) in the environment")?;
        Ok(Self {
            app_id,
            compat_data: PathBuf::from(compat_data),
            install: var("STEAM_COMPAT_INSTALL_PATH").filter(|s| !s.is_empty()).map(PathBuf::from),
        })
    }

    pub fn prefix(&self) -> PathBuf {
        self.compat_data.join("pfx")
    }

    pub fn installscript(&self) -> Option<PathBuf> {
        Some(self.install.as_ref()?.join(format!("installscript_{}.vdf", self.app_id)))
    }
}

/// Where a product id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductSource {
    Override,
    /// The game's `installscript_<appid>.vdf` registers `…\Ubisoft\Launcher\Installs\<id>`.
    InstallScript,
    /// The prefix's `system.reg` has exactly one `…\Launcher\Installs\<id>` key.
    Registry,
    Known,
}

/// The Ubisoft product id for a Steam app: an explicit override, else the game's install
/// script, else the prefix's registry (if it names exactly one product), else the table.
pub fn resolve_product(
    app_id: u32,
    over: Option<u32>,
    installscript: Option<&str>,
    system_reg: Option<&str>,
) -> Result<(u32, ProductSource)> {
    if let Some(id) = over {
        return Ok((id, ProductSource::Override));
    }
    detect_product(app_id, installscript, system_reg).with_context(|| {
        format!("don't know which Ubisoft product Steam app {app_id} is: pass --product <id> (`quiver library` lists ids)")
    })
}

/// [`resolve_product`] without an override: the install script, else the prefix's registry
/// (if it names exactly one product), else the table. `None` when none of them says.
pub fn detect_product(app_id: u32, installscript: Option<&str>, system_reg: Option<&str>) -> Option<(u32, ProductSource)> {
    if let Some(id) = installscript.and_then(product_from_installscript) {
        return Some((id, ProductSource::InstallScript));
    }
    if let Some(text) = system_reg {
        let ids = registry_installs(text);
        if let [id] = ids[..] {
            return Some((id, ProductSource::Registry));
        }
    }
    known_product(app_id).map(|id| (id, ProductSource::Known))
}

/// The table's product id for a Steam app.
pub fn known_product(app_id: u32) -> Option<u32> {
    KNOWN.iter().find(|(a, _)| *a == app_id).map(|&(_, id)| id)
}

impl ProductSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::InstallScript => "install_script",
            Self::Registry => "registry",
            Self::Known => "known",
        }
    }
}

fn installs_id(key: &str) -> Option<u32> {
    let k = key.replace("\\\\", "\\").to_ascii_lowercase();
    let i = k.find(r"ubisoft\launcher\installs\")?;
    k[i + r"ubisoft\launcher\installs\".len()..].trim_end_matches('\\').parse().ok()
}

/// The product id a Steam install script registers under `…\Ubisoft\Launcher\Installs\`.
pub fn product_from_installscript(text: &str) -> Option<u32> {
    fn walk(n: &textvdf::Node) -> Option<u32> {
        n.entries().iter().find_map(|(k, v)| installs_id(k).or_else(|| walk(v)))
    }
    walk(&textvdf::parse(text).ok()?)
}

/// Product ids with a `…\Ubisoft\Launcher\Installs\<id>` key in a Wine `.reg` file.
pub fn registry_installs(text: &str) -> Vec<u32> {
    let mut ids: Vec<u32> = text
        .lines()
        .filter_map(|l| l.strip_prefix('[')?.split(']').next())
        .filter_map(installs_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

// ---------------------------------------------------------------------------------------
// Wine registry text (`system.reg`)

fn reg_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn reg_unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `[key]` header line matches `key` (as written: `\` between parts), case-insensitively.
fn is_header(line: &str, key: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|r| r.split(']').next())
        .is_some_and(|k| k.replace("\\\\", "\\").eq_ignore_ascii_case(key))
}

/// A string value in a Wine `.reg` file (`key` like `Software\Wow6432Node\Ubisoft\Launcher`).
pub fn reg_value(text: &str, key: &str, name: &str) -> Option<String> {
    let mut lines = text.lines().skip_while(|l| !is_header(l, key));
    lines.next()?;
    let prefix = format!("\"{}\"=\"", reg_escape(name));
    for l in lines.take_while(|l| !l.starts_with('[')) {
        if l.len() >= prefix.len() && l[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            return Some(reg_unescape(l[prefix.len()..].trim_end().strip_suffix('"')?));
        }
    }
    None
}

/// Set a string value in a Wine `.reg` file's text, adding the key if it's missing.
/// `stamp` is the key's modification time as Wine writes it (seconds since 1970).
pub fn set_reg_value(text: &str, key: &str, name: &str, value: &str, stamp: u64) -> String {
    let entry = format!("\"{}\"=\"{}\"", reg_escape(name), reg_escape(value));
    let prefix = format!("\"{}\"=", reg_escape(name));
    let mut out: Vec<String> = Vec::new();
    let mut lines = text.lines().peekable();
    let mut done = false;
    while let Some(l) = lines.next() {
        out.push(l.to_owned());
        if done || !is_header(l, key) {
            continue;
        }
        // Inside the key: replace the value, or add it after the header's metadata lines.
        let mut body: Vec<String> = Vec::new();
        while let Some(n) = lines.peek() {
            if n.starts_with('[') {
                break;
            }
            body.push(lines.next().unwrap().to_owned());
        }
        match body.iter().position(|b| b.len() >= prefix.len() && b[..prefix.len()].eq_ignore_ascii_case(&prefix)) {
            Some(i) => body[i] = entry.clone(),
            None => {
                let at = body.iter().take_while(|b| b.starts_with('#')).count();
                body.insert(at, entry.clone());
            }
        }
        out.extend(body);
        done = true;
    }
    if !done {
        if out.last().is_some_and(|l| !l.is_empty()) {
            out.push(String::new());
        }
        out.push(format!("[{}] {stamp}", key.replace('\\', "\\\\")));
        out.push(entry);
        out.push(String::new());
    }
    let mut s = out.join("\n");
    if text.ends_with('\n') || !done {
        s.push('\n');
    }
    s
}

/// A Windows path in a Proton prefix (`C:\…`, `Z:\…`) on the host.
pub fn host_path(prefix: &Path, win: &str) -> Option<PathBuf> {
    let w = win.replace('\\', "/");
    let (drive, rest) = w.split_once(':')?;
    let rest = rest.trim_start_matches('/');
    match drive.to_ascii_lowercase().as_str() {
        "c" => Some(prefix.join("drive_c").join(rest)),
        "z" => Some(PathBuf::from("/").join(rest)),
        _ => None,
    }
}

/// Where the SDK DLLs come from in this prefix: `InstallDir` and whether it holds them.
#[derive(Debug, Clone, PartialEq)]
pub struct SdkCheck {
    pub install_dir: Option<String>,
    pub host_dir: Option<PathBuf>,
    pub has_r1: bool,
    pub has_r2: bool,
}

pub fn check_sdk(prefix: &Path, system_reg: &str) -> SdkCheck {
    let install_dir = reg_value(system_reg, LAUNCHER_KEY, "InstallDir");
    let host_dir = install_dir.as_deref().and_then(|d| host_path(prefix, d));
    let has = |dll: &str| host_dir.as_ref().is_some_and(|d| d.join(dll).is_file());
    SdkCheck { has_r1: has("uplay_r164.dll"), has_r2: has("uplay_r264.dll"), install_dir, host_dir }
}

// ---------------------------------------------------------------------------------------
// Steam's command line

/// Proton verbs that take an executable.
const VERBS: &[&str] = &["waitforexitandrun", "run", "runinprefix", "waitforexitandrunrun"];

/// Where Proton and the game are in Steam's `%command%`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtonCommand {
    /// The `.../proton` script.
    pub proton: usize,
    pub verb: usize,
    pub exe: usize,
}

impl ProtonCommand {
    /// The last `…/proton <verb> <exe>` in `argv` (wrappers such as the reaper and the
    /// Steam Linux Runtime entry point come before it).
    pub fn find(argv: &[String]) -> Result<Self> {
        (0..argv.len().saturating_sub(2))
            .rev()
            .find(|&i| {
                Path::new(&argv[i]).file_name().is_some_and(|n| n == "proton") && VERBS.contains(&argv[i + 1].as_str())
            })
            .map(|i| Self { proton: i, verb: i + 1, exe: i + 2 })
            .with_context(|| {
                format!(
                    "no `proton <verb> <game.exe>` in the command Steam passed ({}); is the game set to run with Proton?",
                    argv.join(" ")
                )
            })
    }

    pub fn exe<'a>(&self, argv: &'a [String]) -> &'a str {
        &argv[self.exe]
    }

    pub fn game_args<'a>(&self, argv: &'a [String]) -> &'a [String] {
        &argv[self.exe + 1..]
    }

    /// `argv` with the game (and its arguments) replaced by `bridge` and `bridge_args`.
    pub fn rewrite(&self, argv: &[String], bridge: &str, bridge_args: &[String]) -> Vec<String> {
        let mut out = argv[..self.exe].to_vec();
        out.push(bridge.to_owned());
        out.extend(bridge_args.iter().cloned());
        out
    }
}

/// Host path -> Windows path through Wine's Z: drive (Proton maps `/` there).
pub fn z_path(p: &Path) -> String {
    crate::launcher::win_path(p)
}

/// Host path -> Windows path the way Wine itself converts it: through the drive (in the
/// prefix's `dosdevices`) whose root is the deepest ancestor of `p`. Proton maps the Steam
/// root to `S:`, so a game Steam launches runs as `S:\steamapps\common\…`, not `Z:\home\…`;
/// games can treat those differently (Black Flag Resynced shows a "game is on a HDD" warning
/// when started from `Z:`). Falls back to Z:.
pub fn prefix_win_path(prefix: &Path, p: &Path) -> String {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    drive_path(&drives(prefix), &p).unwrap_or_else(|| z_path(&p))
}

/// The prefix's drive letters and their (canonical) host roots.
fn drives(prefix: &Path) -> Vec<(char, PathBuf)> {
    let dir = prefix.join("dosdevices");
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_ascii_lowercase();
            let mut cs = name.chars();
            let (letter, colon) = (cs.next()?, cs.next()?);
            if !letter.is_ascii_alphabetic() || colon != ':' || cs.next().is_some() {
                return None;
            }
            Some((letter.to_ascii_uppercase(), std::fs::canonicalize(e.path()).ok()?))
        })
        .collect()
}

fn drive_path(drives: &[(char, PathBuf)], p: &Path) -> Option<String> {
    let (letter, root) = drives.iter().filter(|(_, root)| p.starts_with(root)).max_by_key(|(_, root)| root.components().count())?;
    let rest = p.strip_prefix(root).ok()?;
    Some(format!("{letter}:\\{}", rest.to_string_lossy().replace('/', "\\")))
}

/// `WINEDLLOVERRIDES` with Connect's executables disabled, keeping what was there.
pub fn dll_overrides_without_connect(existing: Option<&str>) -> String {
    let mut parts: Vec<String> = existing.into_iter().flat_map(|e| e.split(';')).filter(|p| !p.is_empty()).map(str::to_owned).collect();
    for exe in CONNECT_EXES {
        let name = exe.to_ascii_lowercase();
        let taken = parts.iter().any(|p| {
            p.split('=').next().is_some_and(|names| names.split(',').any(|n| n.trim().eq_ignore_ascii_case(&name)))
        });
        if !taken {
            parts.push(format!("{name}=d"));
        }
    }
    parts.join(";")
}

/// Saves folder the SDK uses, as Connect lays it out (R2 games keep `Name[Slot].save` files
/// there, R1 games `N.save`). Reusing it keeps the saves Connect-launched sessions wrote.
pub fn savegame_path(install_dir_win: Option<&str>, account_id: &str, product_id: u32) -> String {
    let base = install_dir_win.unwrap_or(CONNECT_WIN).trim_end_matches('\\');
    format!(r"{base}\savegames\{account_id}\{product_id}")
}

/// Processes (cmdline, environ, both NUL-separated as in /proc) that are Connect running in
/// this prefix. Its pipes would collide with the bridge's, and its login with quiverd's.
pub fn connect_in_prefix<'a>(procs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>, compat_data: &Path) -> Vec<String> {
    let compat = compat_data.to_string_lossy();
    let pfx = compat_data.join("pfx");
    let pfx = pfx.to_string_lossy();
    let wanted = [format!("STEAM_COMPAT_DATA_PATH={compat}"), format!("WINEPREFIX={pfx}"), format!("WINEPREFIX={pfx}/")];
    procs
        .into_iter()
        .filter(|(_, env)| env.split(|&b| b == 0).any(|kv| wanted.iter().any(|w| kv == w.as_bytes())))
        .filter_map(|(cmd, _)| {
            let cmd = String::from_utf8_lossy(cmd).replace('\0', " ");
            let lower = cmd.to_ascii_lowercase();
            CONNECT_EXES
                .iter()
                .filter(|e| !e.eq_ignore_ascii_case("UbisoftExtension.exe"))
                .any(|e| lower.contains(&e.to_ascii_lowercase()))
                .then(|| cmd.trim().to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The Black Flag Resynced install script, as Steam ships it (trimmed).
    const BF_SCRIPT: &str = r#""InstallScript"
{
	"Registry"
	{
		"HKEY_LOCAL_MACHINE\\SOFTWARE\\Ubisoft\\Launcher\\Installs\\66088"
		{
			"string"
			{
				"english" { "language" "en-US" }
			}
		}
	}
	"Run Process"
	{
		"UbisoftConnectInstaller"
		{
			"Process 1"		"%INSTALLDIR%\\UbisoftConnectInstaller.exe"
			"command 1"		"/S"
		}
	}
}
"#;

    /// The parts of Black Flag Resynced's compatdata system.reg that matter.
    const BF_REG: &str = r#"WINE REGISTRY Version 2
;; All keys relative to \\Machine

#arch=win64

[Software\\Wow6432Node\\Ubisoft\\Launcher] 1793940008
#time=1dd6da8e70d3932
"InstallDir"="C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\"
"Installer Language"="1033"
"Version"="13416"

[Software\\Wow6432Node\\Ubisoft\\Launcher\\Installs\\66088] 1783725491
#time=1dd10c26055eeea
"language"="en-US"

[Software\\Wow6432Node\\Valve\\Steam] 1794413522
#time=1dd71f76309ca4e
"InstallPath"="C:\\Program Files (x86)\\Steam"
"#;

    #[test]
    fn env_from_steam() {
        let vars = |k: &str| match k {
            "STEAM_COMPAT_DATA_PATH" => Some("/s/steamapps/compatdata/3751950".into()),
            "SteamAppId" => Some("3751950".into()),
            "STEAM_COMPAT_INSTALL_PATH" => Some("/s/steamapps/common/AC".into()),
            _ => None,
        };
        let e = SteamEnv::from_vars(vars).unwrap();
        assert_eq!(e.app_id, 3751950);
        assert_eq!(e.prefix(), Path::new("/s/steamapps/compatdata/3751950/pfx"));
        assert_eq!(e.installscript().unwrap(), Path::new("/s/steamapps/common/AC/installscript_3751950.vdf"));
        // No app id variable: the compatdata folder name.
        let e = SteamEnv::from_vars(|k| (k == "STEAM_COMPAT_DATA_PATH").then(|| "/x/compatdata/42".into())).unwrap();
        assert_eq!((e.app_id, e.install), (42, None));
        assert!(SteamEnv::from_vars(|_| None).unwrap_err().to_string().contains("steam-run %command%"));
    }

    #[test]
    fn product_mapping() {
        assert_eq!(product_from_installscript(BF_SCRIPT), Some(66088));
        assert_eq!(product_from_installscript(r#""InstallScript" { "Firewall" { "x" "y" } }"#), None);
        assert_eq!(registry_installs(BF_REG), vec![66088]);
        let user_reg = "[Software\\\\Ubisoft\\\\Launcher\\\\Installs\\\\66088] 1783725612\n\"InstallState\"=dword:00000001\n";
        assert_eq!(registry_installs(user_reg), vec![66088]);

        assert_eq!(resolve_product(1, Some(5), Some(BF_SCRIPT), None).unwrap(), (5, ProductSource::Override));
        assert_eq!(resolve_product(1, None, Some(BF_SCRIPT), None).unwrap(), (66088, ProductSource::InstallScript));
        assert_eq!(resolve_product(1, None, Some("garbage {"), Some(BF_REG)).unwrap(), (66088, ProductSource::Registry));
        assert_eq!(resolve_product(3751950, None, None, None).unwrap(), (66088, ProductSource::Known));
        // Two products in one prefix: ambiguous, not guessed.
        let two = format!("{BF_REG}\n[Software\\\\Wow6432Node\\\\Ubisoft\\\\Launcher\\\\Installs\\\\3539] 1\n");
        assert_eq!(registry_installs(&two), vec![3539, 66088]);
        assert!(resolve_product(7, None, None, Some(&two)).unwrap_err().to_string().contains("--product"));
    }

    #[test]
    fn registry_read() {
        let dir = reg_value(BF_REG, LAUNCHER_KEY, "InstallDir").unwrap();
        assert_eq!(dir, r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\");
        assert_eq!(reg_value(BF_REG, &LAUNCHER_KEY.to_lowercase(), "installdir").as_deref(), Some(dir.as_str()));
        assert_eq!(reg_value(BF_REG, LAUNCHER_KEY, "Version").as_deref(), Some("13416"));
        // Values of other keys don't leak in.
        assert_eq!(reg_value(BF_REG, LAUNCHER_KEY, "InstallPath"), None);
        assert_eq!(reg_value(BF_REG, r"Software\Nope", "InstallDir"), None);
        assert_eq!(
            host_path(Path::new("/c/pfx"), &dir).unwrap(),
            Path::new("/c/pfx/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher")
        );
        assert_eq!(host_path(Path::new("/c/pfx"), r"Z:\home\u\sdk").unwrap(), Path::new("/home/u/sdk"));
        assert_eq!(host_path(Path::new("/c/pfx"), r"D:\x"), None);
    }

    #[test]
    fn registry_edit() {
        // Replace in place; everything else byte-for-byte unchanged.
        let out = set_reg_value(BF_REG, LAUNCHER_KEY, "InstallDir", r"Z:\home\u\.local\share\quiver\sdk\", 9);
        assert_eq!(reg_value(&out, LAUNCHER_KEY, "InstallDir").as_deref(), Some(r"Z:\home\u\.local\share\quiver\sdk\"));
        assert_eq!(out.lines().count(), BF_REG.lines().count());
        let changed: Vec<_> = BF_REG.lines().zip(out.lines()).filter(|(a, b)| a != b).collect();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].1, r#""InstallDir"="Z:\\home\\u\\.local\\share\\quiver\\sdk\\""#);
        // Setting it back restores the original exactly.
        let back = set_reg_value(&out, LAUNCHER_KEY, "InstallDir", r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\", 9);
        assert_eq!(back, BF_REG);
        // A missing value goes after the key's #time line.
        let added = set_reg_value(BF_REG, r"Software\Wow6432Node\Valve\Steam", "Extra", "a\"b", 9);
        assert_eq!(reg_value(&added, r"Software\Wow6432Node\Valve\Steam", "Extra").as_deref(), Some("a\"b"));
        assert!(added.contains("#time=1dd71f76309ca4e\n\"Extra\"=\"a\\\"b\"\n\"InstallPath\""));
        // A missing key is appended, with a timestamp.
        let fresh = set_reg_value("WINE REGISTRY Version 2\n", LAUNCHER_KEY, "InstallDir", r"C:\x\", 1794000000);
        assert!(fresh.contains("[Software\\\\Wow6432Node\\\\Ubisoft\\\\Launcher] 1794000000\n\"InstallDir\"=\"C:\\\\x\\\\\"\n"));
        assert_eq!(reg_value(&fresh, LAUNCHER_KEY, "InstallDir").as_deref(), Some(r"C:\x\"));
    }

    #[test]
    fn sdk_check() {
        let tmp = tempfile::tempdir().unwrap();
        let pfx = tmp.path().join("pfx");
        let dir = pfx.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("uplay_r264.dll"), b"x").unwrap();
        let c = check_sdk(&pfx, BF_REG);
        assert!(c.has_r2 && !c.has_r1);
        assert_eq!(c.host_dir.unwrap(), dir);
        let c = check_sdk(&pfx, "");
        assert_eq!((c.install_dir, c.has_r2), (None, false));
    }

    /// `%command%` for a Proton game under the Steam Linux Runtime, as Steam passes it.
    fn steam_argv() -> Vec<String> {
        s(&[
            "/s/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=3751950",
            "--",
            "/s/ubuntu12_32/steam-launch-wrapper",
            "--",
            "/s/steamapps/common/SteamLinuxRuntime_sniper/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/s/steamapps/common/Proton - Experimental/proton",
            "waitforexitandrun",
            "/s/steamapps/common/Assassin's Creed Black Flag Resynced/ACBlackFlag.exe",
            "-uplay_steam_mode",
        ])
    }

    #[test]
    fn command_rewrite() {
        let argv = steam_argv();
        let pc = ProtonCommand::find(&argv).unwrap();
        assert_eq!(pc, ProtonCommand { proton: 9, verb: 10, exe: 11 });
        assert!(pc.exe(&argv).ends_with("ACBlackFlag.exe"));
        assert_eq!(pc.game_args(&argv), ["-uplay_steam_mode"]);
        let game_win = z_path(Path::new(pc.exe(&argv)));
        assert_eq!(game_win, r"Z:\s\steamapps\common\Assassin's Creed Black Flag Resynced\ACBlackFlag.exe");
        let bridge_args =
            quiver_ipc::run::BridgeArgs::run_command(r"Z:\q\context.json", Some(r"Z:\q\bridge.log"), &game_win, pc.game_args(&argv));
        let out = pc.rewrite(&argv, "/q/quiver-bridge.exe", &bridge_args);
        assert_eq!(out[..11], argv[..11]);
        assert_eq!(
            out[11..],
            s(&[
                "/q/quiver-bridge.exe",
                "--run",
                r"Z:\q\context.json",
                "--log",
                r"Z:\q\bridge.log",
                "--",
                r"Z:\s\steamapps\common\Assassin's Creed Black Flag Resynced\ACBlackFlag.exe",
                "-uplay_steam_mode",
            ])[..]
        );
        // What the bridge will see after Proton strips its own part.
        assert_eq!(
            quiver_ipc::run::BridgeArgs::parse(&out[12..]).unwrap(),
            quiver_ipc::run::BridgeArgs::Run {
                context: r"Z:\q\context.json".into(),
                log: Some(r"Z:\q\bridge.log".into()),
                exe: game_win,
                args: s(&["-uplay_steam_mode"]),
            }
        );
        // Without the runtime container (older Proton) it's found the same way.
        let plain = s(&["/p/proton", "waitforexitandrun", "/g/game.exe"]);
        assert_eq!(ProtonCommand::find(&plain).unwrap().exe, 2);
        // A native game, or Proton without an exe.
        assert!(ProtonCommand::find(&s(&["/g/game.x86_64"])).is_err());
        assert!(ProtonCommand::find(&s(&["/p/proton", "waitforexitandrun"])).is_err());
        // A directory named proton elsewhere doesn't confuse it.
        let tricky = s(&["/x/proton", "--flag", "/p/proton", "run", "/g/a.exe"]);
        assert_eq!(ProtonCommand::find(&tricky).unwrap().proton, 2);
    }

    #[test]
    fn dll_overrides() {
        let o = dll_overrides_without_connect(None);
        assert!(o.starts_with("ubisoftgamelauncher.exe=d;ubisoftgamelauncher64.exe=d;"));
        assert_eq!(o.split(';').count(), CONNECT_EXES.len());
        // The user's own overrides stay first; one they set for a Connect exe wins.
        let o = dll_overrides_without_connect(Some("winhttp=n,b;upc.exe=n"));
        assert!(o.starts_with("winhttp=n,b;upc.exe=n;"));
        assert_eq!(o.matches("upc.exe").count(), 1);
        let o = dll_overrides_without_connect(Some("dxgi,UbisoftConnect.exe=b"));
        assert!(!o.contains("ubisoftconnect.exe=d"));
    }

    #[test]
    fn saves_path() {
        assert_eq!(
            savegame_path(Some(r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\"), "99ef", 66088),
            r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\savegames\99ef\66088"
        );
        assert_eq!(savegame_path(None, "a", 1), format!(r"{CONNECT_WIN}\savegames\a\1"));
    }

    #[test]
    fn connect_detection() {
        let compat = Path::new("/s/compatdata/3751950");
        let env_this = b"HOME=/h\0STEAM_COMPAT_DATA_PATH=/s/compatdata/3751950\0".as_slice();
        let env_wine = b"WINEPREFIX=/s/compatdata/3751950/pfx\0".as_slice();
        let env_other = b"STEAM_COMPAT_DATA_PATH=/s/compatdata/1\0".as_slice();
        let procs: Vec<(&[u8], &[u8])> = vec![
            (b"C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\UbisoftConnect.exe\0-upc_uplay_id\0", env_this),
            (b"C:\\...\\upc.exe\0", env_wine),
            (b"C:\\...\\UbisoftConnect.exe\0", env_other),
            (b"C:\\...\\ACBlackFlag.exe\0", env_this),
            // The COM helper alone doesn't hold pipes or a login.
            (b"C:\\...\\UbisoftExtension.exe\0", env_this),
        ];
        let found = connect_in_prefix(procs, compat);
        assert_eq!(found.len(), 2);
        assert!(found[0].contains("UbisoftConnect.exe -upc_uplay_id"));
        assert!(found[1].contains("upc.exe"));
    }

    #[test]
    fn prefix_win_path_uses_the_deepest_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let steam = root.join("Steam");
        let game = steam.join("steamapps/common/Game Name");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("game.exe"), b"").unwrap();
        let pfx = root.join("pfx");
        std::fs::create_dir_all(pfx.join("dosdevices")).unwrap();
        std::fs::create_dir_all(pfx.join("drive_c")).unwrap();
        std::os::unix::fs::symlink("../drive_c", pfx.join("dosdevices/c:")).unwrap();
        std::os::unix::fs::symlink(&steam, pfx.join("dosdevices/s:")).unwrap();
        std::os::unix::fs::symlink("/", pfx.join("dosdevices/z:")).unwrap();
        std::os::unix::fs::symlink("/dev/ttyS0", pfx.join("dosdevices/com1")).unwrap();
        assert_eq!(prefix_win_path(&pfx, &game.join("game.exe")), r"S:\steamapps\common\Game Name\game.exe");
        assert_eq!(prefix_win_path(&pfx, &pfx.join("drive_c/x.txt")), r"C:\x.txt");
        let other = root.join("elsewhere/f");
        assert_eq!(prefix_win_path(&pfx, &other), z_path(&other));
        // No dosdevices: Z:.
        assert_eq!(prefix_win_path(&root.join("nope"), &game), z_path(&game));
    }
}
