//! A game's launch settings applied to a Steam launch (`quiver steam-run`).
//!
//! Steam owns the launch: its Proton, its prefix, its runtime container. What carries
//! over from Quiver's settings is what can ride along on Steam's own command line:
//! environment variables, Wine DLL overrides (`WINEDLLOVERRIDES`, which Proton honours),
//! extra game arguments and wrapper commands in front of Steam's command (as if typed
//! before `%command%`). Proton and language stay Steam's.

use quiver_api::steamlib::SteamLaunchPlan;

/// `existing` (a `WINEDLLOVERRIDES` value) with `overrides` set, replacing any entry that
/// already names one of those DLLs.
pub fn merge_dll_overrides<'a>(existing: &str, overrides: impl IntoIterator<Item = (&'a String, &'a String)>) -> String {
    let mut parts: Vec<String> = existing.split(';').filter(|p| !p.is_empty()).map(str::to_owned).collect();
    for (dll, mode) in overrides {
        let dll = dll.to_ascii_lowercase();
        parts = parts
            .into_iter()
            .filter_map(|p| {
                let (names, m) = p.split_once('=').unwrap_or((p.as_str(), ""));
                let keep: Vec<&str> = names.split(',').map(str::trim).filter(|n| !n.eq_ignore_ascii_case(&dll)).collect();
                (!keep.is_empty()).then(|| format!("{}={m}", keep.join(",")))
            })
            .collect();
        parts.push(format!("{dll}={mode}"));
    }
    parts.join(";")
}

/// The command with the plan's wrappers in front (each split on whitespace, outermost
/// first).
pub fn wrap(plan: &SteamLaunchPlan, argv: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = plan.wrappers.iter().flat_map(|w| w.split_whitespace().map(str::to_owned)).collect();
    out.extend(argv);
    out
}

/// Environment variables to set (`WINEDLLOVERRIDES` is merged separately).
pub fn env(plan: &SteamLaunchPlan) -> Vec<(String, String)> {
    plan.env.iter().filter(|(k, _)| k.as_str() != "WINEDLLOVERRIDES").map(|(k, v)| (k.clone(), v.clone())).collect()
}

// ---------------------------------------------------------------------------------------
// Any Steam game (not a Ubisoft copy): nothing is swapped (design/STORE-STEAM.md §9)

/// Prefixes of the variables `steam-run` tells quiverd about (`SteamLaunchPlan`'s `env`): the
/// ones tweaks yield to and the HUD's layer order looks at. Nothing else of Steam's
/// environment leaves the process.
const PLAN_ENV_PREFIXES: &[&str] = &[
    "DXVK_", "VKD3D_", "PROTON_", "WINE", "VK_", "ENABLE_", "DISABLE_", "LSFG", "NVPRESENT_", "MANGOHUD", "OBS_VKCAPTURE", "__GL_", "NGX_", "SL_", "PROTONFIXES_",
];

/// Steam's variable for the game's install folder: quiverd looks there for anti-cheat
/// (the Sight layer stays out of those games; `quiver_core::anticheat`).
pub const STEAM_INSTALL_VAR: &str = "STEAM_COMPAT_INSTALL_PATH";

/// The part of Steam's environment quiverd plans against (see [`PLAN_ENV_PREFIXES`]), and
/// the install folder ([`STEAM_INSTALL_VAR`]).
pub fn plan_env<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> std::collections::BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(k, _)| *k == STEAM_INSTALL_VAR || PLAN_ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Steam's command with the plan applied: wrappers in front (as if typed before
/// `%command%`), the plan's arguments after the game's own. Nothing else changes: the
/// reaper, the runtime, Proton and the game stay Steam's.
pub fn steam_command(plan: &SteamLaunchPlan, argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    out.extend(plan.args.iter().cloned());
    wrap(plan, out)
}

/// The variables to set on Steam's command, given what Steam set (`inherited`): the plan's
/// `env`, `WINEDLLOVERRIDES` merged with Steam's, and `LD_PRELOAD` kept after Steam's (its
/// overlay rides on it), so a setting never drops Steam's overlay.
pub fn steam_env(plan: &SteamLaunchPlan, inherited: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env(plan)
        .into_iter()
        .map(|(k, v)| match (k.as_str(), inherited(&k).filter(|s| !s.is_empty())) {
            ("LD_PRELOAD", Some(steam)) if !steam.split([' ', ':']).any(|x| x == v) => (k, format!("{steam}:{v}")),
            ("LD_PRELOAD", Some(steam)) => (k, steam),
            _ => (k, v),
        })
        .collect();
    if !plan.dll_overrides.is_empty() {
        let base = inherited("WINEDLLOVERRIDES").unwrap_or_default();
        out.push(("WINEDLLOVERRIDES".into(), merge_dll_overrides(&base, &plan.dll_overrides)));
    }
    out
}

/// What the plan changes, one line each (for the steam-run log).
pub fn describe(plan: &SteamLaunchPlan) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in &plan.env {
        out.push(format!("env {k}={v}"));
    }
    for (k, v) in &plan.dll_overrides {
        out.push(format!("dll {k}={v}"));
    }
    if !plan.args.is_empty() {
        out.push(format!("args {}", plan.args.join(" ")));
    }
    if !plan.wrappers.is_empty() {
        out.push(format!("wrappers {}", plan.wrappers.join(" ")));
    }
    for i in &plan.ignored {
        out.push(format!("not applied: {i}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dll_overrides_merge() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("DXGI".to_owned(), "n,b".to_owned()); // sorts before d3d11
        m.insert("d3d11".to_owned(), "n".to_owned());
        assert_eq!(merge_dll_overrides("", &m), "dxgi=n,b;d3d11=n");
        assert_eq!(
            merge_dll_overrides("upc.exe=d;dxgi,d3d9=b;winmm=n,b", &m),
            "upc.exe=d;d3d9=b;winmm=n,b;dxgi=n,b;d3d11=n"
        );
        assert_eq!(merge_dll_overrides("upc.exe=d", &std::collections::BTreeMap::new()), "upc.exe=d");
    }

    #[test]
    fn wrappers_and_env() {
        let mut p = SteamLaunchPlan { product_id: 1, ..Default::default() };
        p.wrappers = vec!["gamemoderun".into(), "mangohud --dlsym".into()];
        p.env.insert("DXVK_HUD".into(), "fps".into());
        p.env.insert("WINEDLLOVERRIDES".into(), "x=n".into());
        assert_eq!(wrap(&p, vec!["reaper".into(), "--".into()]), ["gamemoderun", "mangohud", "--dlsym", "reaper", "--"]);
        assert_eq!(env(&p), [("DXVK_HUD".to_owned(), "fps".to_owned())]);
        assert!(describe(&p).iter().any(|l| l == "wrappers gamemoderun mangohud --dlsym"));
        assert!(wrap(&SteamLaunchPlan::default(), vec!["a".into()]) == ["a"]);
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn any_steam_game_keeps_steams_command() {
        let mut p = SteamLaunchPlan::for_game(quiver_api::GameRef::steam(870780));
        // Steam's `%command%`: the reaper, the runtime, Proton, the game and its arguments.
        let argv = s(&["/s/ubuntu12_32/reaper", "SteamLaunch", "AppId=870780", "--", "/s/proton", "waitforexitandrun", "/g/Control_DX12.exe", "-steam"]);
        assert_eq!(steam_command(&p, &argv), argv, "an empty plan changes nothing");
        p.wrappers = vec!["gamemoderun".into()];
        p.args = vec!["-dx12".into()];
        let out = steam_command(&p, &argv);
        assert_eq!(out[0], "gamemoderun");
        assert_eq!(out[1..9], argv[..]);
        assert_eq!(out[9], "-dx12");
        // A native game the same way.
        assert_eq!(steam_command(&p, &s(&["/g/game.x86_64"])), s(&["gamemoderun", "/g/game.x86_64", "-dx12"]));

        // The environment: Steam's overlay preload kept, DLL overrides merged with Steam's.
        p.env.insert("LD_PRELOAD".into(), "/x/libmine.so".into());
        p.env.insert("PROTON_LOG".into(), "1".into());
        p.dll_overrides.insert("dxgi".into(), "n,b".into());
        let steam = |k: &str| match k {
            "LD_PRELOAD" => Some("/s/gameoverlayrenderer.so".to_owned()),
            "WINEDLLOVERRIDES" => Some("steam.exe=b".to_owned()),
            _ => None,
        };
        let env: std::collections::BTreeMap<String, String> = steam_env(&p, steam).into_iter().collect();
        assert_eq!(env["LD_PRELOAD"], "/s/gameoverlayrenderer.so:/x/libmine.so");
        assert_eq!(env["WINEDLLOVERRIDES"], "steam.exe=b;dxgi=n,b");
        assert_eq!(env["PROTON_LOG"], "1");
        let env: std::collections::BTreeMap<String, String> = steam_env(&p, |_| None).into_iter().collect();
        assert_eq!((env["LD_PRELOAD"].as_str(), env["WINEDLLOVERRIDES"].as_str()), ("/x/libmine.so", "dxgi=n,b"));

        // Only the variables the plan needs go to quiverd.
        let sent = plan_env([("DXVK_CONFIG", "a=1"), ("HOME", "/h"), ("STEAM_COMPAT_DATA_PATH", "/c"), ("VK_INSTANCE_LAYERS", "x"), ("ENABLE_VKBASALT", "1"), ("SSH_AUTH_SOCK", "/s"), ("STEAM_COMPAT_INSTALL_PATH", "/s/common/G")]);
        assert_eq!(sent.keys().map(String::as_str).collect::<Vec<_>>(), ["DXVK_CONFIG", "ENABLE_VKBASALT", "STEAM_COMPAT_INSTALL_PATH", "VK_INSTANCE_LAYERS"]);
    }
}
