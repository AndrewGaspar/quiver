//! Mechanisms (design/TWEAKS.md §3.3): each tweak's `via` bindings tried in order against the
//! capability set, compiled into what a launch writes.
//!
//! | `mech` | Writes |
//! |---|---|
//! | `env` | environment variables |
//! | `dxvk` | entries merged into `DXVK_CONFIG` after the person's own (`k = v; k = v`) |
//! | `vkd3d` | `VKD3D_*` variables, `VKD3D_CONFIG` flags merged with the person's |
//! | `nvapi-drs` | one `DXVK_NVAPI_DRS_<KEY>` per setting (never `DXVK_NVAPI_DRS_SETTINGS`) |
//! | `ngx` | `PROTON_ENABLE_NGX_UPDATER`, `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS`; the updater copy |
//! | (provider) | a mod's option, through the bound default layer ([`super::bound_for`]) |
//!
//! Merging rules: the person's launch settings (Quiver's `env`, the game's settings, mods'
//! `[launch]`) win per key, and the tweak says it was superseded. GE-Proton's convenience
//! switches (`PROTON_DLSS_UPGRADE`, …) are never set (steer 5); when the person's settings set
//! them, the plan warns. Evidence needs logs, so a launch with a `dxvk` or `nvapi-drs` tweak
//! points `DXVK_LOG_PATH` / `DXVK_NVAPI_LOG_PATH` at the session's log dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use quiver_api::options::{Apply, Requires, Schema, ViaDef, ViaValue, WhenCond, cond_holds, same_value, scalar_text};
use quiver_api::tweaks::{BoundValue, Capabilities, Considered, Planned, Prefer, ScopeKind, TweakPlanEntry, TweaksPlan};

use super::caps::version_holds;
use super::{ActiveConflict, ProviderInfo, Provision, Resolved, concrete, display_value, provision};

/// The Quiver phase this build implements (bindings with a later `phase` are ignored).
pub const PHASE: u32 = 1;
/// Mechanisms the schema may name.
pub const KNOWN: &[&str] = &["env", "dxvk", "vkd3d", "nvapi-drs", "ngx", "layer", "vk-layer", "wrapper", "mangohud", "mod"];
/// Mechanisms this build implements.
pub const BUILT: &[&str] = &["env", "dxvk", "vkd3d", "nvapi-drs", "ngx"];

/// GE-Proton switches that download vendor DLLs from a third-party mirror (TWEAKS §9 rule 3).
pub const MIRROR_SWITCHES: &[&str] =
    &["PROTON_DLSS_UPGRADE", "PROTON_XESS_UPGRADE", "PROTON_FSR3_UPGRADE", "PROTON_FSR4_UPGRADE", "PROTON_STREAMLINE_UPGRADE", "PROTON_USE_OPTISCALER", "PROTON_MLFG_UPGRADE"];

/// What a launch's tweaks are compiled from.
pub struct Inputs<'a> {
    pub schema: &'a Schema,
    pub resolved: &'a BTreeMap<String, Resolved>,
    pub prefer: &'a BTreeMap<String, (Prefer, ScopeKind)>,
    pub caps: &'a Capabilities,
    pub providers: &'a [ProviderInfo],
    pub conflicts: &'a [ActiveConflict],
    /// The launch's environment before tweaks (Quiver's `env`, the game's settings, mods).
    pub user_env: &'a BTreeMap<String, String>,
    /// The session's log dir (a launch; `None` for `tweaks plan`).
    pub log_dir: Option<&'a Path>,
    /// Quiver turned the DLSS indicator on in this prefix earlier.
    pub indicator_was_on: bool,
}

/// A compiled plan plus what the launcher does with files.
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    pub plan: TweaksPlan,
    /// Copy the driver's `nvngx_update.exe` (this file) into the prefix's `system32`.
    pub copy_updater: Option<PathBuf>,
    /// The DLSS indicator is on for this launch.
    pub indicator_on: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Write {
    Env(String, String),
    Dxvk(String, String),
    Vkd3dFlag(String),
}

impl Write {
    fn describe(&self) -> String {
        match self {
            Write::Env(k, v) => format!("{k}={v}"),
            Write::Dxvk(k, v) => format!("DXVK_CONFIG: {k} = {v}"),
            Write::Vkd3dFlag(f) => format!("VKD3D_CONFIG: {f}"),
        }
    }
}

/// `k = v` entries of a `DXVK_CONFIG` value.
pub fn dxvk_entries(v: &str) -> Vec<(String, String)> {
    v.split(';')
        .filter_map(|e| {
            let (k, v) = e.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_owned(), v.trim().to_owned()))
        })
        .collect()
}

fn api_label(api: &str) -> &str {
    match api {
        "dxvk" => "D3D8–11",
        "d3d12" => "D3D12",
        "vulkan" => "Vulkan",
        "opengl" => "OpenGL",
        _ => "unknown",
    }
}

fn api_matches(want: &str, have: &str) -> bool {
    let w = match want {
        "d3d8" | "d3d9" | "d3d10" | "d3d11" => "dxvk",
        w => w,
    };
    w.eq_ignore_ascii_case(have)
}

fn series_word(s: u32) -> String {
    if s >= 50 { format!("an RTX {s}") } else { format!("an RTX {s} or newer") }
}

/// Whether a binding's `requires` holds; `Err` says why not, in words.
pub fn requires(r: &Requires, caps: &Capabilities, tweaks: &BTreeMap<String, Value>, defaults: &BTreeMap<String, Value>) -> Result<(), String> {
    let gpu = caps.gpu.as_ref();
    if let Some(v) = &r.vendor
        && !gpu.is_some_and(|g| g.vendor.eq_ignore_ascii_case(v))
    {
        let name = match v.as_str() {
            "nvidia" => "NVIDIA",
            "amd" => "AMD",
            "intel" => "Intel",
            o => o,
        };
        return Err(format!("{name} only"));
    }
    if r.rtx == Some(true) && !gpu.is_some_and(|g| g.rtx_series.is_some() || g.name.to_ascii_uppercase().contains("RTX")) {
        return Err("needs an NVIDIA RTX GPU".into());
    }
    if let Some(s) = r.rtx_series
        && gpu.and_then(|g| g.rtx_series).is_none_or(|have| have < s)
    {
        return Err(format!("needs {}", series_word(s)));
    }
    if let Some(req) = &r.driver {
        match &caps.driver {
            None => return Err("the driver version isn't known".into()),
            Some(d) if !version_holds(req, d) => return Err(format!("needs driver {} (this is {d})", req.trim_start_matches(['>', '='])) ),
            _ => {}
        }
    }
    let proton = caps.proton.as_ref();
    // Steam picks the Proton build on the `steam-run` route: build tests can't be checked,
    // so they're assumed to hold (the plan notes it).
    let unknown_build = proton.is_none();
    for (req, have, what) in [(&r.nvapi, proton.and_then(|p| p.nvapi.as_ref()), "dxvk-nvapi"), (&r.dxvk, proton.and_then(|p| p.dxvk.as_ref()), "DXVK"), (&r.vkd3d, proton.and_then(|p| p.vkd3d.as_ref()), "vkd3d-proton")] {
        let Some(req) = req else { continue };
        match have {
            _ if unknown_build => {}
            None => return Err(format!("this Proton has no {what}")),
            Some(h) if !version_holds(req, h) => return Err(format!("needs {what} {} (this Proton has {h})", req.trim_start_matches(['>', '=']))),
            _ => {}
        }
    }
    if !unknown_build {
        let has = &proton.unwrap().has;
        if let Some(missing) = r.has.iter().find(|n| !has.contains(n)) {
            let (comp, name) = missing.split_once(':').unwrap_or(("", missing));
            let comp = match comp {
                "dxvk" => "DXVK",
                "nvapi" => "dxvk-nvapi",
                "vkd3d" => "vkd3d-proton",
                _ => "Proton",
            };
            return Err(format!("this Proton's {comp} doesn't know {name}"));
        }
        if let Some(found) = r.lacks.iter().find(|n| has.contains(n)) {
            return Err(format!("this Proton has {found}"));
        }
    }
    if !r.api.is_empty() && !r.api.iter().any(|a| api_matches(a, &caps.api)) {
        let want: Vec<&str> = r.api.iter().map(|a| api_label(if a.starts_with("d3d") && a != "d3d12" { "dxvk" } else { a })).filter(|l| *l != "unknown").collect();
        let mut want = want;
        want.dedup();
        return Err(format!("only for {} games (this one is {})", want.join(" and "), api_label(&caps.api)));
    }
    if r.dxgi.as_deref() == Some("not-dxvk") && proton.is_none_or(|p| p.dxvk.is_some()) {
        return Err("DXVK's dxgi handles this game's swapchain (the DXVK binding applies)".into());
    }
    if let Some(l) = r.layer
        && l != caps.layer
    {
        return Err(if l { "needs the Sight layer".into() } else { "not with the Sight layer".into() });
    }
    if r.session.as_deref() == Some("running") {
        return Err("only for a change made while the game runs".into());
    }
    if !r.route.is_empty() && !r.route.iter().any(|x| x.eq_ignore_ascii_case(&caps.route)) {
        return Err(format!("not on the {} route", if caps.route.is_empty() { "unknown" } else { &caps.route }));
    }
    for f in &r.ngx {
        let ok = match f.as_str() {
            "nvngx_update.exe" => caps.ngx.updater,
            "nvngx.dll" | "_nvngx.dll" => caps.ngx.core,
            _ => caps.ngx.dir.as_ref().is_some_and(|d| d.join(f).is_file()),
        };
        if !ok {
            return Err(match caps.ngx.dir.as_ref() {
                Some(d) => format!("your driver package has no {f} (in {})", d.display()),
                None => format!("the driver's NGX files aren't installed (no {f})"),
            });
        }
    }
    if let Some(d) = &r.display {
        let disp = caps.display.as_ref();
        if d.vrr == Some(true) && !disp.is_some_and(|x| x.vrr) {
            return Err("needs VRR on the game's monitor".into());
        }
        if d.hdr == Some(true) && !disp.and_then(|x| x.hdr).unwrap_or(false) {
            return Err("needs HDR on the game's monitor".into());
        }
    }
    for (id, c) in &r.tweak {
        let v = tweaks.get(id).cloned().unwrap_or(Value::Null);
        let holds = match c {
            WhenCond::One(Value::String(s)) if s == "@on" => !v.is_null() && !defaults.get(id).is_some_and(|d| same_value(d, &v)),
            c => cond_holds(c, &v),
        };
        if !holds {
            return Err(format!("needs {id}"));
        }
    }
    if r.anti_cheat == Some(false) && caps.anti_cheat {
        return Err("off for anti-cheat games".into());
    }
    if !r.any.is_empty() {
        let whys: Vec<String> = r.any.iter().filter_map(|a| requires(a, caps, tweaks, defaults).err()).collect();
        if whys.len() == r.any.len() {
            return Err(whys.join(", or "));
        }
    }
    Ok(())
}

/// `{v}` and `{<tweak id>}` in a template.
fn render(t: &str, v: &str, tweaks: &BTreeMap<String, Value>) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let name = &rest[i + 1..i + j];
        match name {
            "v" => out.push_str(v),
            id => match tweaks.get(id) {
                Some(x) => out.push_str(&scalar_text(x)),
                None => out.push_str(&rest[i..=i + j]),
            },
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}

/// What a binding writes for a value: `Ok(None)` for `@unset`, `Err` for `@skip`.
fn writes_for(b: &ViaDef, text: &str, tweaks: &BTreeMap<String, Value>) -> Result<Option<Vec<Write>>, ()> {
    let set: BTreeMap<String, String> = match b.map.iter().find(|(k, _)| k.eq_ignore_ascii_case(text)).map(|(_, v)| v) {
        Some(ViaValue::Text(t)) if t == "@unset" => return Ok(None),
        Some(ViaValue::Text(t)) if t == "@skip" => return Err(()),
        Some(ViaValue::Text(t)) => b.set.iter().map(|(k, tmpl)| (k.clone(), render(tmpl, t, tweaks))).collect(),
        Some(ViaValue::Set(m)) => m.iter().map(|(k, tmpl)| (k.clone(), render(tmpl, text, tweaks))).collect(),
        None => b.set.iter().map(|(k, tmpl)| (k.clone(), render(tmpl, text, tweaks))).collect(),
    };
    let mut out: Vec<Write> = set
        .into_iter()
        .map(|(k, v)| match b.mech.as_str() {
            "dxvk" => Write::Dxvk(k, v),
            "nvapi-drs" => Write::Env(format!("DXVK_NVAPI_DRS_{}", k.to_ascii_uppercase()), v),
            _ => Write::Env(k, v),
        })
        .collect();
    if b.mech == "vkd3d" {
        out.extend(b.flags.iter().map(|f| Write::Vkd3dFlag(f.clone())));
    }
    Ok(Some(out))
}

fn mech_conflict<'a>(b: &ViaDef, writes: &[Write], conflicts: &'a [ActiveConflict]) -> Option<&'a ActiveConflict> {
    conflicts.iter().find(|c| {
        let Some(m) = &c.mech else { return false };
        let (mech, what) = m.split_once(':').map_or((m.as_str(), None), |(a, b)| (a, Some(b)));
        if mech != b.mech {
            return false;
        }
        match what {
            None => true,
            Some(w) => match b.mech.as_str() {
                "layer" => b.control.as_deref().is_some_and(|c| c.starts_with(w)),
                _ => writes.iter().any(|x| match x {
                    Write::Dxvk(k, _) => k.eq_ignore_ascii_case(w),
                    Write::Env(k, _) => k.eq_ignore_ascii_case(w) || k.eq_ignore_ascii_case(&format!("DXVK_NVAPI_DRS_{w}")),
                    Write::Vkd3dFlag(f) => f.eq_ignore_ascii_case(w),
                }),
            },
        }
    })
}

/// The person's setting that wins over a write, in words.
fn superseded(w: &Write, user: &BTreeMap<String, String>, user_dxvk: &[(String, String)]) -> Option<String> {
    match w {
        Write::Env(k, _) => {
            if let Some(v) = user.get(k) {
                return Some(format!("your launch setting {k}={v}"));
            }
            let drs = k.strip_prefix("DXVK_NVAPI_DRS_")?;
            let settings = user.get("DXVK_NVAPI_DRS_SETTINGS")?;
            settings
                .split(',')
                .filter_map(|e| e.split_once('='))
                .find(|(n, _)| n.trim().eq_ignore_ascii_case(drs))
                .map(|(n, v)| format!("your launch setting DXVK_NVAPI_DRS_SETTINGS sets {}={}", n.trim(), v.trim()))
        }
        Write::Dxvk(k, _) => {
            if let Some((uk, uv)) = user_dxvk.iter().find(|(uk, _)| uk.eq_ignore_ascii_case(k)) {
                return Some(format!("your DXVK_CONFIG sets {uk} = {uv}"));
            }
            // DXVK's own `dxvk.maxFrameRate` beats `dxgi.`/`d3d9.maxFrameRate` (spike B).
            if k.ends_with(".maxFrameRate")
                && let Some((_, uv)) = user_dxvk.iter().find(|(uk, _)| uk.eq_ignore_ascii_case("dxvk.maxFrameRate"))
            {
                return Some(format!("your DXVK_CONFIG sets dxvk.maxFrameRate = {uv}, which DXVK prefers"));
            }
            None
        }
        Write::Vkd3dFlag(_) => None,
    }
}

/// A host path as Wine sees it, as a folder (`Z:/…/`): the form DXVK and dxvk-nvapi accept.
pub fn wine_dir(p: &Path) -> String {
    format!("Z:{}/", p.display())
}

fn truthy(v: &str) -> bool {
    !v.is_empty() && v != "0"
}

/// Compile a launch's (or a preview's) tweaks.
pub fn compile(i: &Inputs<'_>) -> Compiled {
    let mut out = Compiled::default();
    let plan = &mut out.plan;
    plan.caps = i.caps.clone();
    let tweaks: BTreeMap<String, Value> = i.resolved.iter().map(|(id, r)| (id.clone(), concrete(id, &r.value, i.caps.display.as_ref()).unwrap_or_else(|_| r.value.clone()))).collect();
    let defaults: BTreeMap<String, Value> = i.schema.options.iter().filter_map(|(id, d)| d.default.clone().map(|v| (id.clone(), v))).collect();
    let user_dxvk = i.user_env.get("DXVK_CONFIG").map(|v| dxvk_entries(v)).unwrap_or_default();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut dxvk: Vec<(String, String)> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    // Page order, then the rest.
    let mut order: Vec<&String> = i.schema.pages.iter().flat_map(|p| p.options.iter().chain(p.groups.iter().flat_map(|g| g.options.iter()))).collect();
    for id in i.schema.options.keys() {
        if !order.contains(&id) {
            order.push(id);
        }
    }
    for id in order {
        let Some(r) = i.resolved.get(id) else { continue };
        if r.set.is_none() {
            continue;
        }
        let def = &r.def;
        let display = display_value(id, def, &r.value, i.caps.display.as_ref());
        let mut entry = TweakPlanEntry { id: id.clone(), label: def.label_or(id).to_owned(), value: r.value.clone(), display, from: r.from.clone(), ..Default::default() };
        if !r.active() {
            plan.entries.push(entry);
            continue;
        }
        let value = match concrete(id, &r.value, i.caps.display.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                entry.planned = Planned::Unavailable { reason: e };
                plan.entries.push(entry);
                continue;
            }
        };
        let text = scalar_text(&value);
        // A provider first (TWEAKS §7.2).
        if let Some(p) = i.providers.iter().find(|p| p.tweak == *id) {
            let pref = i.prefer.get(id).map_or(Prefer::Provider, |x| x.0);
            let can_prefer_quiver = p.def.off.is_some();
            match provision(p, r, pref, &tweaks, i.caps.display.as_ref()) {
                Provision::Off(off) => {
                    plan.bound.push(BoundValue { tweak: id.clone(), surface: p.surface.clone(), option: p.def.option.clone(), value: off.clone(), scope: i.prefer.get(id).map_or(ScopeKind::Global, |x| x.1) });
                    entry.considered.push(Considered { mech: "mod".into(), chosen: false, why: format!("you prefer Quiver's: {} {} = {}", p.title, p.def.option, scalar_text(&off)) });
                    plan.notes.push(format!("{id}: implemented by Quiver; {} {} set to {}", p.title, p.def.option, scalar_text(&off)));
                }
                Provision::Refused(e) => {
                    entry.planned = Planned::Blocked { reason: e };
                    entry.considered.push(Considered { mech: "mod".into(), chosen: false, why: "the value doesn't fit the provider's option".into() });
                    plan.entries.push(entry);
                    continue;
                }
                prov => {
                    let bound = match &prov {
                        Provision::Bind(Some(v)) => {
                            plan.bound.push(BoundValue { tweak: id.clone(), surface: p.surface.clone(), option: p.def.option.clone(), value: v.clone(), scope: r.set.unwrap_or_default() });
                            Some(v.clone())
                        }
                        _ => None,
                    };
                    // The profile's own value for the option wins over the bound one.
                    let value = p.profile_value.clone().or(bound).or_else(|| (!p.base.is_null()).then(|| p.base.clone()));
                    if let Some(pv) = &p.profile_value {
                        plan.notes.push(format!("{id}: {} profile value {} = {} wins over the tweak (set in {}'s options)", p.title, p.def.option, scalar_text(pv), p.title));
                    }
                    let mode = if prov == Provision::Own { "own" } else { "bind" };
                    entry.planned = Planned::Provided { by: p.title.clone(), surface: p.surface.clone(), option: p.def.option.clone(), value, mode: mode.into(), can_prefer_quiver };
                    entry.considered.push(Considered { mech: "mod".into(), chosen: true, why: String::new() });
                    plan.entries.push(entry);
                    continue;
                }
            }
        }
        // Conflicts on the tweak itself.
        if let Some(c) = i.conflicts.iter().find(|c| c.tweak.as_deref() == Some(id.as_str())) {
            entry.planned = Planned::Blocked { reason: if c.by == "Quiver" { c.reason.clone() } else { format!("{}: {}", c.by, c.reason) } };
            plan.entries.push(entry);
            continue;
        }
        let mut chosen: Option<(&ViaDef, Vec<Write>)> = None;
        let mut whys: Vec<String> = Vec::new();
        let mut nothing = false;
        for b in def.via.iter().filter(|b| !b.also) {
            let label = b.mech.clone();
            if b.phase.unwrap_or(1) > PHASE {
                entry.considered.push(Considered { mech: label, chosen: false, why: format!("phase {} (not built yet)", b.phase.unwrap_or(2)) });
                continue;
            }
            if !BUILT.contains(&b.mech.as_str()) {
                entry.considered.push(Considered { mech: label, chosen: false, why: "not built in this Quiver".into() });
                continue;
            }
            if let Err(why) = requires(&b.requires, i.caps, &tweaks, &defaults) {
                whys.push(why.clone());
                entry.considered.push(Considered { mech: label, chosen: false, why });
                continue;
            }
            match writes_for(b, &text, &tweaks) {
                Err(()) => {
                    let why = format!("can't do {}", entry.display);
                    whys.push(match &b.label {
                        Some(l) => format!("{} needs another binding ({l} can't)", entry.display),
                        None => why.clone(),
                    });
                    entry.considered.push(Considered { mech: label, chosen: false, why });
                }
                Ok(None) => {
                    nothing = true;
                    entry.considered.push(Considered { mech: label, chosen: true, why: String::new() });
                    break;
                }
                Ok(Some(w)) => {
                    if let Some(c) = mech_conflict(b, &w, i.conflicts) {
                        let why = format!("{}: {}", c.by, c.reason);
                        whys.push(why.clone());
                        entry.considered.push(Considered { mech: label, chosen: false, why });
                        continue;
                    }
                    entry.considered.push(Considered { mech: label, chosen: true, why: String::new() });
                    chosen = Some((b, w));
                    break;
                }
            }
        }
        if nothing {
            plan.entries.push(entry);
            continue;
        }
        let Some((b, mut writes)) = chosen else {
            whys.dedup();
            entry.planned = Planned::Unavailable {
                reason: if whys.is_empty() { "no mechanism for it in this Quiver yet (needs the Sight layer, phase 2)".into() } else { whys.join("; ") },
            };
            plan.entries.push(entry);
            continue;
        };
        for a in def.via.iter().filter(|a| a.also && a.phase.unwrap_or(1) <= PHASE && BUILT.contains(&a.mech.as_str())) {
            let ok = requires(&a.requires, i.caps, &tweaks, &defaults);
            match (ok, writes_for(a, &text, &tweaks)) {
                (Ok(()), Ok(Some(w))) if mech_conflict(a, &w, i.conflicts).is_none() => {
                    entry.considered.push(Considered { mech: format!("{} (also)", a.mech), chosen: true, why: String::new() });
                    writes.extend(w);
                }
                (Err(why), _) => entry.considered.push(Considered { mech: format!("{} (also)", a.mech), chosen: false, why }),
                _ => {}
            }
        }
        if let Some(by) = writes.iter().find_map(|w| superseded(w, i.user_env, &user_dxvk)) {
            entry.planned = Planned::Superseded { by };
            plan.entries.push(entry);
            continue;
        }
        for w in &writes {
            match w {
                Write::Env(k, v) => {
                    env.insert(k.clone(), v.clone());
                }
                Write::Dxvk(k, v) => {
                    if !dxvk.iter().any(|(x, _)| x.eq_ignore_ascii_case(k)) {
                        dxvk.push((k.clone(), v.clone()));
                    }
                }
                Write::Vkd3dFlag(f) => {
                    if !flags.contains(f) {
                        flags.push(f.clone());
                    }
                }
            }
        }
        if b.mech == "ngx" && b.copy_updater && !i.caps.proton.as_ref().is_some_and(|p| p.copies_updater) {
            out.copy_updater = i.caps.ngx.dir.as_ref().map(|d| d.join("nvngx_update.exe"));
            plan.files.push("copy the driver's nvngx_update.exe into the prefix (drive_c/windows/system32), as Proton Experimental does".into());
        }
        if b.mech == "ngx" && b.indicator {
            out.indicator_on = true;
        }
        for m in std::iter::once(b.mech.as_str()) {
            if !used.iter().any(|u| u == m) {
                used.push(m.to_owned());
            }
        }
        if writes.iter().any(|w| matches!(w, Write::Env(k, _) if k.starts_with("DXVK_NVAPI_DRS_"))) && !used.iter().any(|u| u == "nvapi-drs") {
            used.push("nvapi-drs".into());
        }
        entry.planned = Planned::Apply { mech: b.mech.clone(), writes: writes.iter().map(Write::describe).collect(), apply: b.apply.unwrap_or(Apply::Launch) };
        entry.confirm = b.confirm.iter().cloned().collect();
        plan.entries.push(entry);
    }
    // -- the environment ----------------------------------------------------------------------
    if !dxvk.is_empty() {
        let ours: Vec<String> = dxvk.iter().map(|(k, v)| format!("{k} = {v}")).collect();
        let mut all: Vec<String> = user_dxvk.iter().map(|(k, v)| format!("{k} = {v}")).collect();
        all.extend(ours.iter().cloned());
        env.insert("DXVK_CONFIG".into(), all.join("; "));
        plan.dxvk_config = ours;
    }
    if !flags.is_empty() {
        let mut all: Vec<String> = i.user_env.get("VKD3D_CONFIG").map(|v| v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        for f in flags {
            if !all.contains(&f) {
                all.push(f);
            }
        }
        env.insert("VKD3D_CONFIG".into(), all.join(","));
    }
    let nvapi = used.iter().any(|u| u == "nvapi-drs" || u == "ngx");
    if nvapi && !i.user_env.get("PROTON_ENABLE_NVAPI").is_some_and(|v| truthy(v)) {
        env.insert("PROTON_ENABLE_NVAPI".into(), "1".into());
        plan.notes.push("dxvk-nvapi enabled for this launch (PROTON_ENABLE_NVAPI=1): driver settings reach games through it".into());
    }
    // The indicator persists in the prefix's registry: undo it once (TWEAKS §9 rule 6).
    if i.indicator_was_on && !out.indicator_on && !i.user_env.contains_key("DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS") {
        env.insert("DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS".into(), "DLSSIndicator=0,DLSSGIndicator=0,".into());
        plan.notes.push("the DLSS indicator Quiver turned on earlier is switched off in the prefix's registry".into());
        if !env.contains_key("PROTON_ENABLE_NVAPI") && !i.user_env.get("PROTON_ENABLE_NVAPI").is_some_and(|v| truthy(v)) {
            env.insert("PROTON_ENABLE_NVAPI".into(), "1".into());
        }
    }
    // Evidence: the session's logs.
    if let Some(dir) = i.log_dir {
        if used.iter().any(|u| u == "dxvk") || (env.keys().any(|k| k.starts_with("VKD3D_"))) {
            match i.user_env.get("DXVK_LOG_PATH") {
                Some(p) => plan.notes.push(format!("your DXVK_LOG_PATH ({p}) is kept: DXVK evidence isn't read this session")),
                None => {
                    env.insert("DXVK_LOG_PATH".into(), wine_dir(dir));
                }
            }
            if let Some(l) = i.user_env.get("DXVK_LOG_LEVEL").filter(|l| matches!(l.as_str(), "none" | "error" | "warn")) {
                plan.notes.push(format!("your DXVK_LOG_LEVEL={l} hides DXVK's configuration lines (evidence)"));
            }
        }
        if nvapi {
            if !i.user_env.contains_key("DXVK_NVAPI_LOG_LEVEL") {
                env.insert("DXVK_NVAPI_LOG_LEVEL".into(), "info".into());
            }
            match i.user_env.get("DXVK_NVAPI_LOG_PATH") {
                Some(p) => plan.notes.push(format!("your DXVK_NVAPI_LOG_PATH ({p}) is kept: dxvk-nvapi evidence isn't read this session")),
                None => {
                    env.insert("DXVK_NVAPI_LOG_PATH".into(), wine_dir(dir));
                }
            }
        }
        if env.contains_key("NVPRESENT_ENABLE_SMOOTH_MOTION") && !i.user_env.contains_key("NVPRESENT_LOG_FILE") {
            env.insert("NVPRESENT_LOG_FILE".into(), dir.join("nvpresent.log").display().to_string());
        }
        plan.log_dir = Some(dir.to_owned());
    }
    // -- warnings -----------------------------------------------------------------------------
    for s in MIRROR_SWITCHES {
        if let Some(v) = i.user_env.get(*s).filter(|v| truthy(v))
            && !i.user_env.get("PROTONFIXES_DISABLE_UPSCALERS").is_some_and(|d| truthy(d))
        {
            plan.warnings.push(format!(
                "your launch settings set {s}={v}: GE-Proton then downloads vendor DLLs from a third-party mirror (loathingkernel.github.io/proton-upscalers). Quiver never sets it; newer DLSS through NVIDIA's own updater is the DLSS version tweak"
            ));
        }
    }
    if i.caps.route == "steam" && i.caps.proton.is_none() {
        plan.notes.push("Steam picks the Proton build for this launch: its component versions weren't checked".into());
    }
    plan.env = env;
    out
}

#[cfg(test)]
pub(crate) fn render_for_test(t: &str, v: &str, tweaks: &BTreeMap<String, Value>) -> String {
    render(t, v, tweaks)
}
