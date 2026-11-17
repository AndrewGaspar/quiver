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
}
