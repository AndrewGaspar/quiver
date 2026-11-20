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
//! | `QUIVER_SIGHT_CAPTURE` | `1` | unless the launch sets it: swapchains readable for the Sight panel's Screenshot (TRANSFER_SRC usage where the surface allows it) |
//! | `LD_PRELOAD` | the launch's own, then `<lib>/sight/$PLATFORM/libquiver_sight_input.so` | the Sight panel's input in the game ([`add_input_preload`], design/SIGHT.md §14); with the layer, never without it. `$PLATFORM` is ld.so's (`x86_64`, `i686`), so 32-bit processes load the 32-bit build. Steam copies keep Steam's own first (STORE-STEAM G6) |
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

// The layer order and the preload's entry are shared with `quiver lab` (quiver_core::sightenv).
pub use quiver_core::sightenv::{ENV_INPUT, ENV_SIGHT, input_preload_in, layer_order, truthy, uses_obs_capture};
/// The Sight layer's capture switch (screenshots of the game's frames).
pub const ENV_CAPTURE: &str = "QUIVER_SIGHT_CAPTURE";
/// MangoHud's logging key moves here (§1.3).
pub const MANGOHUD_LOGGING_KEY: &str = "Shift_L+F7";

pub fn uses_mangohud(env: &BTreeMap<String, String>, wrappers: &[String]) -> bool {
    truthy(env, "MANGOHUD") || wrappers.iter().any(|w| Path::new(w).file_name().is_some_and(|n| n == "mangohud"))
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
    // Screenshots from the Sight panel (design/SIGHT.md §3.7): the layer adds TRANSFER_SRC to
    // the swapchain's usage where the surface allows it. The launch's own value wins (`0`:
    // no screenshots).
    if !env.contains_key(ENV_CAPTURE) {
        p.env.insert(ENV_CAPTURE.into(), "1".into());
    }
    p.notes.push(format!("HUD: {} ({}), presenter {:?}, captures {:?}", cfg.layout, if cfg.on { "shown" } else { "hidden until Shift+F3" }, cfg.presenter, cfg.captures).to_lowercase());
    p
}

/// The Sight panel's input preload (`libquiver_sight_input.so`, design/SIGHT.md §14): with the
/// layer (it shares the layer's per-game switch, `quiver hud enable/disable GAME`), appended
/// to the launch's own `LD_PRELOAD`. Without it the panel opens on the desktop.
pub fn add_input_preload(p: &mut LaunchPerfPlan, env: &BTreeMap<String, String>, preload: Option<&str>) {
    if !p.env.contains_key(ENV_SIGHT) {
        return;
    }
    if env.get(ENV_INPUT).is_some_and(|v| v == "0") {
        p.notes.push(format!("{ENV_INPUT}=0: no Sight input preload; the panel opens on the desktop"));
        return;
    }
    let Some(so) = preload else {
        p.notes.push("the Sight input preload isn't installed: the panel opens on the desktop".into());
        return;
    };
    p.env.insert("LD_PRELOAD".into(), quiver_core::sightenv::join_preload(env.get("LD_PRELOAD").map(String::as_str), so));
    p.notes.push("Sight input preload: the panel draws in the game and takes its keyboard and mouse while open".into());
}

/// The installed input preload, as an `LD_PRELOAD` entry: `<prefix>/lib/quiver/sight/$PLATFORM/
/// libquiver_sight_input.so` when quiverd runs from `<prefix>/bin` and the 64-bit build is
/// there, else a dev build beside quiverd (64-bit only).
pub fn installed_input_preload() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    input_preload_in(exe.parent()?, |p| p.is_file())
}

/// A Quiver-installed layer: `<data>/sight/layer/` with a manifest in it.
pub fn installed_layer_dir(data_dir: &Path) -> Option<std::path::PathBuf> {
    let d = data_dir.join("sight").join("layer");
    std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "json")).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_core::sightenv::SIGHT_LAYER;

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
        // Screenshots: on unless the launch says otherwise.
        assert_eq!(p.env[ENV_CAPTURE], "1");
        let p = plan(&on(), &sess(), &env(&[(ENV_CAPTURE, "0")]), &[], None);
        assert!(!p.env.contains_key(ENV_CAPTURE), "the launch's own value stays (it's in its env already)");
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
    fn input_preload_with_the_layer_after_the_launchs_own() {
        let so = "/usr/lib/quiver/sight/$PLATFORM/libquiver_sight_input.so";
        let mut p = plan(&on(), &sess(), &BTreeMap::new(), &[], None);
        add_input_preload(&mut p, &BTreeMap::new(), Some(so));
        assert_eq!(p.env["LD_PRELOAD"], so);
        let e = env(&[("LD_PRELOAD", "/x/libmine.so")]);
        let mut p = plan(&on(), &sess(), &e, &[], None);
        add_input_preload(&mut p, &e, Some(so));
        assert_eq!(p.env["LD_PRELOAD"], format!("/x/libmine.so:{so}"));
        // Already there: not twice.
        let both = format!("{so} /y.so");
        let e = env(&[("LD_PRELOAD", both.as_str())]);
        let mut p = plan(&on(), &sess(), &e, &[], None);
        add_input_preload(&mut p, &e, Some(so));
        assert_eq!(p.env["LD_PRELOAD"], both);
        // Without the layer (turned off, anti-cheat): never.
        let mut p = plan(&off(), &sess(), &BTreeMap::new(), &[], None);
        add_input_preload(&mut p, &BTreeMap::new(), Some(so));
        assert!(!p.env.contains_key("LD_PRELOAD"));
        // Not installed, or turned off for the launch: a note, no variable.
        let mut p = plan(&on(), &sess(), &BTreeMap::new(), &[], None);
        add_input_preload(&mut p, &BTreeMap::new(), None);
        assert!(!p.env.contains_key("LD_PRELOAD") && p.notes.iter().any(|n| n.contains("isn't installed")));
        let e = env(&[(ENV_INPUT, "0")]);
        let mut p = plan(&on(), &sess(), &e, &[], None);
        add_input_preload(&mut p, &e, Some(so));
        assert!(!p.env.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn input_preload_paths() {
        let installed = |p: &Path| p == Path::new("/usr/lib/quiver/sight/x86_64/libquiver_sight_input.so");
        assert_eq!(input_preload_in(Path::new("/usr/bin"), installed).as_deref(), Some("/usr/lib/quiver/sight/$PLATFORM/libquiver_sight_input.so"));
        let dev = |p: &Path| p == Path::new("/src/q/target/release/libquiver_sight_input.so");
        assert_eq!(input_preload_in(Path::new("/src/q/target/release"), dev).as_deref(), Some("/src/q/target/release/libquiver_sight_input.so"));
        assert_eq!(input_preload_in(Path::new("/usr/bin"), |_| false), None);
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
