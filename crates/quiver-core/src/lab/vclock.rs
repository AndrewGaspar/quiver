//! The lab's fixed-step virtual clock (design/LAB.md, "The virtual clock"): an instance's game
//! is told every frame took exactly `1/rate` s. Faster than real time when frames are cheap,
//! and repeatable when the machine is busy (below the rate the game runs slower than real time
//! and behaves the same).
//!
//! Off by default. On (`quiver lab edit <i> clock=virtual:62.5`, or `quiver lab launch <i>
//! --clock virtual:62.5` for one launch), each launch:
//!
//! - removes any copy of the hook DLL an earlier launch left in the clone (it carries
//!   [`quiver_vclock::MARKER`]);
//! - picks a system DLL the game imports and doesn't ship ([`quiver_vclock::PROXIES`]: version,
//!   winmm, dinput8, dsound) and writes the hook DLL under that name next to the executable,
//!   in the instance's install clone only (checked), for the executable's architecture;
//! - sets that DLL native-then-builtin for the launch, the clock's environment, and (unless
//!   `clock.uncap=false`) uncapped presentation: DXVK's sync interval 0 and no frame-rate caps,
//!   vkd3d-proton's and the GL drivers' too; the container's gamescope boots at
//!   [`UNCAPPED_REFRESH`] Hz ([`boot_refresh`]).
//!
//! The DLL writes `vclock.status` and `vclock.log` into the run folder; [`report_lines`] is what
//! `quiver lab status` and `logs` print.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Inst;
use super::manifest::Manifest;

pub use quiver_vclock::{LOG_FILE, Report, STATUS_FILE, control, unavailable};

/// gamescope's refresh while the clock is on: high enough not to pace an uncapped game.
pub const UNCAPPED_REFRESH: u32 = 1000;

/// Where `prepare` records the DLL it placed (removed before the next launch).
const RECORD: &str = "vclock-files.txt";

/// The manifest's `[clock]`: present means the virtual clock is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualClock {
    /// Virtual frames per second. Rates whose period is a whole number of milliseconds (125,
    /// 100, 62.5, 50, 40, 31.25, 25, 20) are exact for games that time frames in ms too.
    pub rate: f64,
    /// More of the game's modules whose clock imports go virtual (the executable always does):
    /// an engine DLL that does the game's timing, e.g. `UnityPlayer.dll` (added by itself when
    /// it is next to the executable and this is empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,
    /// The system DLL to stand in for (`version`, `winmm`, `dinput8`, `dsound`); by default the
    /// first the game imports and doesn't ship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Clock reads in one frame before time creeps (default 4096).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creep_after: Option<u32>,
    /// Real milliseconds a frame may take before it is released to real time (default 1000).
    /// A released frame's timing isn't repeatable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_ms: Option<u32>,
    /// The longest real sleep a shortened Sleep gets while the clock runs (default 0: a yield).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_max_ms: Option<u32>,
    /// Whose sleeps get shorter: `present` (default: the thread that presents, so a frame
    /// limiter that sleeps ends fast), `all` (every thread: background threads that sleep-poll
    /// then spin and read the clock thousands of times a frame), or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleeps: Option<String>,
    /// Uncapped presentation (sync interval 0, no frame-rate caps, gamescope at 1000 Hz).
    /// Off: the game presents as it would, and runs at most at the display's rate.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub uncap: bool,
}

fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

impl VirtualClock {
    pub fn new(rate: f64) -> Self {
        Self { rate, modules: Vec::new(), proxy: None, creep_after: None, stuck_ms: None, sleep_max_ms: None, sleeps: None, uncap: true }
    }

    pub fn describe(&self) -> String {
        format!("virtual:{}", self.rate)
    }
}

/// `virtual:62.5` (or `virtual:62.5Hz`): on at that rate; `real`: off.
pub fn parse_spec(s: &str) -> Result<Option<f64>> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("real") || t.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let Some(rate) = t.strip_prefix("virtual:") else {
        bail!("'{s}': the clock is `real` or `virtual:<Hz>` (e.g. virtual:62.5)");
    };
    quiver_vclock::parse_rate(rate).map(Some).map_err(anyhow::Error::msg)
}

/// `quiver lab edit` for `clock=…` and `clock.<field>=…`. Ok(false): not a clock key.
pub fn edit(m: &mut Manifest, key: &str, value: &str) -> Result<bool> {
    if key == "clock" {
        m.clock = match parse_spec(value)? {
            None => None,
            Some(rate) => Some(VirtualClock { rate, ..m.clock.clone().unwrap_or_else(|| VirtualClock::new(rate)) }),
        };
        return Ok(true);
    }
    let Some(field) = key.strip_prefix("clock.") else { return Ok(false) };
    let c = m.clock.as_mut().with_context(|| format!("the virtual clock is off: set clock=virtual:<Hz> before {key}"))?;
    let num = |v: &str| -> Result<Option<u32>> { if v.is_empty() { Ok(None) } else { Ok(Some(v.parse().with_context(|| format!("{key}: a number"))?)) } };
    match field {
        "modules" => c.modules = value.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
        "proxy" => {
            let v = value.trim().trim_end_matches(".dll").to_ascii_lowercase();
            c.proxy = match v.as_str() {
                "" | "auto" => None,
                p if quiver_vclock::PROXIES.contains(&p) => Some(p.to_owned()),
                _ => bail!("clock.proxy: one of {} (or auto)", quiver_vclock::PROXIES.join(", ")),
            }
        }
        "creep_after" => c.creep_after = num(value)?,
        "stuck_ms" => c.stuck_ms = num(value)?,
        "sleep_max_ms" => c.sleep_max_ms = num(value)?,
        "sleeps" => {
            c.sleeps = match value.trim() {
                "" | "present" => None,
                v @ ("all" | "none") => Some(v.to_owned()),
                _ => bail!("clock.sleeps: present (the presenting thread's sleeps get shorter), all or none"),
            }
        }
        "uncap" => c.uncap = !matches!(value, "false" | "0" | "no" | "off"),
        _ => bail!("unknown clock key '{key}' (clock.modules, clock.proxy, clock.creep_after, clock.stuck_ms, clock.sleep_max_ms, clock.sleeps, clock.uncap)"),
    }
    Ok(true)
}

/// gamescope's refresh for the instance's container.
pub fn boot_refresh(m: &Manifest) -> u32 {
    match &m.clock {
        Some(c) if c.uncap => m.display.refresh.max(UNCAPPED_REFRESH),
        _ => m.display.refresh,
    }
}

/// The clock this launch runs with: the launch's `--clock` if given, else the manifest's.
pub fn effective(m: &Manifest, launch: Option<&str>) -> Result<Option<VirtualClock>> {
    let Some(spec) = launch else { return Ok(m.clock.clone()) };
    Ok(parse_spec(spec)?.map(|rate| VirtualClock { rate, ..m.clock.clone().unwrap_or_else(|| VirtualClock::new(rate)) }))
}

/// The executable's folder in the instance's install clone, refusing anything outside the
/// instance (the person's own install is never written).
fn exe_dir(inst: &Inst, m: &Manifest) -> Result<PathBuf> {
    let exe = m.install_in(inst).join(&m.paths.exe);
    let dir = exe.parent().context("the executable has no folder")?.to_path_buf();
    let real = dir.canonicalize().with_context(|| format!("{} not found", dir.display()))?;
    let root = inst.dir.canonicalize().with_context(|| format!("{} not found", inst.dir.display()))?;
    if !real.starts_with(&root) {
        bail!("refusing to write the clock's DLL to {}: it isn't inside the instance ({})", real.display(), root.display());
    }
    Ok(real)
}

fn has_file(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name)))
}

/// Remove copies of the hook DLL earlier launches left (recorded, or any proxy name in the
/// executable's folder that carries the marker). Returns what was removed.
pub fn clean(inst: &Inst, m: &Manifest) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_to_string(inst.dir.join(RECORD)).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    if let Ok(dir) = exe_dir(inst, m) {
        for p in quiver_vclock::PROXIES {
            if let Some(f) = has_file(&dir, &format!("{p}.dll")) {
                candidates.push(f);
            }
        }
    }
    let mut removed = Vec::new();
    for f in candidates {
        if std::fs::read(&f).is_ok_and(|b| quiver_vclock::is_ours(&b)) && std::fs::remove_file(&f).is_ok() {
            removed.push(f);
        }
    }
    let _ = std::fs::remove_file(inst.dir.join(RECORD));
    removed.sort();
    removed.dedup();
    removed
}

/// The proxy for this game: the explicit one, or the first of [`quiver_vclock::PROXIES`] the
/// executable (or a DLL beside it) imports and the folder (or the mod) doesn't ship.
fn choose_proxy(c: &VirtualClock, dir: &Path, exe_imports: &[String], mod_dir: Option<&Path>, rel_dir: &Path) -> Result<(String, String)> {
    let shipped = |p: &str| has_file(dir, &format!("{p}.dll")).is_some() || mod_dir.is_some_and(|d| has_file(&d.join(rel_dir), &format!("{p}.dll")).is_some());
    if let Some(p) = &c.proxy {
        if shipped(p) {
            bail!("clock.proxy={p}: the game (or the mod) ships its own {p}.dll next to the executable");
        }
        return Ok((p.clone(), "set with clock.proxy".into()));
    }
    let dlls: Vec<(String, Vec<String>)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) && p.metadata().is_ok_and(|m| m.len() < 512 << 20))
        .take(400)
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            Some((name, super::facts::parse_pe(&std::fs::read(&p).ok()?)?.imports))
        })
        .collect();
    let mut skipped = Vec::new();
    for p in quiver_vclock::PROXIES {
        let dll = format!("{p}.dll");
        if shipped(p) {
            skipped.push(format!("{dll} (shipped)"));
            continue;
        }
        if exe_imports.contains(&dll) {
            return Ok((p.to_string(), "the executable imports it".into()));
        }
        if let Some((by, _)) = dlls.iter().find(|(_, imp)| imp.contains(&dll)) {
            return Ok((p.to_string(), format!("{by}, next to the executable, imports it")));
        }
    }
    bail!(
        "the virtual clock has no way in: neither the executable nor a DLL beside it imports {} {}; if the game loads one of them at run time, set clock.proxy=<name>",
        quiver_vclock::PROXIES.iter().map(|p| format!("{p}.dll")).collect::<Vec<_>>().join(", "),
        if skipped.is_empty() { String::new() } else { format!("(skipped: {})", skipped.join(", ")) }
    )
}

/// `DXVK_CONFIG` with the clock's options after whatever the manifest set (later wins).
fn merge_dxvk_config(existing: Option<&str>) -> String {
    let ours = "dxgi.syncInterval = 0; d3d9.presentInterval = 0; dxgi.maxFrameRate = 0; d3d9.maxFrameRate = 0";
    match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("{}; {ours}", e.trim_end_matches(';')),
        None => ours.into(),
    }
}

/// Before a launch: clean up, then (when the clock is on) place the DLL and put the clock's
/// environment and DLL override into `m` (in memory, for this launch). Returns notes.
pub fn prepare(inst: &Inst, m: &mut Manifest, launch: Option<&str>, mod_dir: Option<&Path>) -> Result<Vec<String>> {
    let mut notes: Vec<String> = clean(inst, m).iter().map(|p| format!("virtual clock: removed the previous launch's {}", p.display())).collect();
    let Some(c) = effective(m, launch)? else { return Ok(notes) };
    let dir = exe_dir(inst, m)?;
    let exe = m.install_in(inst).join(&m.paths.exe);
    let pe = std::fs::read(&exe).ok().and_then(|b| super::facts::parse_pe(&b)).with_context(|| format!("{} isn't a Windows executable", exe.display()))?;
    if let Some(why) = quiver_vclock::unavailable(&pe.arch) {
        bail!("the virtual clock can't run this game: {why}");
    }
    let rel_dir = Path::new(&m.paths.exe).parent().unwrap_or(Path::new("")).to_path_buf();
    let (proxy, why) = choose_proxy(&c, &dir, &pe.imports, mod_dir, &rel_dir)?;
    let bytes = quiver_vclock::dll(&pe.arch, &proxy).context("the hook DLL isn't in this build")?;
    let target = dir.join(format!("{proxy}.dll"));
    super::write_atomic(&target, bytes)?;
    std::fs::write(inst.dir.join(RECORD), format!("{}\n", target.display()))?;

    let mut modules = c.modules.clone();
    if modules.is_empty() && has_file(&dir, "UnityPlayer.dll").is_some() {
        modules.push("UnityPlayer.dll".into());
        notes.push("virtual clock: a Unity game: UnityPlayer.dll's clock imports go virtual too (clock.modules to change)".into());
    }
    // QUIVER_VCLOCK_EXE: other processes that load the proxy (a crash handler, a launcher) don't
    // replace the game's report unless they present.
    let mut env: Vec<(String, String)> = vec![("QUIVER_VCLOCK".into(), c.rate.to_string()), ("QUIVER_VCLOCK_EXE".into(), m.exe_name())];
    if let Some(sl) = &c.sleeps {
        env.push(("QUIVER_VCLOCK_SLEEPS".into(), sl.clone()));
    }
    if !modules.is_empty() {
        env.push(("QUIVER_VCLOCK_MODULES".into(), modules.join(";")));
    }
    for (k, v) in [("QUIVER_VCLOCK_CREEP_AFTER", c.creep_after), ("QUIVER_VCLOCK_STUCK_MS", c.stuck_ms), ("QUIVER_VCLOCK_SLEEP_MAX_MS", c.sleep_max_ms)] {
        if let Some(v) = v {
            env.push((k.into(), v.to_string()));
        }
    }
    env.push(("QUIVER_VCLOCK_SYNC0".into(), if c.uncap { "1" } else { "0" }.into()));
    if c.uncap {
        env.push(("DXVK_CONFIG".into(), merge_dxvk_config(m.env.get("DXVK_CONFIG").map(String::as_str))));
        for (k, v) in [("DXVK_FRAME_RATE", "0"), ("VKD3D_FRAME_RATE", "0"), ("__GL_SYNC_TO_VBLANK", "0"), ("vblank_mode", "0")] {
            env.push((k.into(), v.into()));
        }
    }
    for (k, v) in env {
        m.env.insert(k, v);
    }
    m.dll_overrides.insert(proxy.clone(), "n,b".into());

    let t = quiver_vclock::timing(c.rate, quiver_vclock::WINE_QPF).context("bad rate")?;
    notes.push(format!(
        "virtual clock: {} Hz, every frame {:.4} ms of game time ({} ticks at QPF {}), through {proxy}.dll in the clone ({why}; {}); report in vclock.status, log in vclock.log",
        c.rate,
        t.period_ms,
        t.ticks_per_frame,
        t.qpf,
        pe.arch
    ));
    if !t.exact_ms {
        notes.push(format!(
            "virtual clock: {:.4} ms isn't a whole number of milliseconds, so a game timing frames with timeGetTime or GetTickCount sees {} and {} ms alternate; 125, 100, 62.5, 50 or 40 Hz are exact",
            t.period_ms,
            t.period_ms.floor(),
            t.period_ms.ceil()
        ));
    }
    if !t.exact_qpc {
        notes.push(format!("virtual clock: {} Hz doesn't divide the counter into whole ticks; frames are {}/{} s", c.rate, t.ticks_per_frame, t.qpf));
    }
    if c.uncap {
        if let Some(hz) = booted_refresh(inst)
            && hz < UNCAPPED_REFRESH
        {
            notes.push(format!(
                "virtual clock: the container's display was booted at {hz} Hz, which can pace the game's presents; `quiver lab down {0} && quiver lab up {0}` boots it at {UNCAPPED_REFRESH} Hz",
                inst.name
            ));
        }
        notes.push("virtual clock: presentation uncapped (sync interval 0, DXVK/vkd3d/GL frame caps off); a mod's own frame limiter (Pharos's fps_limit…) is yours to turn off".into());
    }
    Ok(notes)
}

/// The refresh the instance's gamescope last booted with (`/lab/boot.log`).
fn booted_refresh(inst: &Inst) -> Option<u32> {
    let text = std::fs::read_to_string(inst.lab().join("boot.log")).ok()?;
    let line = text.lines().rev().find(|l| l.starts_with("boot "))?;
    line.rsplit('@').next()?.trim().parse().ok()
}

/// What `quiver lab status` and `logs` print about a run's clock (nothing when it was off).
pub fn report_lines(run_dir: &Path, full: bool) -> Option<Vec<String>> {
    let r = Report::read(run_dir)?;
    let mut out = vec![r.summary()];
    if full || r.engaged {
        out.push(r.counters());
    }
    out.extend(r.hold_line());
    if full {
        out.extend(r.census());
        if r.engaged {
            out.push(if r.exact() {
                "every step the game read from the performance counter was exactly one frame".into()
            } else {
                "not every performance-counter step was one frame: see the census (creep shows as small steps, an escape as odd ones)".into()
            });
            out.push(if r.deterministic() {
                "deterministic: no frame was released to real time".into()
            } else {
                format!("NOT FULLY DETERMINISTIC: {} frame(s) were released to real time by the escape", r.escapes)
            });
        }
    }
    out.extend(r.verdict().into_iter().filter(|v| full || !v.starts_with("NOT FULLY")));
    Some(out)
}

/// The pause/step lines of the run's `vclock.log` (`control: …`), oldest first, without the
/// DLL's prefix.
pub fn control_lines(run_dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(run_dir.join(LOG_FILE)).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (head, rest) = l.split_once("] control: ")?;
            let t = head.rsplit(' ').next().unwrap_or("").trim();
            Some(format!("+{t}s {rest}"))
        })
        .collect()
}

/// The brief's section.
pub fn brief_section(i: &str) -> String {
    format!(
        "## The virtual clock (faster than real time, and repeatable)\n\n\
         `quiver lab edit {i} clock=virtual:62.5` (or `quiver lab launch {i} --clock virtual:62.5` for one launch; \
         `clock=real` turns it off) tells the game that every frame took exactly 1/62.5 s, however long it really took:\n\n\
         - **Faster than real time** when frames are cheap: presentation is uncapped, so a small or old game rendering \
         600 fps runs its simulation ~10x as fast. Use it to get through repros, menus, long walks and waits quickly.\n\
         - **Repeatable** when the machine is busy: if real frames are slower than 1/62.5 s the game runs slower than real \
         time but behaves exactly as at 62.5 Hz, frame for frame, so two runs of the same inputs see the same time. A \
         speed-up below 1.0x is normal then, not an error.\n\n\
         Pick a rate whose period is a whole number of milliseconds (125, 100, 62.5, 50, 40 Hz): games that time frames \
         with timeGetTime or GetTickCount then see the same step every frame. It changes time only, in the game's own \
         modules (the executable, plus `clock.modules=Engine.dll,…`; `UnityPlayer.dll` by itself); Wine, DXVK and the \
         lab keep real time.\n\n\
         **Reading the report** (`quiver lab status {i}`, `quiver lab logs {i}`; the DLL's own files are \
         `runs/r<N>/vclock.status` and `vclock.log`): engaged or not (it engages at the first Present it sees; until then \
         the game is on real time), frames, virtual vs real seconds and the speed-up, and a census of the steps the game \
         read between frames (one value, the frame, means exact). The counters: **creeping frames** (one thread read the clock \
         more than 4096 times in a frame, a busy-wait: time creeps so the wait ends; repeatable), **escapes** (a frame took over \
         1000 ms of real time and was released to real time: that frame's timing followed the machine, so the run is \
         flagged NOT FULLY DETERMINISTIC; raise `clock.stuck_ms` if loading screens trip it), **overruns** (the frames \
         creep or escape had already moved past the grid), sleeps shortened (a Sleep never advances game time; the \
         presenting thread's get shorter so a sleeping frame limiter ends fast, other threads' stay real so pollers \
         don't spin; `clock.sleeps=all|none` to change that).\n\n\
         **Pause and frame stepping** (clock on): `quiver lab clock {i} pause` holds the game at the end of the frame \
         in flight (it stays on screen; virtual time stops, however long you take), `quiver lab clock {i} step 12` lets \
         exactly 12 more frames through (each exactly 1/62.5 s) and holds again, `quiver lab clock {i} resume` runs \
         free, `quiver lab clock {i} status` says where it is. Each waits until the game holds, so `quiver lab clock {i} \
         step 12 && quiver lab shot {i}` shoots the 12th frame (`shot --hdr` works while held too). Input sent while held \
         is read when the game next steps: hold a key across steps with `quiver lab key {i} --down Right`, `quiver lab \
         clock {i} step 12`, `quiver lab key {i} --up Right`, `quiver lab clock {i} step 1`. A pause never times out \
         unless you say so (`--auto-resume SECS`). `quiver lab logs {i}` lists every pause and step.\n\n\
         **Limits.** Audio runs on a clock this can't reach and drifts (or stutters) at other speeds. Games that pace on \
         audio, on timers (waitable timers, timeSetEvent, WaitForSingleObject timeouts) or on clocks outside the list \
         (QueryPerformanceCounter, GetTickCount(64), timeGetTime, GetSystemTimeAsFileTime, Sleep) keep real time there. \
         Multiplayer, and DRM or anti-cheat timing checks, see a clock that disagrees with the world: use it on \
         DRM-free, offline, small games first; Denuvo and other DRM titles may react badly to time hooks. A mod's own \
         frame limiter (e.g. Pharos's `fps_limit`) caps the speed-up: turn it off yourself. It never touches licence or \
         ownership code.\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        super::super::manifest::tests::sample()
    }

    #[test]
    fn specs_and_edits() {
        assert_eq!(parse_spec("virtual:62.5").unwrap(), Some(62.5));
        assert_eq!(parse_spec("real").unwrap(), None);
        assert!(parse_spec("62.5").is_err() && parse_spec("virtual:0").is_err());
        let mut m = manifest();
        assert!(edit(&mut m, "clock.stuck_ms", "500").is_err(), "needs the clock on first");
        assert!(edit(&mut m, "clock", "virtual:62.5").unwrap());
        edit(&mut m, "clock.modules", "Engine.dll, Game.dll").unwrap();
        edit(&mut m, "clock.stuck_ms", "2000").unwrap();
        edit(&mut m, "clock.sleeps", "all").unwrap();
        assert!(edit(&mut m, "clock.sleeps", "some").is_err());
        edit(&mut m, "clock.proxy", "WINMM.dll").unwrap();
        assert!(edit(&mut m, "clock.proxy", "d3d9").is_err() && edit(&mut m, "clock.bogus", "1").is_err());
        assert!(!edit(&mut m, "display", "1x1").unwrap());
        // A new rate keeps the rest.
        edit(&mut m, "clock", "virtual:125").unwrap();
        let c = m.clock.clone().unwrap();
        assert_eq!((c.rate, c.modules.len(), c.stuck_ms, c.proxy.as_deref()), (125.0, 2, Some(2000), Some("winmm")));
        assert_eq!(boot_refresh(&m), UNCAPPED_REFRESH);
        edit(&mut m, "clock.uncap", "false").unwrap();
        assert_eq!(boot_refresh(&m), 165);
        // The manifest round-trips with it, and without.
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("lab.toml");
        m.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[clock]") && text.contains("rate = 125.0") && text.contains("uncap = false"), "{text}");
        assert_eq!(Manifest::load(&p).unwrap(), m);
        edit(&mut m, "clock", "real").unwrap();
        assert!(m.clock.is_none());
        // --clock on a launch overrides the manifest.
        assert_eq!(effective(&m, Some("virtual:50")).unwrap().unwrap().rate, 50.0);
        assert!(effective(&m, None).unwrap().is_none());
    }

    /// A minimal PE importing `dlls`.
    fn pe(dlls: &[&str]) -> Vec<u8> {
        super::super::facts::tests::fake_pe(dlls, None)
    }

    fn instance(t: &Path) -> (Inst, Manifest) {
        let lab = super::super::Lab::new(t.to_path_buf());
        let inst = lab.instance("t1");
        let mut m = manifest();
        m.paths.exe = "bin/Game.exe".into();
        std::fs::create_dir_all(inst.install().join("bin")).unwrap();
        (inst, m)
    }

    #[test]
    fn the_dll_goes_next_to_the_exe_and_comes_back_out() {
        if quiver_vclock::unavailable("x64").is_some() {
            eprintln!("skipping: no x64 DLL in this build");
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let (inst, mut m) = instance(t.path());
        let bin = inst.install().join("bin");
        std::fs::write(bin.join("Game.exe"), pe(&["kernel32.dll", "winmm.dll"])).unwrap();
        // The game ships its own version.dll: skipped; winmm is imported: chosen.
        std::fs::write(bin.join("version.dll"), b"the game's own").unwrap();
        m.clock = Some(VirtualClock::new(62.5));
        m.env.insert("DXVK_CONFIG".into(), "dxgi.customVendorId = 10de".into());
        let notes = prepare(&inst, &mut m, None, None).unwrap();
        assert!(notes.iter().any(|n| n.contains("through winmm.dll") && n.contains("the executable imports it")), "{notes:?}");
        assert!(quiver_vclock::is_ours(&std::fs::read(bin.join("winmm.dll")).unwrap()));
        assert_eq!(std::fs::read(bin.join("version.dll")).unwrap(), b"the game's own");
        assert_eq!(m.dll_overrides.get("winmm").map(String::as_str), Some("n,b"));
        assert_eq!(m.env.get("QUIVER_VCLOCK").map(String::as_str), Some("62.5"));
        assert_eq!(m.env.get("QUIVER_VCLOCK_EXE").map(String::as_str), Some("Game.exe"));
        assert!(!m.env.contains_key("QUIVER_VCLOCK_SLEEPS"), "present is the default");
        let dx = m.env.get("DXVK_CONFIG").unwrap();
        assert!(dx.starts_with("dxgi.customVendorId = 10de;") && dx.contains("dxgi.syncInterval = 0"), "{dx}");
        // The next launch with the clock off: the DLL is gone, the game's own file stays.
        let mut m2 = m.clone();
        m2.clock = None;
        let notes = prepare(&inst, &mut m2, None, None).unwrap();
        assert!(notes.iter().any(|n| n.contains("removed")), "{notes:?}");
        assert!(!bin.join("winmm.dll").exists() && bin.join("version.dll").exists());
        // --clock real overrides a manifest that has it on.
        let mut m3 = m.clone();
        assert!(prepare(&inst, &mut m3, Some("real"), None).unwrap().is_empty());
        assert!(!bin.join("winmm.dll").exists());
    }

    #[test]
    fn a_dll_beside_the_exe_can_be_the_way_in_and_none_is_an_error() {
        if quiver_vclock::unavailable("x64").is_some() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let (inst, mut m) = instance(t.path());
        let bin = inst.install().join("bin");
        std::fs::write(bin.join("Game.exe"), pe(&["kernel32.dll", "UnityPlayer.dll"])).unwrap();
        m.clock = Some(VirtualClock::new(60.0));
        let e = prepare(&inst, &mut m.clone(), None, None).unwrap_err().to_string();
        assert!(e.contains("no way in"), "{e}");
        std::fs::write(bin.join("UnityPlayer.dll"), pe(&["kernel32.dll", "version.dll"])).unwrap();
        let notes = prepare(&inst, &mut m, None, None).unwrap();
        assert!(notes.iter().any(|n| n.contains("UnityPlayer.dll, next to the executable, imports it")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("isn't a whole number of milliseconds")), "{notes:?}");
        assert_eq!(m.env.get("QUIVER_VCLOCK_MODULES").map(String::as_str), Some("UnityPlayer.dll"));
        // A mod that brings its own version.dll: refused when forced, skipped when not.
        let md = t.path().join("mod/bin");
        std::fs::create_dir_all(&md).unwrap();
        std::fs::write(md.join("version.dll"), b"mod").unwrap();
        let mut m2 = m.clone();
        m2.clock.as_mut().unwrap().proxy = Some("version".into());
        assert!(prepare(&inst, &mut m2, None, Some(&t.path().join("mod"))).unwrap_err().to_string().contains("ships its own"));
    }

    #[test]
    fn never_outside_the_instance() {
        let t = tempfile::tempdir().unwrap();
        let (inst, mut m) = instance(t.path());
        m.paths.exe = "../../../outside/Game.exe".into();
        std::fs::create_dir_all(t.path().join("outside")).unwrap();
        std::fs::write(t.path().join("outside/Game.exe"), pe(&["winmm.dll"])).unwrap();
        m.clock = Some(VirtualClock::new(60.0));
        let e = prepare(&inst, &mut m, None, None).unwrap_err().to_string();
        assert!(e.contains("isn't inside the instance"), "{e}");
        assert!(!t.path().join("outside/winmm.dll").exists());
    }

    #[test]
    fn reports_read_from_the_run_folder() {
        let t = tempfile::tempdir().unwrap();
        assert!(report_lines(t.path(), true).is_none());
        std::fs::write(t.path().join(STATUS_FILE), "rate=62.5\nproxy=version.dll\nengaged=1\npresent_path=dxgi\nticks_per_frame=160000\nqpf=10000000\nframes=10\nvirtual_s=0.16\nreal_s=0.02\nspeedup=8\nescapes=1\nstuck_ms=1000\ncensus_qpc=160000:9\n").unwrap();
        let l = report_lines(t.path(), true).unwrap();
        assert!(l[0].contains("8.00x"), "{l:?}");
        assert!(l.iter().any(|x| x.starts_with("NOT FULLY DETERMINISTIC: 1 frame")), "{l:?}");
        assert!(l.iter().any(|x| x.contains("exactly one frame")), "{l:?}");
    }

    #[test]
    fn the_brief_explains_it() {
        let s = brief_section("t1");
        for want in ["clock=virtual:62.5", "--clock virtual:62.5", "Repeatable", "below 1.0x is normal", "Audio", "Denuvo", "NOT FULLY DETERMINISTIC", "creeping frames", "fps_limit", "clock t1 step 12 && quiver lab shot t1", "--down Right"] {
            assert!(s.contains(want), "{want}");
        }
    }

    #[test]
    fn pause_and_step_lines_from_the_log() {
        let t = tempfile::tempdir().unwrap();
        assert!(control_lines(t.path()).is_empty());
        std::fs::write(
            t.path().join(LOG_FILE),
            "[vclock pid 512     0.001] control page Z:\\lab\\runs\\r1\\vclock.ctl: quiver lab clock pause|step|resume reach this process\n\
             [vclock pid 512     0.053] control: pause at frame 0 (virtual 0.000000 s): holds at frame 1\n\
             [vclock pid 512     0.596] control: held at frame 1 (virtual 0.016000 s)\n\
             [vclock pid 512    10.000] report: engaged ...\n",
        )
        .unwrap();
        assert_eq!(control_lines(t.path()), vec!["+0.053s pause at frame 0 (virtual 0.000000 s): holds at frame 1", "+0.596s held at frame 1 (virtual 0.016000 s)"]);
        // The report's line once pause/step was used.
        std::fs::write(t.path().join(STATUS_FILE), "rate=62.5\nengaged=1\nframes=40\nhold_state=held\npauses=1\nholds=2\nsteps=12\n").unwrap();
        let l = report_lines(t.path(), false).unwrap();
        assert!(l.iter().any(|x| x.starts_with("pause/step: HELD at frame 40")), "{l:?}");
    }
}
