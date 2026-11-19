//! The EOS filter (design/LAB.md section 3.1): a `secur32.dll` put next to an Epic game's
//! executable and EOS SDK in a lab instance's *cloned* install, loaded with `secur32=n,b`.
//! Games on the EOS SDK talk to Epic themselves (libcurl over Windows schannel), all on one
//! host, `api.epicgames.dev`: sign-in, ownership, and also cloud saves, achievements, stats
//! and telemetry. The DLL sees each request in plaintext before it is encrypted and lets
//! through only the allowlist (`contrib/lab/eos-filter/allowlist.h`: sign-in, the SDK's
//! config, Connect, ownership and entitlement reads); every other request to that host fails
//! before a byte of it is sent. Other hosts pass untouched.
//!
//! The C is in `contrib/lab/eos-filter` (`build.sh` makes both architectures; installed as
//! `<libdir>/lab/eos-filter/<arch>/secur32.dll`). This module finds the build, decides where
//! an instance needs it, installs it there (only ever inside the instance's folder), and
//! reads its log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The DLL's file name, in the build and in the instance.
pub const DLL: &str = "secur32.dll";
/// Every build carries this string (`shim.c`); a `secur32.dll` without it isn't ours.
pub const MARKER: &[u8] = b"QUIVER-EOS-FILTER/1";
/// The filter's log in the run folder, and the variable that names it for the DLL.
pub const LOG_FILE: &str = "eos-filter.log";
pub const LOG_ENV: &str = "QUIVER_EOS_FILTER_LOG";
/// Where the build is looked for first (a folder holding `<arch>/secur32.dll`).
pub const DIR_ENV: &str = "QUIVER_EOS_FILTER_DIR";
/// The DLL override the launch sets.
pub const OVERRIDE: &str = "secur32=n,b";
/// The allowlist, as the DLL is built with it.
pub const ALLOWLIST_H: &str = include_str!("../../../../contrib/lab/eos-filter/allowlist.h");

/// The EOS SDK's DLLs.
const SDK_DLLS: &[(&str, Arch)] = &[("eossdk-win64-shipping.dll", Arch::X86_64), ("eossdk-win32-shipping.dll", Arch::I686)];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arch {
    X86_64,
    I686,
}

impl Arch {
    /// The build's folder for it.
    pub fn dir(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::I686 => "i686",
        }
    }
    fn of_pe(pe: &super::facts::Pe) -> Option<Arch> {
        match pe.arch.as_str() {
            "x64" => Some(Arch::X86_64),
            "x86" => Some(Arch::I686),
            _ => None,
        }
    }
}

/// Is this file a build of the filter?
pub fn is_filter(bytes: &[u8]) -> bool {
    bytes.windows(MARKER.len()).any(|w| w == MARKER)
}

/// The allowlist's entries (method, pattern, what it is), read from `allowlist.h`.
pub fn allowlist() -> Vec<(String, String, String)> {
    let body = ALLOWLIST_H.split_once("EF_ALLOWLIST[] = {").map(|(_, b)| b).unwrap_or_default();
    let body = body.split_once("};").map(|(b, _)| b).unwrap_or_default();
    body.lines()
        .filter_map(|l| {
            let l = l.trim();
            let inner = l.strip_prefix('{')?.trim_end_matches(',').strip_suffix('}')?;
            let q: Vec<&str> = inner.split('"').collect();
            // { "M", "P", "W" } -> ["", M, ", ", P, ", ", W, ""]
            (q.len() == 7).then(|| (q[1].to_owned(), q[3].to_owned(), q[5].to_owned()))
        })
        .collect()
}

/// The hosts the filter applies to.
pub fn filtered_hosts() -> Vec<String> {
    let body = ALLOWLIST_H.split_once("EF_FILTERED_HOSTS[] = {").map(|(_, b)| b).unwrap_or_default();
    let body = body.split_once("};").map(|(b, _)| b).unwrap_or_default();
    body.split('"').skip(1).step_by(2).map(str::to_owned).collect()
}

/// The EOS SDK DLLs in an install (relative paths), with their architectures.
pub fn eos_sdks(install: &Path) -> Vec<(PathBuf, Arch)> {
    let mut out: Vec<(PathBuf, Arch)> = super::facts::walk(install, 10, 200_000)
        .into_iter()
        .filter(|(_, d)| !d)
        .filter_map(|(p, _)| {
            let n = p.file_name()?.to_string_lossy().to_ascii_lowercase();
            SDK_DLLS.iter().find(|(dll, _)| *dll == n).map(|(_, a)| (p, *a))
        })
        .collect();
    out.sort();
    out
}

/// Where an instance gets the filter.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The game ships the EOS SDK: a launch without the filter is refused.
    pub required: bool,
    /// The SDK DLLs found (relative to the install).
    pub sdks: Vec<PathBuf>,
    /// Folders (relative to the install) that get a `secur32.dll`, and for which
    /// architecture: the game executable's (where Windows looks first) and each SDK DLL's.
    pub dirs: Vec<(PathBuf, Arch)>,
}

/// Where the filter goes for a game, or `None` when it needs none (not an Epic game and no
/// EOS SDK in it). Epic games get it whether or not the SDK was found.
pub fn plan(install: &Path, exe: &str, epic: bool) -> Option<Plan> {
    let sdks = eos_sdks(install);
    if sdks.is_empty() && !epic {
        return None;
    }
    let exe_dir = Path::new(exe).parent().map(Path::to_path_buf).unwrap_or_default();
    let exe_arch = std::fs::read(install.join(exe)).ok().and_then(|b| super::facts::parse_pe(&b)).and_then(|pe| Arch::of_pe(&pe));
    let mut dirs: Vec<(PathBuf, Arch)> = Vec::new();
    let mut add = |d: PathBuf, a: Arch| {
        if !dirs.iter().any(|(x, _)| *x == d) {
            dirs.push((d, a));
        }
    };
    add(exe_dir, exe_arch.or(sdks.first().map(|s| s.1)).unwrap_or(Arch::X86_64));
    for (p, a) in &sdks {
        add(p.parent().map(Path::to_path_buf).unwrap_or_default(), *a);
    }
    Some(Plan { required: !sdks.is_empty(), sdks: sdks.into_iter().map(|s| s.0).collect(), dirs })
}

/// Where the build may be, best first, for a binary in `exe_dir`: `$QUIVER_EOS_FILTER_DIR`,
/// the install (`<prefix>/lib/quiver/lab/eos-filter` from `<prefix>/bin`), then this
/// checkout's (`contrib/lab/eos-filter/build.sh target/eos-filter`).
pub fn build_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = std::env::var_os(DIR_ENV).filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(d));
    }
    if let Some(prefix) = exe_dir.and_then(Path::parent) {
        out.push(prefix.join("lib/quiver/lab/eos-filter"));
    }
    out.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/eos-filter"));
    out
}

/// Is `dir` a complete build (both architectures, both carrying the marker)?
fn complete(dir: &Path) -> bool {
    [Arch::X86_64, Arch::I686].iter().all(|a| std::fs::read(dir.join(a.dir()).join(DLL)).is_ok_and(|b| is_filter(&b)))
}

/// The filter's build for this Quiver.
pub fn find_build() -> Result<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let candidates = build_candidates(exe_dir.as_deref());
    candidates.iter().find(|d| complete(d)).cloned().with_context(|| {
        let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
        format!(
            "the EOS filter isn't installed (looked in {}); tools/install-user.sh builds and installs it (needs mingw-w64-gcc), or run contrib/lab/eos-filter/build.sh target/eos-filter in the checkout",
            tried.join(", ")
        )
    })
}

/// Put the filter in the instance's install clone, where `plan` says. Refuses any folder
/// that resolves outside `instance_dir` (a symlink in the clone back to the person's own
/// files), and any `secur32.dll` already there that isn't a filter build (the game's, or a
/// mod's: the filter must be the one Windows finds). Returns the DLLs written or already
/// current.
pub fn install(instance_dir: &Path, install_clone: &Path, plan: &Plan, build: &Path) -> Result<Vec<PathBuf>> {
    let root = instance_dir.canonicalize().with_context(|| format!("instance folder {}", instance_dir.display()))?;
    let mut out = Vec::new();
    for (rel, arch) in &plan.dirs {
        let dir = install_clone.join(rel);
        let dir = dir.canonicalize().with_context(|| format!("{} (where the game's files should be)", dir.display()))?;
        if !dir.starts_with(&root) {
            bail!("{} resolves to {}, outside the instance; the EOS filter is only ever installed in the instance's own clone", install_clone.join(rel).display(), dir.display());
        }
        let want = std::fs::read(build.join(arch.dir()).join(DLL)).with_context(|| format!("read the {} EOS filter build", arch.dir()))?;
        if !is_filter(&want) {
            bail!("{} isn't an EOS filter build", build.join(arch.dir()).join(DLL).display());
        }
        let dst = dir.join(DLL);
        // Windows finds names case-insensitively: every spelling in the folder must be ours.
        let mut current = false;
        for e in std::fs::read_dir(&dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.eq_ignore_ascii_case(DLL) {
                continue;
            }
            let md = std::fs::symlink_metadata(e.path())?;
            let bytes = if md.is_file() { std::fs::read(e.path())? } else { Vec::new() };
            if !md.is_file() || !is_filter(&bytes) {
                bail!("{} is already there and isn't Quiver's EOS filter (the game's own, or a mod's); the filter must be the secur32.dll the game loads", e.path().display());
            }
            if name != DLL {
                std::fs::remove_file(e.path())?;
            } else if bytes == want {
                current = true;
            }
        }
        if !current {
            let tmp = dir.join(format!(".{DLL}.quiver-new.{}", std::process::id()));
            std::fs::write(&tmp, &want).with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &dst).with_context(|| format!("install {}", dst.display()))?;
        }
        if std::fs::read(&dst)? != want {
            bail!("{} doesn't hold the EOS filter after installing it", dst.display());
        }
        out.push(dst);
    }
    Ok(out)
}

/// `WINEDLLOVERRIDES` with the filter's override, replacing any other `secur32` setting.
pub fn with_override(existing: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for p in existing.into_iter().flat_map(|e| e.split(';')).filter(|p| !p.trim().is_empty()) {
        let Some((names, how)) = p.split_once('=') else {
            parts.push(p.to_owned());
            continue;
        };
        let keep: Vec<&str> = names.split(',').filter(|n| !n.trim().eq_ignore_ascii_case("secur32") && !n.trim().eq_ignore_ascii_case("secur32.dll")).collect();
        if !keep.is_empty() {
            parts.push(format!("{}={how}", keep.join(",")));
        }
    }
    parts.push(OVERRIDE.to_owned());
    parts.join(";")
}

/// What a filter log says (`eos-filter.log`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub loaded: usize,
    pub allowed: usize,
    pub blocked: usize,
    /// "METHOD target" of blocked requests, with how often.
    pub blocked_requests: BTreeMap<String, usize>,
}

pub fn summarize(log: &str) -> Summary {
    let mut s = Summary::default();
    for line in log.lines() {
        let Some((_, rest)) = line.split_once(' ') else { continue };
        if rest.starts_with("--- EOS filter") && rest.contains(" loaded ") {
            s.loaded += 1;
        } else if let Some(r) = rest.strip_prefix("allow ") {
            if !r.starts_with(|c: char| c.is_ascii_digit()) {
                s.allowed += 1;
            }
        } else if let Some(r) = rest.strip_prefix("block ") {
            s.blocked += 1;
            let what = r.split(" -- ").next().unwrap_or(r).to_owned();
            *s.blocked_requests.entry(what).or_default() += 1;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_build(dir: &Path) {
        for a in ["x86_64", "i686"] {
            std::fs::create_dir_all(dir.join(a)).unwrap();
            std::fs::write(dir.join(a).join(DLL), format!("MZ...QUIVER-EOS-FILTER/1 {a}...")).unwrap();
        }
    }

    #[test]
    fn the_allowlist_reads_from_the_header() {
        let a = allowlist();
        assert!(a.len() >= 10, "{a:?}");
        assert!(a.iter().any(|(m, p, _)| m == "POST" && p == "/epic/oauth/{v}/token"));
        assert!(a.iter().any(|(m, p, _)| m == "GET" && p == "/sdk/v1/product/{}"));
        // Nothing that writes to the account or reports anything.
        for (_, p, w) in &a {
            for bad in ["playerdata", "achievement", "stats", "leaderboard", "telemetry", "redeem", "presence", "/auth/v1/users", "/auth/v1/accounts"] {
                assert!(!p.contains(bad), "{p}");
            }
            assert!(!w.is_empty());
        }
        assert_eq!(filtered_hosts(), ["api.epicgames.dev"]);
    }

    #[test]
    fn plans_follow_the_sdk_and_the_exe() {
        let t = tempfile::tempdir().unwrap();
        let i = t.path();
        std::fs::create_dir_all(i.join("Game/Binaries/Win64")).unwrap();
        std::fs::create_dir_all(i.join("Engine/Binaries/ThirdParty/EOSSDK/Win64")).unwrap();
        std::fs::write(i.join("Game/Binaries/Win64/Game-Win64-Shipping.exe"), super::super::facts::tests::fake_pe(&["kernel32.dll"], None)).unwrap();
        std::fs::write(i.join("Engine/Binaries/ThirdParty/EOSSDK/Win64/EOSSDK-Win64-Shipping.dll"), b"").unwrap();
        let p = plan(i, "Game/Binaries/Win64/Game-Win64-Shipping.exe", true).unwrap();
        assert!(p.required);
        assert_eq!(p.dirs, [(PathBuf::from("Game/Binaries/Win64"), Arch::X86_64), (PathBuf::from("Engine/Binaries/ThirdParty/EOSSDK/Win64"), Arch::X86_64)]);
        // AWR's layout: the exe and the SDK side by side, one folder.
        let t2 = tempfile::tempdir().unwrap();
        std::fs::write(t2.path().join("EOSSDK-Win64-Shipping.dll"), b"").unwrap();
        let p = plan(t2.path(), "Game_f_x64_EOS.exe", true).unwrap();
        assert_eq!(p.dirs, [(PathBuf::new(), Arch::X86_64)]);
        // An Epic game without the SDK still gets it (not required); others don't need it.
        let t3 = tempfile::tempdir().unwrap();
        assert!(!plan(t3.path(), "x.exe", true).unwrap().required);
        assert!(plan(t3.path(), "x.exe", false).is_none());
        // A 32-bit SDK in a non-Epic game: required, 32-bit.
        std::fs::write(t3.path().join("EOSSDK-Win32-Shipping.dll"), b"").unwrap();
        let p = plan(t3.path(), "x.exe", false).unwrap();
        assert!(p.required && p.dirs == [(PathBuf::new(), Arch::I686)]);
    }

    #[test]
    fn installs_only_inside_the_instance() {
        let t = tempfile::tempdir().unwrap();
        let build = t.path().join("build");
        fake_build(&build);
        assert!(complete(&build));
        let inst = t.path().join("instances/awr");
        let clone = inst.join("install");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("EOSSDK-Win64-Shipping.dll"), b"").unwrap();
        let p = plan(&clone, "Game.exe", true).unwrap();
        let got = install(&inst, &clone, &p, &build).unwrap();
        assert_eq!(got.len(), 1);
        assert!(std::fs::read(clone.join(DLL)).unwrap().ends_with(b"x86_64..."));
        // Again: already current, nothing rewritten.
        assert_eq!(install(&inst, &clone, &p, &build).unwrap().len(), 1);
        // A secur32.dll that isn't ours (a mod's, the game's): refused, and left alone.
        std::fs::write(clone.join(DLL), b"MZ a mod's secur32").unwrap();
        let e = install(&inst, &clone, &p, &build).unwrap_err().to_string();
        assert!(e.contains("isn't Quiver's EOS filter"), "{e}");
        assert_eq!(std::fs::read(clone.join(DLL)).unwrap(), b"MZ a mod's secur32");
        std::fs::remove_file(clone.join(DLL)).unwrap();
        // Another spelling of an old build is replaced by the right name.
        std::fs::write(clone.join("SECUR32.DLL"), b"QUIVER-EOS-FILTER/1 old").unwrap();
        install(&inst, &clone, &p, &build).unwrap();
        let names: Vec<String> = std::fs::read_dir(&clone).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.eq_ignore_ascii_case(DLL)).collect();
        assert_eq!(names, [DLL]);
        // A folder of the clone that is a link back to the person's own install: refused.
        let real = t.path().join("persons-real-install");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, clone.join("linked")).unwrap();
        let p2 = Plan { required: true, sdks: vec![], dirs: vec![(PathBuf::from("linked"), Arch::X86_64)] };
        let e = install(&inst, &clone, &p2, &build).unwrap_err().to_string();
        assert!(e.contains("outside the instance"), "{e}");
        assert!(std::fs::read_dir(&real).unwrap().next().is_none(), "nothing written in the person's folder");
        // A symlink named secur32.dll: refused (writing through it would land elsewhere).
        let t2 = tempfile::tempdir().unwrap();
        let inst2 = t2.path().join("i");
        std::fs::create_dir_all(&inst2).unwrap();
        std::os::unix::fs::symlink(real.join("x.dll"), inst2.join(DLL)).unwrap();
        let p3 = Plan { required: true, sdks: vec![], dirs: vec![(PathBuf::new(), Arch::X86_64)] };
        assert!(install(&inst2, &inst2, &p3, &build).is_err());
        assert!(!real.join("x.dll").exists());
    }

    #[test]
    fn overrides() {
        assert_eq!(with_override(None), "secur32=n,b");
        assert_eq!(with_override(Some("dxgi=n,b;secur32=b")), "dxgi=n,b;secur32=n,b");
        assert_eq!(with_override(Some("dxgi,Secur32=n;;x")), "dxgi=n;x;secur32=n,b");
    }

    #[test]
    fn candidates_and_summaries() {
        let c = build_candidates(Some(Path::new("/usr/bin")));
        assert!(c.contains(&PathBuf::from("/usr/lib/quiver/lab/eos-filter")), "{c:?}");
        let log = "\
2026-11-18T10:00:00.000Z --- EOS filter (x86_64) loaded in pid 32 C:\\g\\game.exe
2026-11-18T10:00:01.000Z allow GET /sdk/v1/product/p?platformId=\u{2026} -- SDK config (product)
2026-11-18T10:00:02.000Z allow POST /epic/oauth/v1/token -- Epic account sign-in (token)
2026-11-18T10:00:03.000Z block PUT /playerdata/v1/d/p/data/s -- not on the allowlist
2026-11-18T10:00:04.000Z block PUT /playerdata/v1/d/p/data/s -- not on the allowlist
2026-11-18T10:00:05.000Z block POST /telemetry/data/datarouter/api/v1/public/data -- not on the allowlist
";
        let s = summarize(log);
        assert_eq!((s.loaded, s.allowed, s.blocked), (1, 2, 3));
        assert_eq!(s.blocked_requests["PUT /playerdata/v1/d/p/data/s"], 2);
    }
}
