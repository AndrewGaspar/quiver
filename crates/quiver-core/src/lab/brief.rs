//! `quiver lab brief <game>`: a ready-to-paste brief that lets a fresh agent start modding
//! a game in minutes: the game's facts, the instance commands, the rules (each one learned
//! the hard way by the Pharos harness), the tool menu and where the mod SDK is.

use serde::{Deserialize, Serialize};

use super::facts::Facts;
use super::manifest::Licence;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BriefInput {
    pub game: String,
    pub title: String,
    pub store: String,
    /// How a lab launch gets the licence.
    pub licence: Option<Licence>,
    /// Why a lab can't run this game (then `licence` is `None`).
    pub refused: Option<String>,
    pub install: String,
    pub prefix: String,
    pub proton: String,
    pub facts: Facts,
    /// The instance to use (made already, or the name the brief suggests).
    pub instance: String,
    pub instance_exists: bool,
    pub display: String,
    pub lab_root: String,
    pub fixtures: Vec<String>,
    pub profiles: Vec<String>,
    /// Where Quiver's checkout is, for the docs (`design/LAB.md`), if known.
    pub docs: Option<String>,
}

fn list(items: &[String], none: &str) -> String {
    if items.is_empty() {
        return format!("  - {none}\n");
    }
    items.iter().map(|i| format!("  - `{i}`\n")).collect()
}

/// The brief, as Markdown.
pub fn render(b: &BriefInput) -> String {
    let f = &b.facts;
    let i = &b.instance;
    let g = &b.game;
    let docs = b.docs.as_deref().map(|d| format!("{}/", d.trim_end_matches('/'))).unwrap_or_default();
    let mut s = String::new();
    s.push_str(&format!("# Modding brief: {} (`{g}`)\n\n", b.title));
    s.push_str(
        "You are starting work on a mod for this game. Quiver (`quiver`) gives you a disposable, headless \
         instance of the user's installed copy, running in a container with the user's real licence, that you \
         drive from the command line. Read the rules before the first launch.\n\n",
    );

    s.push_str("## The game\n\n");
    s.push_str(&format!("- Store: {} (`{g}`)\n", b.store));
    if let Some(why) = &b.refused {
        s.push_str(&format!("- **Lab launches are refused for this game:** {why}\n"));
    } else if let Some(l) = b.licence {
        s.push_str(&format!("- Licence in the lab: {}\n", l.describe()));
    }
    s.push_str(&format!("- Install: `{}` (the instance has its own reflink clone; the original is never mounted)\n", b.install));
    if f.exe.is_empty() {
        s.push_str("- Executable: not known (the store doesn't say; give it with `--exe`)");
    } else {
        s.push_str(&format!("- Executable: `{}`", f.exe));
    }
    if let Some(pe) = &f.pe {
        s.push_str(&format!(" ({})", pe.arch));
    }
    s.push('\n');
    s.push_str(&format!("- Graphics API (direct imports): {}\n", if f.graphics_apis.is_empty() { "none found (loaded at run time, or a launcher exe)".into() } else { f.graphics_apis.join(", ") }));
    let hints = |h: &[super::facts::Hint]| if h.is_empty() { "none found".to_owned() } else { h.iter().map(|h| format!("{} (`{}`)", h.name, h.evidence)).collect::<Vec<_>>().join("; ") };
    s.push_str(&format!("- Engine hints: {}\n", hints(&f.engines)));
    s.push_str(&format!("- SDKs and middleware: {}\n", hints(&f.sdks)));
    let eos_sdk = f.sdks.iter().any(|h| h.name == "Epic Online Services");
    if eos_sdk || b.licence == Some(Licence::EpicExchangeCode) {
        s.push_str(&eos_note(eos_sdk, i));
    }
    s.push_str(&format!("- Anti-cheat: {}\n", f.anticheat.as_deref().unwrap_or("no marker found")));
    if f.anticheat.is_some() {
        s.push_str("  - **Stop and ask the user** before touching anything online: mods are for offline single-player only, and anti-cheat is never bypassed.\n");
    }
    if !f.steam.drm_free() || b.store == "Steam" {
        s.push_str(&format!(
            "- Steam DRM: Steamworks {}, SteamStub {}\n",
            f.steam.steamworks.as_deref().map_or("no".into(), |e| format!("yes (`{e}`)")),
            if f.steam.steamstub { "yes (.bind section)" } else { "no" }
        ));
    }
    s.push_str(&format!("- Proton: `{}` (pinned in the instance; DXVK and vkd3d versions are in `quiver lab show {i}`)\n", b.proton));
    s.push_str("- Config files in the install:\n");
    s.push_str(&list(&f.install_configs, "none found"));
    s.push_str(&format!("- Config files in the prefix (`{}`), newest first:\n", b.prefix));
    s.push_str(&list(&f.prefix_configs, "none found (the game may write them on first run: look again after one)"));
    s.push_str("- Save files in the prefix, newest first:\n");
    s.push_str(&list(&f.prefix_saves, "none found"));
    s.push('\n');

    s.push_str("## Your instance\n\n");
    if b.instance_exists {
        s.push_str(&format!("Instance `{i}` exists ({}). `quiver lab show {i}` prints its manifest and state.\n\n", b.display));
    } else {
        s.push_str(&format!("Make it (once): `quiver lab create {g} --name {i} --display {}`\n\n", b.display));
    }
    s.push_str(&format!(
        "```sh\n\
         quiver lab up {i}                       # boot the container (headless gamescope display)\n\
         quiver lab launch {i} --wait frame      # kill everything, fresh licence, start the game, wait for a frame\n\
         quiver lab status {i}                   # readiness, pids, the mod's state, host load\n\
         quiver lab shot {i}                     # full-resolution screenshot (path printed)\n\
         quiver lab key {i} Return               # press a key (xdotool key names)\n\
         quiver lab wait {i} in-world --timeout 300   # a state your mod reports (see the lab channel)\n\
         quiver lab kill {i}                     # stop the game and every Wine process in the instance\n\
         quiver lab down {i}                     # stop the container\n\
         ```\n\n"
    ));
    s.push_str(&format!(
        "Launch with your mod build: `quiver lab launch {i} --mod <dir>` copies the folder's files over the instance's install \
         (originals are kept and put back before the next launch). Set it for every launch with \
         `quiver lab edit {i} mod_dir=<dir>`.\n\n"
    ));
    s.push_str(&format!("Lab root: `{}`. Fixtures for this game: {}. Graphics profiles: {}.\n\n", b.lab_root, if b.fixtures.is_empty() { "none yet".into() } else { b.fixtures.join(", ") }, if b.profiles.is_empty() { "none yet".into() } else { b.profiles.join(", ") }));

    s.push_str("## Rules (non-negotiable)\n\n");
    for r in RULES {
        s.push_str(&format!("- {r}\n"));
    }
    s.push('\n');

    s.push_str("## Tools\n\n");
    for (cmd, what) in TOOLS {
        s.push_str(&format!("- `{}` — {what}\n", cmd.replace("<i>", i)));
    }
    s.push('\n');

    s.push_str("## The lab channel (what your mod can tell the lab)\n\n");
    s.push_str(
        "The game's environment has `QUIVER_LAB=Z:\\lab` (the instance's lab folder), `QUIVER_LAB_RUN=r<N>` and \
         `QUIVER_LAB_RUN_DIR=Z:\\lab\\runs\\r<N>` (a fresh folder per launch: put your mod's log there, so you never \
         read the previous run's lines).\n\n\
         - **Readiness:** write one word to `%QUIVER_LAB%\\state` (`menu`, `loading`, `in-world`, …), optionally \
         followed by `key=value` lines. `quiver lab wait <i> in-world` then waits on it. Replace the file atomically \
         (write `state.tmp`, rename).\n\
         - **Requests:** `quiver lab call <i> <verb> [args…]` writes `%QUIVER_LAB%\\req\\<id>.json` \
         (`{\"id\":…, \"verb\":…, \"args\":[…]}`) and waits for your mod to write `%QUIVER_LAB%\\rsp\\<id>.json` \
         (`{\"ok\":true, \"result\":…}`). `quiver lab where <i>` is `call where`; `quiver lab teleport <i> <location>` \
         is `call teleport x y z [yaw [pitch]]` with the location looked up in the game's `locations.txt`.\n\
         - **Live config:** if your mod re-reads its config file, set `reload_pattern` (a regex for the log line \
         that says it did) and `log_file` in the manifest; `quiver lab set <i> … --wait` and A/B runs then wait \
         for the reload instead of sleeping.\n\n",
    );

    s.push_str(&super::vclock::brief_section(i));

    s.push_str("## Mod SDK and docs\n\n");
    s.push_str(&format!(
        "- `{docs}design/LAB.md`: this lab, its invariants and why each exists.\n\
         - `{docs}sdk/quiver_mod.h`: Quiver's mod SDK (options and metrics over the bridge's pipe; Ubisoft games \
         launched by Quiver). Lab sessions have no mod channel: in the lab, configure the mod through its files \
         (`quiver lab set`).\n\
         - `{docs}design/MOD-CONFIG.md`: how a mod declares options (`quiver-options.toml`) for the app's Options page.\n\
         - `{docs}design/MODS-V2.md`: how mods are packaged and versioned (`quiver-mod.toml`) once it works.\n\
         - Worked example: Pharos (the AC Origins DLSS/Reflex/frame-generation mod) was built with the harness this \
         lab generalises: a `dxgi`/`d3d11` proxy DLL in the install folder, a live-reloaded ini, a perf log line, and \
         `where`/`teleport` answered through files.\n"
    ));
    s
}

/// What the EOS proxy does to an Epic or EOS game in the lab (design/LAB.md section 3.1).
fn eos_note(sdk: bool, i: &str) -> String {
    let mut s = format!(
        "- **Epic Online Services:** in the instance, `{host}` (and Epic's other names) resolve to the lab's EOS proxy, \
         which terminates TLS with a certificate from the instance's own CA (trusted only in the instance's cloned prefix) \
         and forwards only sign-in, the SDK's config, EOS Connect and ownership/entitlement reads to Epic. Everything else, \
         cloud saves (Player Data Storage), achievements, stats, leaderboards, presence and telemetry included, gets a 403 \
         and never reaches Epic, so the game may say those are offline or failed; that is expected. If the game doesn't \
         trust the lab's CA, EOS is simply offline for the run (fail closed). Every handshake and decision is in \
         `runs/r<N>/eos-filter.log`; `quiver lab logs {i}` gives the verdict (did the proxy see the game's TLS and \
         requests, and did anything bypass it). If sign-in or ownership needs a blocked path, add it to `ALLOWLIST` in \
         `crates/quiver-core/src/lab/eos.rs` (design/LAB.md 3.1); never anything that writes to the account or reports.{}\n",
        if sdk { " Launches refuse to start unless the proxy answers from inside the container." } else { "" },
        host = super::eos::FORWARDED_HOST,
    );
    s.push_str("  - The allowlist:");
    for a in super::eos::ALLOWLIST {
        s.push_str(&format!(" `{} {}`", a.method, a.pattern));
    }
    s.push('\n');
    s
}

/// The rules every brief carries.
pub const RULES: &[&str] = &[
    "Never sign in to any store, and never run a store's client (Ubisoft Connect, the Epic launcher, Steam) in an instance. The lab gets the licence from quiverd per launch; Connect is disabled in every instance and a Connect process kills the run.",
    "Never read credential or token files (Connect's `ConnectSecureStorage.dat`, keyrings, `~/.config/*/credentials`).",
    "Never touch the user's real prefix or install (`quiver lab` never mounts them). Copy saves in as fixtures (`quiver lab fixture`), never link them.",
    "Nothing from an instance is reported: no play time, achievements, stats or cloud saves reach the user's accounts (for EOS games the lab's EOS proxy blocks them; its log is in the run folder). Don't enable a game's own cloud sync in an instance, and never route around the proxy (no hard-coded Epic addresses, no other DNS).",
    "Use `quiver lab launch`, `kill` and `wait`; never assemble pkill/sleep sequences. Never `pkill -f` or `pgrep -f` (it matches your own shell); the lab identifies processes by container and prefix.",
    "Poll a real signal, never a fixed sleep: `quiver lab wait` (process, window, frame, or your mod's state), `--wait` on `set`.",
    "Prefer keys and relative mouse motion. Clicks are fractions of the window (`quiver lab click <i> 0.5 0.62`) and must be checked: use `--expect-change` or a screenshot.",
    "Screenshots: let the frame settle first (temporal AA and upscalers converge over frames): `quiver lab shot <i> --settle 2`.",
    "Measurements: take the GPU first (`quiver lab gpu -- <cmd>`), and keep the host state each one logs with its numbers. Don't compare runs measured while other instances were rendering.",
    "Long runs (benchmarks, soaks) go in the background with `--bg` and are awaited with `quiver lab job wait <id>`; don't block on them.",
    "Mods stay offline and single-player. No licence, ownership, DRM or anti-cheat code is patched, stubbed or bypassed, ever.",
];

/// The tool menu (`<i>`: the instance).
pub const TOOLS: &[(&str, &str)] = &[
    ("quiver lab status <i> [--json]", "readiness ladder (process, window, frame, mod state), pids, run id, host state"),
    ("quiver lab wait <i> STATE [--timeout S] [--stable]", "wait for process|window|frame|exited or a mod state; --stable also waits until consecutive frames stop changing"),
    ("quiver lab shot <i> [FILE] [--settle S] [--crop C]", "screenshot from the instance's display (full resolution)"),
    ("quiver lab key <i> KEY… [--hold MS]", "press keys (xdotool names: Return, Escape, w, shift+Tab)"),
    ("quiver lab type <i> TEXT", "type text"),
    ("quiver lab mouse <i> DX DY [--steps N] [--repeat N --gap A-B --log FILE]", "relative mouse motion (mouse-look), optionally timed and logged for latency work"),
    ("quiver lab click <i> FX FY [--button N] [--expect-change]", "click at a fraction of the window; --expect-change fails if the screen didn't change"),
    ("quiver lab set <i> FILE:[section]key=value… [--wait]", "edit the instance's config files (manifest aliases, install:, prefix paths); --wait waits for the mod's reload"),
    ("quiver lab profile <i> NAME", "apply a graphics profile (config-set lines with {width}/{height}/{refresh})"),
    ("quiver lab fixture save|install|list <i> NAME", "save fixtures: archive the instance's saves, install one (game stopped)"),
    ("quiver lab call|where|teleport <i> …", "requests your mod answers through the lab channel; named locations in locations.txt"),
    ("quiver lab gpu [--for <i>] -- CMD…", "run a measurement with the GPU to itself: a lock, other instances throttled through their manifests' throttle lines"),
    ("quiver lab bench <i> --variant A=… --variant B=… [--rounds N --samples N --settle S --crop C --log-metric NAME=REGEX]", "A/B runs: live config variants, settle, N samples (sharpness, temporal noise, log numbers), aggregated, host state per variant"),
    ("quiver lab edit <i> clock=virtual:62.5 | clock=real", "the fixed-step virtual clock: every frame is exactly 1/62.5 s to the game (faster than real time when frames are cheap, repeatable when the machine is busy); `launch --clock …` for one launch"),
    ("quiver lab host [--json]", "free memory, load, GPU, who uses it, who holds the GPU lock"),
    ("quiver lab exec <i> -- CMD…", "a command in the instance's container with its display set"),
    ("quiver lab logs <i> [--run N]", "the run's folder: game.log, bridge.log, DXVK logs, your mod's log"),
    ("quiver lab job list|wait|show", "background jobs started with --bg"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_has_the_facts_commands_and_rules() {
        let b = BriefInput {
            game: "ubisoft:3539".into(),
            title: "Assassin's Creed Origins".into(),
            store: "Ubisoft".into(),
            licence: Some(Licence::UbisoftBridge),
            install: "/g/origins".into(),
            prefix: "/pf/origins".into(),
            proton: "GE-Proton11-12".into(),
            facts: Facts { exe: "ACOrigins.exe".into(), graphics_apis: vec!["Direct3D 11".into()], prefix_configs: vec!["drive_c/users/steamuser/Documents/Assassin's Creed Origins/ACO.ini".into()], ..Default::default() },
            instance: "origins".into(),
            instance_exists: false,
            display: "2560x1600@60".into(),
            lab_root: "/l".into(),
            docs: Some("/src/quiver".into()),
            ..Default::default()
        };
        let s = render(&b);
        for want in ["quiver lab create ubisoft:3539 --name origins", "Direct3D 11", "ACO.ini", "Connect is disabled", "pkill -f", "/src/quiver/design/LAB.md", "quiver lab bench origins", "QUIVER_LAB", "clock=virtual:62.5"] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
        assert!(!s.contains("EOS filter:") && !s.contains("Epic Online Services:**"));
        let refused = render(&BriefInput { licence: None, refused: Some("Steamworks needs the Steam client".into()), ..b.clone() });
        assert!(refused.contains("Lab launches are refused"));
        // An Epic game with the EOS SDK: the filter, its log and the allowlist.
        let sdk = super::super::facts::Hint { name: "Epic Online Services".into(), evidence: "EOSSDK-Win64-Shipping.dll".into() };
        let epic = render(&BriefInput { game: "epic:awr".into(), licence: Some(Licence::EpicExchangeCode), instance: "awr".into(), facts: Facts { sdks: vec![sdk], ..b.facts.clone() }, ..b });
        for want in ["**Epic Online Services:**", "api.epicgames.dev", "cloud saves (Player Data Storage)", "eos-filter.log", "quiver lab logs awr", "lab/eos.rs", "unless the proxy answers", "fail closed", "`POST /epic/oauth/{v}/token`"] {
            assert!(epic.contains(want), "missing {want:?} in\n{epic}");
        }
    }
}
