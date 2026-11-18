//! What a launch gets from the HUD (SIGHT-HUD §3.7, §2.4, §2.12, §1.3) when it has the Sight
//! layer: every launch by default, with the HUD hidden until Shift+F3 (`PerfHub::launch_config`
//! leaves it out where the setting turns it off or anti-cheat is in the install folder):
//!
//! | Variable | Value | For |
//! |---|---|---|
//! | `QUIVER_SESSION` | `s-…` | the layer and mods (the same id Options and the mod pipe use) |
//! | `QUIVER_PERF_DIR` | `/dev/shm/quiver-<uid>/<session>` | the Sight layer: its ring and `hud.image` |
//! | `QUIVER_SIGHT` | `1` | the layer manifest's `enable_environment` (an implicit layer that stays inert without it) |
//! | `VK_ADD_IMPLICIT_LAYER_PATH` | Quiver's layer dir | only when the layer is installed there rather than in `~/.local/share/vulkan/implicit_layer.d` |
//! | `VK_INSTANCE_LAYERS` | the order below | always: without it the loader may put Sight above (closer to the application than) vkBasalt or lsfg-vk, and the HUD gets post-processed or interpolated |
//! | `MANGOHUD_CONFIG` | `…,toggle_logging=Shift_L+F7` | only when the launch runs MangoHud: moves its logging key off Sight's Shift+F2 |
//! | `__NGX_LOG_LEVEL`, `__NGX_ENABLE_OVERRIDE_LOG_PATH`, `__NGX_LOG_PATH_OVERRIDE` | `1`, `1`, `Z:\…\<session>\ngx\` | only with `ngx_log` on: NGX's own log, for the DLSS preset, quality and render resolution (SIGHT-HUD §3.17) |
//! | `QUIVER_SIGHT_REFLEX_TIMINGS` | `1` | only with `reflex_timings` on: the layer reads Reflex's timing reports |
//!
//! Layer order, application first: the user's own `VK_INSTANCE_LAYERS`, then the
//! frame-generation and post-processing layers the launch turns on (the HUD must not be
//! interpolated or post-processed), then for captures `hidden` obs-vkcapture before Sight
//! (OBS captures before the HUD is composited), for `shown` Sight before obs-vkcapture. Only
//! layers the launch actually enables are listed (a layer named there is loaded even without
//! its enable variable). Spike H2 confirms the loader honours it for implicit layers.

use std::collections::BTreeMap;
use std::path::Path;

use quiver_api::perf::{Captures, HudConfig, LaunchPerfPlan};

pub const ENV_SIGHT: &str = "QUIVER_SIGHT";
pub const SIGHT_LAYER: &str = "VK_LAYER_QUIVER_sight";
pub const OBS_LAYER: &str = "VK_LAYER_OBS_vkcapture_64";
/// Layers that must stay above Sight (closer to the application), the variables that turn each
/// on (any), and the one that vetoes it. Frame generators belong here, or the HUD gets
/// interpolated with the game.
const ABOVE_SIGHT: [(&str, &[&str], Option<&str>); 4] = [
    ("VK_LAYER_VKBASALT_post_processing", &["ENABLE_VKBASALT"], None),
    // lsfg-vk v1.
    ("VK_LAYER_LS_frame_generation", &["ENABLE_LSFG"], None),
    // lsfg-vk v2: a config (or configless env mode) decides; DISABLE_LSFGVK vetoes.
    ("VK_LAYER_LSFGVK_frame_generation", &["LSFGVK_CONFIG", "LSFGVK_ENV"], Some("DISABLE_LSFGVK")),
    // NVIDIA Smooth Motion (the driver's implicit present layer).
    ("VK_LAYER_NV_present", &["NVPRESENT_ENABLE_SMOOTH_MOTION"], Some("DISABLE_LAYER_NV_PRESENT_1")),
];
/// MangoHud's logging key moves here (§1.3).
pub const MANGOHUD_LOGGING_KEY: &str = "Shift_L+F7";

fn truthy(env: &BTreeMap<String, String>, k: &str) -> bool {
    env.get(k).is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn uses_mangohud(env: &BTreeMap<String, String>, wrappers: &[String]) -> bool {
    truthy(env, "MANGOHUD") || wrappers.iter().any(|w| Path::new(w).file_name().is_some_and(|n| n == "mangohud"))
}

pub fn uses_obs_capture(env: &BTreeMap<String, String>, wrappers: &[String]) -> bool {
    truthy(env, "OBS_VKCAPTURE") || wrappers.iter().any(|w| Path::new(w).file_name().is_some_and(|n| n == "obs-gamecapture"))
}

/// The Vulkan layer order captures imply, application first.
pub fn layer_order(captures: Captures, env: &BTreeMap<String, String>, obs: bool) -> Vec<String> {
    let mut out: Vec<String> = env.get("VK_INSTANCE_LAYERS").map(|v| v.split(':').filter(|l| !l.is_empty() && *l != SIGHT_LAYER && *l != OBS_LAYER).map(str::to_owned).collect()).unwrap_or_default();
    for (layer, vars, veto) in ABOVE_SIGHT {
        let on = vars.iter().any(|v| truthy(env, v)) && !veto.is_some_and(|v| env.contains_key(v));
        if on && !out.iter().any(|l| l == layer) {
            out.push(layer.into());
        }
    }
    match (captures, obs) {
        (Captures::Hidden, true) => out.extend([OBS_LAYER.into(), SIGHT_LAYER.into()]),
        (Captures::Shown, true) => out.extend([SIGHT_LAYER.into(), OBS_LAYER.into()]),
        (_, false) => out.push(SIGHT_LAYER.into()),
    }
    out
}

/// MangoHud's config with the logging key moved: appended to the user's own
/// `MANGOHUD_CONFIG` (unless it already sets `toggle_logging`), else `read_cfg` first so
/// MangoHud still reads the user's config file.
pub fn mangohud_config(existing: Option<&str>) -> Option<String> {
    let key = format!("toggle_logging={MANGOHUD_LOGGING_KEY}");
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) if e.split([',', '\n']).any(|kv| kv.trim().starts_with("toggle_logging")) => None,
        Some(e) => Some(format!("{e},{key}")),
        None => Some(format!("read_cfg,{key}")),
    }
}

/// The plan for one launch. `env`/`wrappers` are the launch's settings as resolved so far;
/// `session_env` is the session dir's (`QUIVER_SESSION`, `QUIVER_PERF_DIR`); `layer_dir`
/// a Quiver-installed layer manifest dir, if any.
pub fn plan(cfg: &HudConfig, session_env: &[(&str, String)], env: &BTreeMap<String, String>, wrappers: &[String], layer_dir: Option<&Path>) -> LaunchPerfPlan {
    let mut p = LaunchPerfPlan { hud: Some(cfg.clone()), ..Default::default() };
    if !cfg.enabled {
        p.notes.push("no Sight layer (turned off; quiver hud enable GAME): the launch is unchanged".into());
        return p;
    }
    for (k, v) in session_env {
        p.env.insert((*k).to_owned(), v.clone());
    }
    p.env.insert(ENV_SIGHT.into(), "1".into());
    if let Some(d) = layer_dir {
        let joined = match env.get("VK_ADD_IMPLICIT_LAYER_PATH").filter(|v| !v.is_empty()) {
            Some(v) => format!("{}:{v}", d.display()),
            None => d.display().to_string(),
        };
        p.env.insert("VK_ADD_IMPLICIT_LAYER_PATH".into(), joined);
    }
    let obs = uses_obs_capture(env, wrappers);
    p.layer_order = layer_order(cfg.captures, env, obs);
    // Always, not only with OBS: the loader's own order for implicit layers could put
    // Sight above vkBasalt or lsfg-vk.
    p.env.insert("VK_INSTANCE_LAYERS".into(), p.layer_order.join(":"));
    if obs {
        p.notes.push(format!("obs-vkcapture: HUD {} in OBS game capture", if cfg.captures == Captures::Hidden { "hidden" } else { "shown" }));
    }
    if p.layer_order.len() > 1 {
        p.notes.push(format!("Vulkan layers: {}", p.layer_order.join(" → ")));
    }
    if uses_mangohud(env, wrappers) {
        match mangohud_config(env.get("MANGOHUD_CONFIG").map(String::as_str)) {
            Some(c) => {
                p.env.insert("MANGOHUD_CONFIG".into(), c);
                p.notes.push(format!("MangoHud logging moved to {MANGOHUD_LOGGING_KEY} (Shift+F2 is Sight's)"));
            }
            None => p.notes.push("MangoHud's toggle_logging is set by the game's settings; left as is".into()),
        }
    }
    if cfg.ngx_log
        && let Some((_, dir)) = session_env.iter().find(|(k, _)| *k == quiver_perf::session::ENV_PERF_DIR)
    {
        let add = super::nvidia::ngx_env(Path::new(dir), env);
        if add.len() < 3 {
            p.notes.push("NGX logging: the launch sets its own __NGX_* variables; those win".into());
        }
        p.env.extend(add);
        p.notes.push("NGX logging on (DLSS preset, quality and resolution in the HUD)".into());
    }
    if cfg.reflex_timings {
        p.env.insert("QUIVER_SIGHT_REFLEX_TIMINGS".into(), "1".into());
    }
    p.notes.push(format!("HUD: {} ({}), presenter {:?}, captures {:?}", cfg.layout, if cfg.on { "shown" } else { "hidden until Shift+F3" }, cfg.presenter, cfg.captures).to_lowercase());
    p
}

/// A Quiver-installed layer: `<data>/sight/layer/` with a manifest in it.
pub fn installed_layer_dir(data_dir: &Path) -> Option<std::path::PathBuf> {
    let d = data_dir.join("sight").join("layer");
    std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "json")).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn on() -> HudConfig {
        HudConfig { enabled: true, ..Default::default() }
    }

    fn off() -> HudConfig {
        HudConfig { enabled: false, ..Default::default() }
    }

    const SESSION: [(&str, &str); 2] = [("QUIVER_SESSION", "s-1"), ("QUIVER_PERF_DIR", "/dev/shm/quiver-1000/s-1")];

    fn sess() -> Vec<(&'static str, String)> {
        SESSION.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    #[test]
    fn off_changes_nothing() {
        let p = plan(&off(), &sess(), &env(&[("MANGOHUD", "1")]), &[], None);
        assert!(p.env.is_empty() && p.layer_order.is_empty());
        assert!(HudConfig::default().enabled, "the layer is in launches by default");
    }

    #[test]
    fn on_sets_the_session_and_the_layer() {
        let p = plan(&on(), &sess(), &BTreeMap::new(), &[], None);
        assert_eq!(p.env["QUIVER_SESSION"], "s-1");
        assert_eq!(p.env["QUIVER_PERF_DIR"], "/dev/shm/quiver-1000/s-1");
        assert_eq!(p.env["QUIVER_SIGHT"], "1");
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], SIGHT_LAYER, "always set while the HUD is on");
        assert!(!p.env.contains_key("MANGOHUD_CONFIG"));
        assert_eq!(p.layer_order, [SIGHT_LAYER]);
        let p = plan(&on(), &sess(), &BTreeMap::new(), &[], Some(Path::new("/q/sight/layer")));
        assert_eq!(p.env["VK_ADD_IMPLICIT_LAYER_PATH"], "/q/sight/layer");
    }

    #[test]
    fn capture_order() {
        let e = env(&[("OBS_VKCAPTURE", "1"), ("ENABLE_VKBASALT", "1"), ("VK_INSTANCE_LAYERS", "VK_LAYER_mine")]);
        let hidden = plan(&HudConfig { captures: Captures::Hidden, ..on() }, &sess(), &e, &[], None);
        assert_eq!(hidden.env["VK_INSTANCE_LAYERS"], "VK_LAYER_mine:VK_LAYER_VKBASALT_post_processing:VK_LAYER_OBS_vkcapture_64:VK_LAYER_QUIVER_sight");
        let shown = plan(&on(), &sess(), &BTreeMap::new(), &["obs-gamecapture".into()], None);
        assert_eq!(shown.env["VK_INSTANCE_LAYERS"], "VK_LAYER_QUIVER_sight:VK_LAYER_OBS_vkcapture_64");
    }

    #[test]
    fn sight_goes_below_post_processing_and_frame_generation_without_obs() {
        let e = env(&[("ENABLE_VKBASALT", "1"), ("ENABLE_LSFG", "1")]);
        let p = plan(&on(), &sess(), &e, &[], None);
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_VKBASALT_post_processing:VK_LAYER_LS_frame_generation:VK_LAYER_QUIVER_sight");
        // Only layers that are on: vkBasalt off, lsfg-vk on.
        let p = plan(&on(), &sess(), &env(&[("ENABLE_VKBASALT", "0"), ("ENABLE_LSFG", "1")]), &[], None);
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_LS_frame_generation:VK_LAYER_QUIVER_sight");
        let p = plan(&on(), &sess(), &env(&[("LSFGVK_CONFIG", "/s/lsfg.toml"), ("NVPRESENT_ENABLE_SMOOTH_MOTION", "1")]), &[], None);
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_LSFGVK_frame_generation:VK_LAYER_NV_present:VK_LAYER_QUIVER_sight");
        let p = plan(&on(), &sess(), &env(&[("LSFGVK_ENV", "1"), ("DISABLE_LSFGVK", "1")]), &[], None);
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_QUIVER_sight");
        // The user's own list is kept, in front, without duplicates of ours.
        let p = plan(&on(), &sess(), &env(&[("VK_INSTANCE_LAYERS", "VK_LAYER_mine:VK_LAYER_QUIVER_sight:VK_LAYER_VKBASALT_post_processing"), ("ENABLE_VKBASALT", "1")]), &[], None);
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_mine:VK_LAYER_VKBASALT_post_processing:VK_LAYER_QUIVER_sight");
        // Layer off: untouched.
        let p = plan(&off(), &sess(), &e, &[], None);
        assert!(!p.env.contains_key("VK_INSTANCE_LAYERS"));
    }

    #[test]
    fn nvidia_detail_is_opt_in() {
        let p = plan(&on(), &sess(), &BTreeMap::new(), &[], None);
        assert!(!p.env.keys().any(|k| k.starts_with("__NGX") || k == "QUIVER_SIGHT_REFLEX_TIMINGS"));
        let p = plan(&HudConfig { ngx_log: true, reflex_timings: true, ..on() }, &sess(), &env(&[("__NGX_LOG_LEVEL", "2")]), &[], None);
        assert_eq!(p.env["__NGX_LOG_PATH_OVERRIDE"], "Z:\\dev\\shm\\quiver-1000\\s-1\\ngx\\");
        assert_eq!(p.env["__NGX_ENABLE_OVERRIDE_LOG_PATH"], "1");
        assert!(!p.env.contains_key("__NGX_LOG_LEVEL"), "the launch's own level wins");
        assert_eq!(p.env["QUIVER_SIGHT_REFLEX_TIMINGS"], "1");
    }

    #[test]
    fn mangohud_logging_moves_off_shift_f2() {
        assert_eq!(mangohud_config(None).unwrap(), "read_cfg,toggle_logging=Shift_L+F7");
        assert_eq!(mangohud_config(Some("fps,frametime")).unwrap(), "fps,frametime,toggle_logging=Shift_L+F7");
        assert_eq!(mangohud_config(Some("fps,toggle_logging=F10")), None);
        let p = plan(&on(), &sess(), &env(&[("MANGOHUD_CONFIG", "cpu_temp")]), &["gamemoderun".into(), "/usr/bin/mangohud".into()], None);
        assert_eq!(p.env["MANGOHUD_CONFIG"], "cpu_temp,toggle_logging=Shift_L+F7");
        assert!(uses_mangohud(&env(&[("MANGOHUD", "1")]), &[]));
        assert!(!uses_mangohud(&env(&[("MANGOHUD", "0")]), &[]));
    }
}
