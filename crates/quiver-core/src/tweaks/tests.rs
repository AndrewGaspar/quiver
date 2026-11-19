use super::compile::{Inputs, compile, dxvk_entries};
use super::evidence::{Logs, finish, scan};
use super::*;
use quiver_api::options::Severity;
use quiver_api::tweaks::{Capabilities, Gpu, NgxCaps, Planned, ProtonCaps, TweakEffect};
use serde_json::json;

fn nvidia(series: u32) -> Capabilities {
    let name = format!("NVIDIA GeForce RTX {series}70 Laptop GPU");
    let gpu = Gpu { vendor: "nvidia".into(), name, rtx_series: Some(series), arch: None };
    Capabilities {
        gpus: vec![gpu.clone()],
        gpu: Some(gpu),
        driver: Some("620.70.06".into()),
        proton: Some(ge()),
        ngx: NgxCaps { dir: Some("/usr/lib/nvidia/wine".into()), core: true, updater: false },
        route: "quiver".into(),
        api: "unknown".into(),
        ..Default::default()
    }
}

/// GE-Proton11-12 as the spikes found it.
fn ge() -> ProtonCaps {
    ProtonCaps {
        name: "GE-Proton11-12".into(),
        flavour: "ge".into(),
        dxvk: Some("v3.1.1-174-gf86377680".into()),
        vkd3d: Some("1.1-5687-g8293130eb".into()),
        nvapi: Some("v0.9.3".into()),
        has: ["dxvk:dxgi.maxFrameRate", "dxvk:dxvk.latencySleep", "dxvk:dxvk.disableNvLowLatency2", "nvapi:SL_DLSS_OVERRIDE", "nvapi:NGX_DLSSG_MODE", "nvapi:NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE", "proton:PROTON_ENABLE_HDR"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    }
}

/// Proton Experimental: DXVK master renamed and removed the low-latency options.
fn experimental() -> ProtonCaps {
    ProtonCaps { name: "Proton - Experimental".into(), flavour: "valve".into(), dxvk: Some("v3.1.1-209".into()), nvapi: Some("v0.9.3".into()), has: vec!["dxvk:dxvk.enableNvLowLatency2".into()], copies_updater: true, ..Default::default() }
}

fn amd() -> Capabilities {
    let gpu = Gpu { vendor: "amd".into(), ..Default::default() };
    Capabilities { gpu: Some(gpu.clone()), gpus: vec![gpu], proton: Some(ge()), route: "quiver".into(), api: "unknown".into(), ..Default::default() }
}

/// A plan for these tweak values at the global scope.
fn plan_with(values: Value, caps: &Capabilities, user: &[(&str, &str)]) -> super::compile::Compiled {
    plan_full(values, caps, user, &[], &[], &BTreeMap::new(), false)
}

fn plan_full(values: Value, caps: &Capabilities, user: &[(&str, &str)], providers: &[ProviderInfo], extra: &[ActiveConflict], prefer: &BTreeMap<String, (Prefer, ScopeKind)>, indicator: bool) -> super::compile::Compiled {
    let rev = OptionsRevision { revision: 1, source: "cli".into(), values: [(SURFACE_KEY.to_owned(), serde_json::from_value(values).unwrap())].into(), ..Default::default() };
    let scopes = Scopes { global: vec![rev], game: Some(Vec::new()), profile: None };
    let resolved = resolve(schema(), &scopes, ScopeKind::Profile);
    let mut conflicts = builtin_conflicts(schema(), &effective(&resolved));
    conflicts.extend(extra.iter().cloned());
    let user: BTreeMap<String, String> = user.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
    compile(&Inputs { schema: schema(), resolved: &resolved, prefer, caps, providers, conflicts: &conflicts, user_env: &user, log_dir: Some(Path::new("/s/logs")), indicator_was_on: indicator })
}

fn planned<'a>(c: &'a super::compile::Compiled, id: &str) -> &'a Planned {
    &c.plan.entry(id).unwrap_or_else(|| panic!("no entry {id}")).planned
}

#[test]
fn the_catalog_parses_and_lints_clean_with_the_defaults_game_s_choice() {
    let s = schema();
    assert_eq!(s.kind, Some(quiver_api::options::SurfaceKind::Platform));
    let errors: Vec<String> = crate::options::schema::lint(s).into_iter().filter(|f| f.severity == Severity::Error).map(|f| f.to_string()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    // Steer 15: every tweak defaults to the game's choice / off, so nothing is written.
    let c = plan_with(json!({}), &nvidia(50), &[]);
    assert!(c.plan.entries.is_empty() && c.plan.env.is_empty());
    for (id, d) in &s.options {
        let v = d.default.clone().unwrap_or_else(|| panic!("{id} has no default"));
        let off = matches!(scalar_text(&v).as_str(), "app" | "auto" | "0" | "false") || d.when.is_some();
        assert!(off, "{id} defaults to {v}");
    }
    // The v1 list (TWEAKS §4.1).
    for id in ["fps.cap", "sync.vsync", "sync.max_latency", "latency.mode", "dlss.sr.model", "dlss.sr.scale", "dlss.rr.model", "dlss.fg.model", "dlss.fg.frames", "dlss.fg.target", "dlss.runtime", "dlss.indicator", "fg.smooth_motion", "tex.anisotropy", "tex.lod_bias", "hdr"] {
        assert!(s.options.contains_key(id), "{id}");
    }
    // sight.stereo (design/SIGHT.md §15): the Sight layer's environment, nothing for auto.
    let c = plan_with(json!({"sight.stereo": "hsbs"}), &nvidia(50), &[]);
    assert_eq!(c.plan.env.get("QUIVER_SIGHT_STEREO").map(String::as_str), Some("hsbs"), "{:?}", c.plan.env);
    let c = plan_with(json!({"sight.stereo": "auto"}), &nvidia(50), &[]);
    assert!(c.plan.env.is_empty());
    // sight.depth (design/STEREO-UI.md): a percent for the layer, nothing at the default.
    let c = plan_with(json!({"sight.depth": "150"}), &nvidia(50), &[]);
    assert_eq!(c.plan.env.get("QUIVER_SIGHT_DEPTH").map(String::as_str), Some("150"), "{:?}", c.plan.env);
    let c = plan_with(json!({"sight.depth": "off"}), &nvidia(50), &[]);
    assert_eq!(c.plan.env.get("QUIVER_SIGHT_DEPTH").map(String::as_str), Some("off"), "{:?}", c.plan.env);
    let c = plan_with(json!({"sight.depth": "auto"}), &nvidia(50), &[]);
    assert!(c.plan.env.is_empty(), "{:?}", c.plan.env);
    // Every preset validates.
    for p in &s.presets {
        for (id, v) in &p.set {
            s.options[id].normalize(v).unwrap_or_else(|e| panic!("preset {}: {id}: {e}", p.id));
        }
    }
}

#[test]
fn aco_dlss_declares_what_it_provides() {
    let (_, aco, _) = crate::options::catalog().into_iter().find(|(id, _, _)| *id == "aco-dlss").unwrap();
    assert_eq!(aco.provides["fps.cap"].option, "reflex.fps_limit");
    assert_eq!(aco.provides["tex.lod_bias"].mode, ProvideMode::Own);
    assert!(aco.conflicts.iter().any(|c| c.tweak.as_deref() == Some("dlss.fg.frames")));
    let lint = crate::options::schema::lint(&aco);
    assert!(lint.iter().all(|f| f.severity != Severity::Error), "{lint:#?}");
}

#[test]
fn scopes_resolve_global_then_game_then_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();
    let game = GameRef::ubisoft(3539);
    let store = ModStore::new(data, 3539);
    let s = schema();
    let set = |at: At<'_>, ops: Vec<OptionOp>, inherited: &BTreeMap<String, Value>| {
        let cur = at.current().unwrap();
        let a = apply_ops(s, at.kind(), &cur, inherited, &ops);
        at.append(a.values, a.prefer, "cli", None, a.changes).unwrap()
    };
    let op = |id: &str, v: Value| OptionOp::Set { surface: "quiver".into(), id: id.into(), value: v };
    set(At::Global(data), vec![op("fps.cap", json!(160)), op("tex.anisotropy", json!("16"))], &BTreeMap::new());
    set(At::Game(data, game), vec![op("sync.vsync", json!("off")), op("tex.anisotropy", json!("app"))], &BTreeMap::new());
    // The profile scope lives in the profile's options file, next to its mods' values.
    crate::options::append(&store, "vanilla", [("m1/aco-dlss".to_owned(), [("fg.mode".to_owned(), json!("off"))].into())].into(), Default::default(), "cli", None, None, vec![]).unwrap();
    set(At::Profile(&store, "vanilla".into()), vec![op("fps.cap", json!("120"))], &BTreeMap::new());
    assert_eq!(crate::options::current(&store, "vanilla").unwrap().values["m1/aco-dlss"]["fg.mode"], json!("off"), "other surfaces are kept");

    let scopes = Scopes::load(data, Some(&game), Some((&store, "vanilla"))).unwrap();
    let r = resolve(s, &scopes, ScopeKind::Profile);
    assert_eq!(r["fps.cap"].value, json!(120.0));
    assert_eq!(r["fps.cap"].inherited, json!(160.0));
    assert!(matches!(r["fps.cap"].from, ValueSource::Scope { scope: ScopeKind::Profile, revision: 2, .. }));
    assert_eq!(r["sync.vsync"].set, Some(ScopeKind::Game));
    // The game scope sets the game's choice back: inactive, nothing written.
    assert_eq!(r["tex.anisotropy"].value, json!("app"));
    assert!(!r["tex.anisotropy"].active());
    assert_eq!(scopes.revisions(), TweakRevisions { global: 1, game: 1, profile: 2 });
    // Seen from the game scope, the profile's 120 isn't there; the row knows it's overridden.
    let st = state(s, &scopes, ScopeKind::Game, None, None, None);
    let row = &st.options["fps.cap"];
    assert_eq!(row.value, json!(160.0));
    assert_eq!(row.tweak.as_ref().unwrap().overridden_by, Some(ScopeKind::Profile));
    assert!(matches!(row.from, ValueSource::Scope { scope: ScopeKind::Global, .. }));
    assert_eq!(st.kind, quiver_api::options::SurfaceKind::Platform);
    assert_eq!(st.pages[0].id, "frames");
    // Global only.
    let g = resolve(s, &Scopes::load(data, None, None).unwrap(), ScopeKind::Global);
    assert_eq!(g["fps.cap"].value, json!(160.0));
    assert_eq!(g["sync.vsync"].value, json!("app"));
}

#[test]
fn ops_validate_reset_and_presets() {
    let s = schema();
    let cur = OptionsRevision::default();
    let op = |id: &str, v: Value| OptionOp::Set { surface: "quiver".into(), id: id.into(), value: v };
    let a = apply_ops(s, ScopeKind::Global, &cur, &BTreeMap::new(), &[op("fps.cap", json!("vrr")), op("fps.cap", json!(5)), op("nope", json!(1)), op("sync.vsync", json!("sometimes"))]);
    assert_eq!(a.values["fps.cap"], json!("vrr"));
    assert!(a.outcomes[0].1.is_none());
    assert!(matches!(&a.outcomes[1].1, Some(Outcome::Rejected { reason }) if reason.contains("below the minimum 20")));
    assert!(matches!(&a.outcomes[2].1, Some(Outcome::Rejected { reason }) if reason.contains("no tweak nope")));
    assert!(matches!(&a.outcomes[3].1, Some(Outcome::Rejected { .. })));
    let cur = OptionsRevision { values: [(SURFACE_KEY.to_owned(), a.values.clone())].into(), ..Default::default() };
    let b = apply_ops(s, ScopeKind::Global, &cur, &BTreeMap::new(), &[OptionOp::Preset { surface: "quiver".into(), id: "competitive".into() }, OptionOp::Reset { surface: "quiver".into(), id: "fps.cap".into() }]);
    assert_eq!(b.values["sync.max_latency"], json!("1"));
    assert!(!b.values.contains_key("fps.cap"), "{:?}", b.values);
    assert!(b.changes[0].starts_with("Global: preset Competitive"), "{:?}", b.changes);
    let p = apply_prefer(s, ScopeKind::Game, &cur, "fps.cap", Some(Prefer::Quiver)).unwrap();
    assert_eq!(p.prefer["fps.cap"], json!("quiver"));
    assert!(apply_prefer(s, ScopeKind::Game, &cur, "nope", None).is_err());
}

#[test]
fn dxvk_entries_merge_after_the_person_s_and_their_keys_win() {
    let c = plan_with(json!({"fps.cap": 160, "sync.vsync": "off", "tex.lod_bias": "clamp"}), &nvidia(50), &[("DXVK_CONFIG", "dxgi.syncInterval = 1"), ("PROTON_ENABLE_NVAPI", "1")]);
    assert_eq!(c.plan.env["DXVK_CONFIG"], "dxgi.syncInterval = 1; d3d9.maxFrameRate = 160; dxgi.maxFrameRate = 160; d3d11.clampNegativeLodBias = True; d3d9.clampNegativeLodBias = True");
    assert!(matches!(planned(&c, "fps.cap"), Planned::Apply { mech, writes, .. } if mech == "dxvk" && writes[0] == "DXVK_CONFIG: d3d9.maxFrameRate = 160"));
    assert!(matches!(planned(&c, "sync.vsync"), Planned::Superseded { by } if by == "your DXVK_CONFIG sets dxgi.syncInterval = 1"));
    assert_eq!(c.plan.env["DXVK_LOG_PATH"], "Z:/s/logs/");
    assert!(!c.plan.env.contains_key("DXVK_NVAPI_LOG_PATH"), "no nvapi tweak, no nvapi log");
    assert_eq!(dxvk_entries(&c.plan.env["DXVK_CONFIG"]).len(), 5);
    // DXVK's own dxvk.maxFrameRate beats dxgi.maxFrameRate (spike B).
    let c = plan_with(json!({"fps.cap": 160}), &nvidia(50), &[("DXVK_CONFIG", "dxvk.maxFrameRate=90")]);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Superseded { by } if by.contains("dxvk.maxFrameRate = 90")));
    assert!(!c.plan.env.contains_key("DXVK_CONFIG"));
}

#[test]
fn dlss_overrides_go_through_dxvk_nvapi_drs_variables() {
    let c = plan_with(json!({"dlss.sr.model": "K", "dlss.sr.scale": "custom", "dlss.sr.ratio": 58, "dlss.indicator": true}), &nvidia(50), &[]);
    let env = &c.plan.env;
    assert_eq!(env["DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE"], "on");
    assert_eq!(env["DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION"], "render_preset_K");
    assert_eq!(env["DXVK_NVAPI_DRS_SL_DLSS_OVERRIDE"], "on", "Streamline games, dxvk-nvapi 0.9.3");
    assert_eq!(env["DXVK_NVAPI_DRS_NGX_DLSS_SR_MODE"], "custom");
    assert_eq!(env["DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_SCALING_RATIO"], "58");
    assert_eq!(env["DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS"], "DLSSIndicator=1024,DLSSGIndicator=2");
    assert_eq!(env["PROTON_ENABLE_NVAPI"], "1");
    assert_eq!((env["DXVK_NVAPI_LOG_LEVEL"].as_str(), env["DXVK_NVAPI_LOG_PATH"].as_str()), ("info", "Z:/s/logs/"));
    assert!(!env.contains_key("DXVK_NVAPI_DRS_SETTINGS"), "never the shared variable");
    assert!(c.indicator_on);
    // "latest".
    let c = plan_with(json!({"dlss.sr.model": "latest"}), &nvidia(40), &[]);
    assert_eq!(c.plan.env["DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION"], "render_preset_latest");
    // The person's own DRS settings win, also through DXVK_NVAPI_DRS_SETTINGS.
    let c = plan_with(json!({"dlss.sr.model": "M"}), &nvidia(50), &[("DXVK_NVAPI_DRS_SETTINGS", "ngx_dlss_sr_override=off")]);
    assert!(matches!(planned(&c, "dlss.sr.model"), Planned::Superseded { by } if by.contains("ngx_dlss_sr_override=off")));
    // Not NVIDIA: says so.
    let c = plan_with(json!({"dlss.sr.model": "K", "fps.cap": 60}), &amd(), &[]);
    assert!(matches!(planned(&c, "dlss.sr.model"), Planned::Unavailable { reason } if reason == "NVIDIA only"));
    assert!(matches!(planned(&c, "fps.cap"), Planned::Apply { .. }), "caps work everywhere");
    // An old dxvk-nvapi.
    let mut old = nvidia(50);
    old.proton.as_mut().unwrap().nvapi = Some("v0.7.1-94-gafb59a8".into());
    let c = plan_with(json!({"dlss.sr.model": "K"}), &old, &[]);
    assert!(matches!(planned(&c, "dlss.sr.model"), Planned::Unavailable { reason } if reason.contains("needs dxvk-nvapi 0.9.0 (this Proton has v0.7.1")));
}

#[test]
fn frame_generation_needs_the_right_gpu_and_dynamic_blocks_the_cap() {
    let c = plan_with(json!({"dlss.fg.frames": "4x"}), &nvidia(40), &[]);
    assert!(matches!(planned(&c, "dlss.fg.frames"), Planned::Unavailable { reason } if reason.contains("needs an RTX 50")), "{:?}", planned(&c, "dlss.fg.frames"));
    let c = plan_with(json!({"dlss.fg.frames": "2x"}), &nvidia(40), &[]);
    assert_eq!(c.plan.env["DXVK_NVAPI_DRS_NGX_DLSSG_MULTI_FRAME_COUNT"], "1");
    let c = plan_with(json!({"dlss.fg.frames": "4x"}), &nvidia(50), &[]);
    assert_eq!(c.plan.env["DXVK_NVAPI_DRS_NGX_DLSSG_MULTI_FRAME_COUNT"], "3");
    let c = plan_with(json!({"dlss.fg.frames": "dynamic", "dlss.fg.target": 0, "fps.cap": 120, "fg.smooth_motion": true}), &nvidia(50), &[]);
    assert_eq!(c.plan.env["DXVK_NVAPI_DRS_NGX_DLSSG_MODE"], "dynamic");
    assert!(matches!(planned(&c, "fps.cap"), Planned::Blocked { reason } if reason.contains("Dynamic Frame Generation")));
    assert!(matches!(planned(&c, "fg.smooth_motion"), Planned::Blocked { reason } if reason.contains("Smooth Motion")));
    // Smooth Motion alone: the driver's variable, and its log into the session dir.
    let c = plan_with(json!({"fg.smooth_motion": true}), &nvidia(50), &[]);
    assert_eq!(c.plan.env["NVPRESENT_ENABLE_SMOOTH_MOTION"], "1");
    assert_eq!(c.plan.env["NVPRESENT_LOG_FILE"], "/s/logs/nvpresent.log");
    let mut old = nvidia(40);
    old.driver = Some("575.64".into());
    let c = plan_with(json!({"fg.smooth_motion": true}), &old, &[]);
    assert!(matches!(planned(&c, "fg.smooth_motion"), Planned::Unavailable { reason } if reason.contains("needs driver 580.82 (this is 575.64)")), "{:?}", planned(&c, "fg.smooth_motion"));
}

#[test]
fn low_latency_follows_the_option_names_the_build_has() {
    let c = plan_with(json!({"latency.mode": "on"}), &nvidia(50), &[]);
    assert!(c.plan.env["DXVK_CONFIG"].contains("dxvk.latencySleep = True"));
    let mut exp = nvidia(50);
    exp.proton = Some(experimental());
    let c = plan_with(json!({"latency.mode": "on"}), &exp, &[]);
    assert!(matches!(planned(&c, "latency.mode"), Planned::Unavailable { reason } if reason.contains("doesn't know dxvk.latencySleep")), "{:?}", planned(&c, "latency.mode"));
    let c = plan_with(json!({"latency.mode": "off"}), &exp, &[]);
    assert_eq!(c.plan.env["DXVK_CONFIG"], "dxvk.enableNvLowLatency2 = False");
    // AMD, a D3D12 game: Mesa's Anti-Lag layer.
    let mut a = amd();
    a.api = "d3d12".into();
    let c = plan_with(json!({"latency.mode": "on"}), &a, &[]);
    assert_eq!(c.plan.env["ENABLE_LAYER_MESA_ANTI_LAG"], "1");
}

#[test]
fn caps_resolve_vrr_against_the_display_and_say_why_not() {
    let mut caps = nvidia(50);
    let c = plan_with(json!({"fps.cap": "vrr"}), &caps, &[]);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Unavailable { reason } if reason.contains("refresh rate")));
    caps.display = Some(DisplayCaps { name: "eDP-1".into(), refresh_hz: 165.0, vrr: true, hdr: None });
    let c = plan_with(json!({"fps.cap": "vrr"}), &caps, &[]);
    assert!(c.plan.env["DXVK_CONFIG"].contains("dxgi.maxFrameRate = 162"));
    assert_eq!(c.plan.entry("fps.cap").unwrap().display, "refresh rate − 3 (162 fps)");
    // A native Vulkan game: only the Sight layer could (phase 2).
    caps.api = "vulkan".into();
    let c = plan_with(json!({"fps.cap": 100, "tex.anisotropy": "16"}), &caps, &[]);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Unavailable { reason } if reason.contains("only for D3D8–11 and D3D12 games (this one is Vulkan)")), "{:?}", planned(&c, "fps.cap"));
    let fps = c.plan.entry("fps.cap").unwrap();
    assert!(fps.considered.iter().any(|x| x.mech == "layer" && x.why.contains("phase 2")));
}

#[test]
fn the_updater_is_copied_from_the_driver_only_when_it_has_one() {
    let c = plan_with(json!({"dlss.runtime": "updater"}), &nvidia(50), &[]);
    assert!(matches!(planned(&c, "dlss.runtime"), Planned::Unavailable { reason } if reason.contains("your driver package has no nvngx_update.exe")));
    assert!(c.copy_updater.is_none());
    let mut caps = nvidia(50);
    caps.ngx.updater = true;
    let c = plan_with(json!({"dlss.runtime": "updater"}), &caps, &[]);
    assert_eq!(c.plan.env["PROTON_ENABLE_NGX_UPDATER"], "1");
    assert_eq!(c.copy_updater.as_deref(), Some(Path::new("/usr/lib/nvidia/wine/nvngx_update.exe")));
    // Proton Experimental copies it itself.
    caps.proton = Some(experimental());
    assert!(plan_with(json!({"dlss.runtime": "updater"}), &caps, &[]).copy_updater.is_none());
}

#[test]
fn the_indicator_is_undone_and_mirror_switches_warned_about() {
    let c = plan_full(json!({"fps.cap": 60}), &nvidia(50), &[("PROTON_DLSS_UPGRADE", "1")], &[], &[], &BTreeMap::new(), true);
    assert_eq!(c.plan.env["DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS"], "DLSSIndicator=0,DLSSGIndicator=0,");
    assert!(!c.indicator_on);
    assert!(c.plan.warnings.iter().any(|w| w.contains("PROTON_DLSS_UPGRADE=1") && w.contains("third-party mirror")));
    assert!(!c.plan.env.keys().any(|k| super::compile::MIRROR_SWITCHES.contains(&k.as_str())), "never set by Quiver");
    let c = plan_full(json!({}), &nvidia(50), &[("PROTON_DLSS_UPGRADE", "1"), ("PROTONFIXES_DISABLE_UPSCALERS", "1")], &[], &[], &BTreeMap::new(), false);
    assert!(c.plan.warnings.is_empty());
}

/// aco-dlss as a provider, built from its catalog schema.
fn aco_provider(tweak: &str, profile_value: Option<Value>) -> ProviderInfo {
    let (_, aco, _) = crate::options::catalog().into_iter().find(|(id, _, _)| *id == "aco-dlss").unwrap();
    let def = aco.provides[tweak].clone();
    ProviderInfo { tweak: tweak.into(), surface: "m1/aco-dlss".into(), title: "DLSS".into(), option: aco.options.get(&def.option).cloned(), def, profile_value, base: json!(0) }
}

#[test]
fn providers_take_over_and_values_flow_into_their_options() {
    let caps = nvidia(50);
    let provs = [aco_provider("fps.cap", None), aco_provider("dlss.sr.model", None), aco_provider("dlss.sr.scale", None), aco_provider("tex.lod_bias", None)];
    let fg = [ActiveConflict { tweak: None, mech: Some("dxvk:dxgi.maxFrameRate".into()), reason: "FG presents two frames".into(), by: "DLSS".into() }];
    let c = plan_full(json!({"fps.cap": 160, "dlss.sr.model": "latest", "dlss.sr.scale": "custom", "dlss.sr.ratio": 50, "tex.lod_bias": -0.5}), &caps, &[], &provs, &fg, &BTreeMap::new(), false);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Provided { by, option, value: Some(v), mode, can_prefer_quiver: true, .. } if by == "DLSS" && option == "reflex.fps_limit" && v == &json!(160.0) && mode == "bind"));
    let bound: BTreeMap<&str, &Value> = c.plan.bound.iter().map(|b| (b.option.as_str(), &b.value)).collect();
    assert_eq!(bound["upscaler.preset"], &json!("default"));
    assert_eq!(bound["upscaler.render_scale"], &json!(0.5));
    assert!(!bound.contains_key("upscaler.mip_bias"), "mode = own: the mod decides");
    assert!(matches!(planned(&c, "tex.lod_bias"), Planned::Provided { mode, .. } if mode == "own"));
    assert!(c.plan.env.is_empty(), "Quiver's mechanisms stand down: {:?}", c.plan.env);
    // Out of the mod option's range: blocked, not silently clamped.
    let c = plan_full(json!({"fps.cap": 500}), &caps, &[], &provs, &fg, &BTreeMap::new(), false);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Blocked { reason } if reason.contains("above the maximum 360")));
    // Prefer Quiver's: the mod's cap goes off, DXVK's would apply but FG conflicts with it.
    let prefer = [("fps.cap".to_owned(), (Prefer::Quiver, ScopeKind::Game))].into();
    let c = plan_full(json!({"fps.cap": 160}), &caps, &[], &provs, &fg, &prefer, false);
    assert_eq!(c.plan.bound[0].value, json!(0));
    assert!(matches!(planned(&c, "fps.cap"), Planned::Unavailable { reason } if reason.contains("DLSS: FG presents two frames")), "{:?}", planned(&c, "fps.cap"));
    let c = plan_full(json!({"fps.cap": 160}), &caps, &[], &provs, &[], &prefer, false);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Apply { mech, .. } if mech == "dxvk"));
    // A profile's own value for the mod's option wins (shown as such).
    let p = [aco_provider("fps.cap", Some(json!(120)))];
    let c = plan_full(json!({"fps.cap": 160}), &caps, &[], &p, &[], &BTreeMap::new(), false);
    assert!(matches!(planned(&c, "fps.cap"), Planned::Provided { value: Some(v), .. } if v == &json!(120)));
    assert!(c.plan.notes.iter().any(|n| n.contains("wins over the tweak")));
    // Tweak conflicts a provider declares.
    let block = [ActiveConflict { tweak: Some("dlss.fg.frames".into()), mech: None, reason: "one frame per game frame".into(), by: "DLSS".into() }];
    let c = plan_full(json!({"dlss.fg.frames": "4x"}), &caps, &[], &[], &block, &BTreeMap::new(), false);
    assert!(matches!(planned(&c, "dlss.fg.frames"), Planned::Blocked { reason } if reason == "DLSS: one frame per game frame"));
}

#[test]
fn caps_probe_reads_fixture_trees() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    let w = |p: &str, c: &[u8]| {
        let p = r.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    };
    w("proc/driver/nvidia/version", b"NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  620.70.06  Release Build  (builder@host)  Tue Nov 4\nGCC version: gcc 15\n");
    w("proc/driver/nvidia/gpus/0000:01:00.0/information", b"Model: \t\t NVIDIA GeForce RTX 5070 Laptop GPU\nIRQ: 1\n");
    w("sys/class/drm/card0/device/vendor", b"0x8086\n");
    w("sys/class/drm/card1/device/vendor", b"0x10de\n");
    w("sys/class/drm/card1-eDP-1/status", b"connected\n");
    w("usr/lib/nvidia/wine/nvngx.dll", b"x");
    w("usr/lib/nvidia/wine/_nvngx.dll", b"x");
    let m = caps::machine(r);
    assert_eq!(m.driver.as_deref(), Some("620.70.06"));
    let g = m.gpu.unwrap();
    assert_eq!((g.vendor.as_str(), g.rtx_series, g.arch.as_deref()), ("nvidia", Some(50), Some("blackwell")));
    assert_eq!(m.gpus.len(), 2, "{:?}", m.gpus);
    assert!(m.ngx.core && !m.ngx.updater);
    // A Proton build.
    let b = r.join("GE-Proton11-12");
    let w2 = |p: &str, c: &[u8]| {
        let p = b.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    };
    w2("version", b"1793687165 GE-Proton11-12\n");
    w2("proton", b"#!/usr/bin/env python3\n# PROTON_ENABLE_HDR\nfor f in [\"_nvngx.dll\", \"nvngx.dll\"]:\n");
    w2("files/lib/wine/dxvk/version", b"f863776806824486d709fee16ea619c03e22def4 dxvk (v3.1.1-174-gf86377680)\n");
    w2("files/lib/wine/nvapi/version", b"9c0f214cb66b543ed6604ef04c29fde98832dc77 dxvk-nvapi (v0.9.3)\n");
    w2("files/lib/wine/vkd3d-proton/version", b"8293130e vkd3d-proton (vkd3d-1.1-5687-g8293130eb)\n");
    w2("files/lib/wine/dxvk/x86_64-windows/d3d11.dll", b"MZ\0\0dxgi.maxFrameRate\0dxvk.latencySleep\0");
    w2("files/lib/wine/nvapi/x86_64-windows/nvapi64.dll", b"MZ\0NGX_DLSSG_MODE\0");
    let p = caps::probe_proton(&b, &caps::needles(schema()));
    assert_eq!((p.dxvk.as_deref(), p.nvapi.as_deref(), p.vkd3d.as_deref()), (Some("v3.1.1-174-gf86377680"), Some("v0.9.3"), Some("1.1-5687-g8293130eb")));
    assert_eq!(p.flavour, "ge");
    assert!(p.has.contains(&"dxvk:dxvk.latencySleep".into()) && p.has.contains(&"nvapi:NGX_DLSSG_MODE".into()) && p.has.contains(&"proton:PROTON_ENABLE_HDR".into()));
    assert!(!p.has.contains(&"dxvk:dxvk.enableNvLowLatency2".into()) && !p.copies_updater);
    let cached = caps::probe_proton_cached(r, &b, &caps::needles(schema()));
    assert_eq!(cached, p);
    assert!(r.join("tweaks/probe-cache.json").is_file());
    // Versions.
    assert!(caps::version_holds(">=0.9.0", "v0.9.3") && !caps::version_holds(">=0.9.3", "v0.9.0-10-g0ea2317"));
    assert!(caps::version_holds(">=580.82", "620.70.06") && !caps::version_holds(">=580.82", "575.64.05"));
    assert_eq!(caps::rtx_series("NVIDIA GeForce RTX 4090"), Some(40));
    assert_eq!(caps::rtx_series("NVIDIA GeForce GTX 1080"), None);
}

/// Log lines as DXVK v3.1.1 and dxvk-nvapi v0.9.3 print them (spike A1, B1).
const DXGI_LOG: &str = "info:  Game: ACOrigins.exe\ninfo:  DXVK: v3.1.1-174-gf86377680682448-GE-Proton(dirty)\ninfo:  Found config env: dxgi.maxFrameRate = 160; d3d9.maxFrameRate = 160\ninfo:  Effective configuration:\ninfo:    d3d9.maxFrameRate = 160\ninfo:    dxgi.maxFrameRate = 160\ninfo:  Using feature level D3D_FEATURE_LEVEL_11_1\n";
const NVAPI_LOG: &str = "---------- 2026-11-17 21:04:10 ----------\n416463.618:013c:0140:info:nvapi64:DXVK-NVAPI v0.9.3 NVAPI gcc 14.0.0 x86_64 plain (ACOrigins.exe)\n416463.706:013c:0140:info:nvapi64:<-NvAPI_Initialize: OK\n416463.906:013c:0140:info:nvapi64:Applying the following DRS settings when requested by the application (2 total):\n416463.906:013c:0140:info:nvapi64:    0x10e41e01/Enable DLSS-SR override = 0x1\n416463.906:013c:0140:info:nvapi64:    0x10e41df3/Override DLSS-SR presets = 0xb\n416463.906:013c:0140:info:nvapi64:<-NvAPI_DRS_GetSetting (0x10e41df3/Override DLSS-SR presets = 0xb): OK\n";

#[test]
fn evidence_comes_from_fixed_log_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let c = plan_with(json!({"fps.cap": 160, "dlss.sr.model": "K", "dlss.rr.model": "latest", "sync.max_latency": "1"}), &nvidia(50), &[]);
    let mut effects: Vec<TweakEffect> = initial_effects(&c.plan);
    assert!(effects.iter().all(|e| e.effect == Effect::Applied), "{effects:?}");
    // Nothing logged yet.
    assert!(scan(&c.plan, &mut effects, &Logs::read(dir)).is_empty());
    std::fs::write(dir.join("ACOrigins_dxgi.log"), DXGI_LOG).unwrap();
    std::fs::write(dir.join("nvapi64.log"), NVAPI_LOG).unwrap();
    let found = scan(&c.plan, &mut effects, &Logs::read(dir));
    assert_eq!(found, ["fps.cap", "dlss.sr.model"]);
    let e = |effects: &[TweakEffect], id: &str| effects.iter().find(|e| e.id == id).unwrap().effect.clone();
    assert!(matches!(e(&effects, "fps.cap"), Effect::Confirmed { evidence, .. } if evidence.contains("DXVK's effective configuration has") && evidence.contains("ACOrigins_dxgi.log")));
    assert!(matches!(e(&effects, "dlss.sr.model"), Effect::Confirmed { evidence, .. } if evidence.contains("Override DLSS-SR presets") && evidence.contains("0xb")));
    finish(&c.plan, &mut effects, &Logs::read(dir), None);
    assert!(matches!(e(&effects, "dlss.rr.model"), Effect::Unconfirmed { why } if why.contains("didn't ask for this setting")));
    assert!(matches!(e(&effects, "sync.max_latency"), Effect::Unconfirmed { why } if why.contains("don't list")), "{:?}", e(&effects, "sync.max_latency"));
    // No logs at all.
    let empty = tempfile::tempdir().unwrap();
    let mut effects = initial_effects(&c.plan);
    finish(&c.plan, &mut effects, &Logs::read(empty.path()), None);
    assert!(matches!(&effects[0].effect, Effect::Unconfirmed { why } if why.contains("no DXVK log")));
    // Plan-only effects.
    let c = plan_with(json!({"dlss.sr.model": "K"}), &amd(), &[]);
    assert!(matches!(&initial_effects(&c.plan)[0].effect, Effect::Unavailable { reason } if reason == "NVIDIA only"));
}

#[test]
fn templates_and_arithmetic() {
    let t: BTreeMap<String, Value> = [("dlss.sr.ratio".to_owned(), json!(58))].into();
    assert_eq!(super::compile::render_for_test("render_preset_{v}", "K", &t), "render_preset_K");
    assert_eq!(super::compile::render_for_test("{dlss.sr.ratio}", "custom", &t), "58");
    let p = aco_provider("dlss.sr.scale", None);
    assert_eq!(provider_value(&p, &json!("custom"), &t).unwrap(), Some(json!(0.58)));
    assert_eq!(provider_value(&p, &json!("app"), &t).unwrap(), None);
}

#[test]
fn the_api_the_layer_saw_last_feeds_the_next_plan() {
    assert_eq!((api_of_engine("DXVK"), api_of_engine("vkd3d"), api_of_engine("idTech"), api_of_engine("")), (Some("dxvk"), Some("d3d12"), Some("vulkan"), None));
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();
    let game = GameRef::ubisoft(1);
    let at = At::Global(data);
    let a = apply_ops(schema(), ScopeKind::Global, &OptionsRevision::default(), &BTreeMap::new(), &[OptionOp::Set { surface: "quiver".into(), id: "tex.anisotropy".into(), value: json!("16") }]);
    at.append(a.values, a.prefer, "cli", None, a.changes).unwrap();
    let plan = |data: &Path| plan_for(data, &game, None, nvidia(50), &BTreeMap::new(), None).unwrap().plan;
    assert!(matches!(&plan(data).entry("tex.anisotropy").unwrap().planned, Planned::Apply { .. }));
    save_api(data, &game, "d3d12").unwrap();
    let p = plan(data);
    assert_eq!(p.caps.api, "d3d12");
    assert!(matches!(&p.entry("tex.anisotropy").unwrap().planned, Planned::Unavailable { reason } if reason.contains("this one is D3D12")), "{:?}", p.entry("tex.anisotropy"));
    // The indicator state survives the API being recorded.
    save_prefix_state(data, &game, &PrefixState { indicator_on: true, ..load_prefix_state(data, &game) }).unwrap();
    save_api(data, &game, "dxvk").unwrap();
    assert!(load_prefix_state(data, &game).indicator_on);
}
