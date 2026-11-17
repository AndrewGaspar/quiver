//! Did it take effect? (design/TWEAKS.md §6). Only fixed, known lines are looked for in the
//! session's logs; the wording says exactly what the line says and never more.
//!
//! | Log | Files | Proves |
//! |---|---|---|
//! | DXVK | `<exe>_dxgi.log`, `_d3d11.log`, `_d3d9.log`, `_d3d8.log` (`DXVK_LOG_PATH`, truncated per process) | `Effective configuration:` lists every `DXVK_CONFIG` entry it read (`  dxgi.maxFrameRate = 160`); swapchain lines give the colour space |
//! | dxvk-nvapi | `nvapi64.log` / `nvapi.log` (`DXVK_NVAPI_LOG_PATH`, appended) | `<-NvAPI_DRS_GetSetting (0x10e41df3/Override DLSS-SR presets = 0xb): OK`: the application (NGX, Streamline) asked for the setting and got Quiver's value; `Set ShowDlssIndicator in registry to 0x400` |
//! | NVIDIA present layer | `nvpresent.log` (`NVPRESENT_LOG_FILE`) | `Smooth Motion enabled [1]` (the level that prints it is unverified) |
//! | the prefix | `drive_c/ProgramData/NVIDIA/NGX/models` | NVIDIA's updater fetched something |
//!
//! An explicit DXVK frame cap logs no "Engaging frame rate limiter" line (that's only DXVK's
//! own refresh-rate heuristic, spike B), so DXVK entries are confirmed by the effective
//! configuration: DXVK read the setting, which is what the effect claims.

use std::path::Path;

use chrono::Utc;
use quiver_api::tweaks::{Effect, Planned, TweakEffect, TweaksPlan};

/// A dxvk-nvapi driver setting's id and NVIDIA's display name (`NvApiDriverSettings.h`), as
/// its log lines print them.
pub fn drs_name(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key.to_ascii_uppercase().as_str() {
        "NGX_DLSS_SR_OVERRIDE" => ("0x10e41e01", "Enable DLSS-SR override"),
        "NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION" => ("0x10e41df3", "Override DLSS-SR presets"),
        "NGX_DLSS_SR_MODE" => ("0x10afb768", "Override DLSS-SR performance mode"),
        "NGX_DLSS_SR_OVERRIDE_SCALING_RATIO" => ("0x10e41df5", "Override scaling ratio for DLSS-SR"),
        "NGX_DLAA_OVERRIDE" => ("0x10e41df4", "Override DLSS mode to be DLAA"),
        "NGX_DLSS_RR_OVERRIDE" => ("0x10e41e02", "Enable DLSS-RR override"),
        "NGX_DLSS_RR_OVERRIDE_RENDER_PRESET_SELECTION" => ("0x10e41df7", "Override DLSS-RR preset"),
        "NGX_DLSS_RR_MODE" => ("0x10bd9423", "Override DLSS-RR performance mode"),
        "NGX_DLSS_RR_OVERRIDE_SCALING_RATIO" => ("0x10c7d4a2", "Override scaling ratio for DLSS-RR"),
        "NGX_DLSS_FG_OVERRIDE" => ("0x10e41e03", "Enable DLSS-FG override"),
        "NGX_DLSS_FG_OVERRIDE_RENDER_PRESET_SELECTION" => ("0x10e41df1", "Override DLSS-FG preset"),
        "NGX_DLSSG_MULTI_FRAME_COUNT" => ("0x104d6667", "Override DLSSG multi-frame count"),
        "NGX_DLSSG_MODE" => ("0x10308298", "Override DLSSG mode"),
        "NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE" => ("0x10cf4125", "Override DLSSG Target Frame Rate"),
        "NGX_DLSSG_DYNAMIC_MULTI_FRAME_COUNT_MAX" => ("0x10562d0f", "Override maximum DLSSG dynamic multi frame count"),
        "SL_DLSS_OVERRIDE" => ("0x10e41e06", "Enable Streamline override"),
        "NGX_DLSS_NR_OVERRIDE" => ("0x10e41e04", "Enable DLSS NR override"),
        _ => return None,
    })
}

/// The logs of a session's log dir.
#[derive(Debug, Clone, Default)]
pub struct Logs {
    /// (file name, text) of DXVK's logs.
    pub dxvk: Vec<(String, String)>,
    pub nvapi: Option<(String, String)>,
    pub nvpresent: Option<(String, String)>,
}

impl Logs {
    pub fn read(dir: &Path) -> Logs {
        let mut logs = Logs::default();
        let mut names: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        names.sort();
        for p in names {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".log") {
                continue;
            }
            // Logs can grow; the lines that matter come early.
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4 << 20)]).into_owned();
            if ["_dxgi.log", "_d3d11.log", "_d3d9.log", "_d3d8.log", "_d3d10core.log"].iter().any(|s| lower.ends_with(s)) {
                logs.dxvk.push((name, text));
            } else if lower == "nvapi64.log" || lower == "nvapi.log" {
                match &mut logs.nvapi {
                    Some((_, t)) => t.push_str(&text),
                    None => logs.nvapi = Some((name, text)),
                }
            } else if lower == "nvpresent.log" {
                logs.nvpresent = Some((name, text));
            }
        }
        logs
    }

    fn by_name(&self, log: &str) -> Vec<&(String, String)> {
        match log {
            "dxvk" => self.dxvk.iter().collect(),
            "nvapi" => self.nvapi.iter().collect(),
            "nvpresent" => self.nvpresent.iter().collect(),
            _ => Vec::new(),
        }
    }
}

/// `DXVK_CONFIG: k = v` writes of a plan entry.
fn dxvk_writes(writes: &[String]) -> Vec<(String, String)> {
    writes.iter().filter_map(|w| w.strip_prefix("DXVK_CONFIG: ")).filter_map(|e| e.split_once(" = ").map(|(k, v)| (k.to_owned(), v.to_owned()))).collect()
}

/// `DXVK_NVAPI_DRS_<KEY>` writes of a plan entry (the keys).
fn drs_writes(writes: &[String]) -> Vec<String> {
    writes.iter().filter_map(|w| w.strip_prefix("DXVK_NVAPI_DRS_")).filter_map(|e| e.split_once('=').map(|(k, _)| k.to_owned())).collect()
}

/// Evidence for one planned entry in the logs, if any: what to say.
pub fn find(plan: &TweaksPlan, id: &str, logs: &Logs) -> Option<String> {
    let e = plan.entry(id)?;
    let Planned::Apply { mech, writes, .. } = &e.planned else { return None };
    for c in &e.confirm {
        for (file, text) in logs.by_name(&c.log) {
            if text.lines().any(|l| l.contains(&c.line) && c.key.as_ref().is_none_or(|k| l.contains(k.as_str()))) {
                return Some(format!("{} ({file})", c.says.clone().unwrap_or_else(|| format!("the log says \"{}\"", c.line))));
            }
        }
    }
    match mech.as_str() {
        "dxvk" => {
            let wanted = dxvk_writes(writes);
            for (file, text) in &logs.dxvk {
                let Some(start) = text.find("Effective configuration:") else { continue };
                let section = &text[start..];
                for (k, v) in &wanted {
                    let needle = format!("{k} = {v}").to_ascii_lowercase();
                    if section.lines().skip(1).take_while(|l| l.contains("info:    ") || l.trim().is_empty()).any(|l| l.to_ascii_lowercase().trim_end().ends_with(&needle)) {
                        return Some(format!("DXVK's effective configuration has {k} = {v} ({file})"));
                    }
                }
            }
            None
        }
        "nvapi-drs" => {
            let (file, text) = logs.nvapi.as_ref()?;
            for key in drs_writes(writes) {
                let Some((hex, name)) = drs_name(&key) else { continue };
                let head = format!("<-NvAPI_DRS_GetSetting ({hex}/{name} = ");
                if let Some(line) = text.lines().find(|l| l.contains(&head) && l.trim_end().ends_with("): OK")) {
                    let value = line.split(&head).nth(1).and_then(|r| r.split(')').next()).unwrap_or("");
                    return Some(format!("the game's DLSS asked for {name} and got the override ({value}; {file})"));
                }
            }
            None
        }
        "ngx" if writes.iter().any(|w| w.starts_with("DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS")) => {
            let (file, text) = logs.nvapi.as_ref()?;
            text.lines().any(|l| l.contains("Set ShowDlssIndicator in registry to 0x400")).then(|| format!("dxvk-nvapi switched the indicator on in the prefix's registry ({file})"))
        }
        _ => None,
    }
}

/// Look for evidence for every effect still `applied`; returns the ids newly confirmed.
pub fn scan(plan: &TweaksPlan, effects: &mut [TweakEffect], logs: &Logs) -> Vec<String> {
    let mut out = Vec::new();
    for e in effects.iter_mut().filter(|e| e.effect == Effect::Applied) {
        if let Some(evidence) = find(plan, &e.id, logs) {
            e.effect = Effect::Confirmed { evidence, at: Utc::now() };
            out.push(e.id.clone());
        }
    }
    out
}

/// At the end of a session: what's still `applied` becomes `unconfirmed`, with why.
/// `prefix`: the game's prefix (for the NGX updater's models folder).
pub fn finish(plan: &TweaksPlan, effects: &mut [TweakEffect], logs: &Logs, prefix: Option<&Path>) {
    scan(plan, effects, logs);
    for e in effects.iter_mut().filter(|e| e.effect == Effect::Applied) {
        let Some(Planned::Apply { mech, writes, .. }) = plan.entry(&e.id).map(|p| &p.planned) else {
            e.effect = Effect::Unconfirmed { why: "no record of what was applied".into() };
            continue;
        };
        let why = match mech.as_str() {
            "dxvk" if logs.dxvk.is_empty() => "no DXVK log this session: DXVK didn't load (an OpenGL or native Vulkan game?)".to_owned(),
            "dxvk" => "DXVK's logs don't list the setting".to_owned(),
            "nvapi-drs" if logs.nvapi.is_none() => "dxvk-nvapi didn't start (no nvapi64.log): the game didn't load NVAPI".to_owned(),
            "nvapi-drs" => "the game didn't ask for this setting (it created no DLSS feature this session?)".to_owned(),
            "ngx" if writes.iter().any(|w| w.starts_with("PROTON_ENABLE_NGX_UPDATER")) => {
                let models = prefix.map(|p| p.join("drive_c/ProgramData/NVIDIA/NGX/models"));
                match models.filter(|m| std::fs::read_dir(m).is_ok_and(|mut d| d.next().is_some())) {
                    Some(m) => {
                        e.effect = Effect::Confirmed { evidence: format!("NVIDIA's updater has models in the prefix ({})", m.display()), at: Utc::now() };
                        continue;
                    }
                    None => "nothing in the prefix's NGX models folder yet (the download can take a launch)".to_owned(),
                }
            }
            "vkd3d" => "vkd3d-proton reports this only on the game's stderr".to_owned(),
            _ => "nothing reports this setting".to_owned(),
        };
        e.effect = Effect::Unconfirmed { why };
    }
}
