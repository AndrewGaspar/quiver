//! What a game launch needs for Sight (design/SIGHT-HUD.md §3.7, design/SIGHT.md §14), shared
//! by quiverd's launches (`quiverd::perf::launch`) and `quiver lab` instances: the Vulkan
//! layer order, the input preload's `LD_PRELOAD` entry, and where the installed Sight
//! libraries are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quiver_api::perf::Captures;

/// The layer manifest's `enable_environment`.
pub const ENV_SIGHT: &str = "QUIVER_SIGHT";
pub const SIGHT_LAYER: &str = "VK_LAYER_QUIVER_sight";
pub const OBS_LAYER: &str = "VK_LAYER_OBS_vkcapture_64";
/// `QUIVER_SIGHT_INPUT=0` in a launch's settings: no input preload (the preload honours it
/// too, passing everything through).
pub const ENV_INPUT: &str = "QUIVER_SIGHT_INPUT";
pub const LAYER_SO: &str = "libquiver_sight_layer.so";
pub const INPUT_SO: &str = "libquiver_sight_input.so";

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

pub fn truthy(env: &BTreeMap<String, String>, k: &str) -> bool {
    env.get(k).is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn uses_obs_capture(env: &BTreeMap<String, String>, wrappers: &[String]) -> bool {
    truthy(env, "OBS_VKCAPTURE") || wrappers.iter().any(|w| Path::new(w).file_name().is_some_and(|n| n == "obs-gamecapture"))
}

/// gamescope's WSI layer (both arches' names; the loader skips the other arch's). It turns the
/// game's HDR swapchain into an sRGB one for the driver and keeps the HDR metadata to itself,
/// so Sight must sit above it to see what the game asked for. It's implicit, which the loader
/// puts above every `VK_INSTANCE_LAYERS` layer, unless it's named there too.
pub const GAMESCOPE_WSI_LAYERS: [&str; 2] = ["VK_LAYER_FROG_gamescope_wsi_x86_64", "VK_LAYER_FROG_gamescope_wsi_i686"];

/// The Vulkan layer order captures imply, application first: the launch's own
/// `VK_INSTANCE_LAYERS`, the frame-generation and post-processing layers it turns on, then
/// Sight (and obs-vkcapture on the side `captures` says), then gamescope's WSI layer when the
/// launch turns it on (`ENABLE_GAMESCOPE_WSI=1`). Without it the loader may put Sight
/// above them and the HUD gets post-processed or interpolated.
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
    if truthy(env, "ENABLE_GAMESCOPE_WSI") {
        // Below Sight: moved there if the launch listed it above.
        out.retain(|l| !GAMESCOPE_WSI_LAYERS.contains(&l.as_str()));
        out.extend(GAMESCOPE_WSI_LAYERS.map(String::from));
    }
    out
}

/// `LD_PRELOAD` with the input preload `so` after the launch's own entries (`existing`), once.
pub fn join_preload(existing: Option<&str>, so: &str) -> String {
    match existing.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if v.split([' ', ':']).any(|x| x == so) => v.to_owned(),
        Some(v) => format!("{v}:{so}"),
        None => so.to_owned(),
    }
}

/// The input preload as an `LD_PRELOAD` entry for a Sight library folder laid out
/// `<dir>/{x86_64,i686}/…`: `<dir>/$PLATFORM/libquiver_sight_input.so` (ld.so's `$PLATFORM`
/// is `x86_64` or `i686`, so 32-bit processes load the 32-bit build).
pub fn preload_entry(sight_dir: &Path) -> String {
    format!("{}/$PLATFORM/{INPUT_SO}", sight_dir.display())
}

/// The installed Sight libraries for a program in `exe_dir`: `<prefix>/lib/quiver/sight` when
/// it runs from `<prefix>/bin` and the 64-bit builds are there.
pub fn installed_sight_dir(exe_dir: &Path, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let sight = exe_dir.parent()?.join("lib/quiver/sight");
    exists(&sight.join("x86_64").join(INPUT_SO)).then_some(sight)
}

/// The input preload as an `LD_PRELOAD` entry for a program in `exe_dir`: the installed one
/// ([`installed_sight_dir`]), else a dev build beside it (64-bit only).
pub fn input_preload_in(exe_dir: &Path, exists: impl Fn(&Path) -> bool) -> Option<String> {
    if let Some(d) = installed_sight_dir(exe_dir, &exists) {
        return Some(preload_entry(&d));
    }
    let beside = exe_dir.join(INPUT_SO);
    exists(&beside).then(|| beside.display().to_string())
}

/// The layer manifest for a library at `library_path` (as `quiver_sight_layer::manifest_json`
/// writes it; `tests` in the layer crate keep the two the same): an implicit layer the loader
/// enables with `QUIVER_SIGHT=1`, `library_arch` so 64- and 32-bit manifests can sit together.
pub fn layer_manifest_json(library_path: &Path, is_64bit: bool) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "file_format_version": "1.2.1",
        "layer": {
            "name": SIGHT_LAYER,
            "type": "GLOBAL",
            "library_path": library_path.display().to_string(),
            "library_arch": if is_64bit { "64" } else { "32" },
            "api_version": "1.3.281",
            "implementation_version": "1",
            "description": "Quiver Sight: present telemetry and the in-game performance HUD (enabled per launch by Quiver)",
            "enable_environment": { ENV_SIGHT: "1" },
            "disable_environment": { "QUIVER_SIGHT_DISABLE": "1" }
        }
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamescope_wsi_goes_below_sight() {
        let e = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        assert_eq!(layer_order(Captures::Shown, &e(&[]), false), [SIGHT_LAYER]);
        let o = layer_order(Captures::Shown, &e(&[("ENABLE_GAMESCOPE_WSI", "1"), ("VK_INSTANCE_LAYERS", "VK_LAYER_FROG_gamescope_wsi_x86_64:VK_LAYER_X")]), false);
        assert_eq!(o, ["VK_LAYER_X", SIGHT_LAYER, "VK_LAYER_FROG_gamescope_wsi_x86_64", "VK_LAYER_FROG_gamescope_wsi_i686"]);
        assert_eq!(layer_order(Captures::Shown, &e(&[("ENABLE_GAMESCOPE_WSI", "0")]), false), [SIGHT_LAYER]);
    }

    #[test]
    fn preload_joins_after_the_launchs_own() {
        let so = "/s/$PLATFORM/libquiver_sight_input.so";
        assert_eq!(join_preload(None, so), so);
        assert_eq!(join_preload(Some(" "), so), so);
        assert_eq!(join_preload(Some("/x/a.so"), so), format!("/x/a.so:{so}"));
        assert_eq!(join_preload(Some(&format!("{so} /y.so")), so), format!("{so} /y.so"));
        assert_eq!(preload_entry(Path::new("/opt/quiver/sight")), "/opt/quiver/sight/$PLATFORM/libquiver_sight_input.so");
    }

    #[test]
    fn installed_dirs() {
        let inst = |p: &Path| p == Path::new("/usr/lib/quiver/sight/x86_64/libquiver_sight_input.so");
        assert_eq!(installed_sight_dir(Path::new("/usr/bin"), inst).as_deref(), Some(Path::new("/usr/lib/quiver/sight")));
        assert_eq!(installed_sight_dir(Path::new("/usr/bin"), |_| false), None);
    }

    #[test]
    fn manifest_shape() {
        let m: serde_json::Value = serde_json::from_str(&layer_manifest_json(Path::new("/opt/q/x86_64/libquiver_sight_layer.so"), true)).unwrap();
        assert_eq!(m["layer"]["name"], SIGHT_LAYER);
        assert_eq!(m["layer"]["library_arch"], "64");
        assert_eq!(m["layer"]["enable_environment"]["QUIVER_SIGHT"], "1");
        // The same as the layer's own template, filled in.
        let template = include_str!("../../quiver-sight-layer/manifest/VK_LAYER_QUIVER_sight.json.in")
            .replace("@LIBRARY_PATH@", "/opt/q/x86_64/libquiver_sight_layer.so")
            .replace("@LIBRARY_ARCH@", "64");
        let t: serde_json::Value = serde_json::from_str(&template).unwrap();
        assert_eq!(m, t);
    }
}
