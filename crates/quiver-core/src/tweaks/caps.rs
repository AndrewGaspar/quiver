//! The capability probe (design/TWEAKS.md §3.4): GPUs and the driver from `/proc` and
//! `/sys`, what a Proton build contains (component versions from its `version` files, the
//! option and variable names its DLLs and script contain), and the driver's NGX pieces.
//! Everything reads under a `root` so tests run on fixture trees. Proton probes are cached
//! per build (by its `version` file).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quiver_api::options::Schema;
use quiver_api::tweaks::{Capabilities, Gpu, NgxCaps, ProtonCaps};
use serde::{Deserialize, Serialize};

/// `620.70.06` from the NVIDIA kernel module's version line.
fn nvidia_driver(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("proc/driver/nvidia/version")).ok()?;
    let line = text.lines().find(|l| l.contains("NVRM version"))?;
    line.split_whitespace().find(|w| w.split('.').count() >= 2 && w.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))).map(str::to_owned)
}

/// RTX 20/30/40/50 from a model name ("NVIDIA GeForce RTX 5070 Laptop GPU").
pub fn rtx_series(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    let i = up.find("RTX")?;
    let digits: String = up[i + 3..].trim_start().chars().take_while(char::is_ascii_digit).collect();
    (digits.len() == 4).then(|| digits[..2].parse::<u32>().ok()).flatten().filter(|s| (20..=90).contains(s))
}

fn arch(series: u32) -> Option<&'static str> {
    Some(match series {
        20 => "turing",
        30 => "ampere",
        40 => "ada",
        50 => "blackwell",
        _ => return None,
    })
}

fn vendor_name(id: &str) -> String {
    match id.trim().to_ascii_lowercase().as_str() {
        "0x10de" => "nvidia".into(),
        "0x1002" => "amd".into(),
        "0x8086" => "intel".into(),
        other => other.to_owned(),
    }
}

/// GPUs: NVIDIA's from `/proc/driver/nvidia/gpus/*/information`, everything else from DRM
/// cards' PCI vendor ids. The GPU games run on: NVIDIA's if there is one (the dGPU on hybrid
/// laptops), else AMD's, else Intel's.
pub fn gpus(root: &Path) -> (Vec<Gpu>, Option<Gpu>) {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root.join("proc/driver/nvidia/gpus")).into_iter().flatten().flatten() {
        let Ok(info) = std::fs::read_to_string(e.path().join("information")) else { continue };
        let name = info.lines().find_map(|l| l.strip_prefix("Model:")).map(|s| s.trim().to_owned()).unwrap_or_default();
        let series = rtx_series(&name);
        out.push(Gpu { vendor: "nvidia".into(), arch: series.and_then(arch).map(str::to_owned), rtx_series: series, name });
    }
    let mut cards: Vec<PathBuf> = std::fs::read_dir(root.join("sys/class/drm")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    cards.sort();
    for c in cards {
        let n = c.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if !n.starts_with("card") || n.contains('-') {
            continue;
        }
        let Ok(v) = std::fs::read_to_string(c.join("device/vendor")) else { continue };
        let vendor = vendor_name(&v);
        if vendor == "nvidia" && out.iter().any(|g| g.vendor == "nvidia") {
            continue;
        }
        out.push(Gpu { vendor, ..Default::default() });
    }
    let pick = ["nvidia", "amd", "intel"].iter().find_map(|v| out.iter().find(|g| g.vendor == *v)).or(out.first()).cloned();
    (out, pick)
}

/// `v0.9.3` from a component's `version` file (`<sha> dxvk-nvapi (v0.9.3)`).
pub fn component_version(text: &str) -> Option<String> {
    let inner = text.trim().rsplit_once('(').map(|(_, r)| r.trim_end_matches(')').trim().to_owned()).unwrap_or_else(|| text.trim().to_owned());
    let v = inner.strip_prefix("vkd3d-").unwrap_or(&inner).to_owned();
    (!v.is_empty()).then_some(v)
}

/// The numbers of a version (`v3.1.1-174-g…` → 3.1.1; `620.70.06` → 620.70.6).
pub fn version_parts(v: &str) -> Vec<u64> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let main = v.split(['-', ' ', '+']).next().unwrap_or("");
    main.split('.').map_while(|p| p.parse().ok()).collect()
}

/// `>=0.9.3`, `>575`, `<=1.0`, `=2.6` (no operator: `>=`) against a version.
pub fn version_holds(req: &str, have: &str) -> bool {
    let req = req.trim();
    let (op, want) = ["<=", ">=", "==", "<", ">", "="].iter().find_map(|op| req.strip_prefix(op).map(|r| (*op, r))).unwrap_or((">=", req));
    let (a, b) = (version_parts(have), version_parts(want));
    let n = a.len().max(b.len());
    let pad = |mut v: Vec<u64>| {
        v.resize(n, 0);
        v
    };
    let ord = pad(a).cmp(&pad(b));
    match op {
        "<=" => ord.is_le(),
        ">=" => ord.is_ge(),
        "<" => ord.is_lt(),
        ">" => ord.is_gt(),
        _ => ord.is_eq(),
    }
}

/// The names a schema's bindings test with `has` / `lacks` (what the probe looks for).
pub fn needles(schema: &Schema) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn walk(r: &quiver_api::options::Requires, out: &mut Vec<String>) {
        for n in r.has.iter().chain(&r.lacks) {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
        for a in &r.any {
            walk(a, out);
        }
    }
    for d in schema.options.values() {
        for b in &d.via {
            walk(&b.requires, &mut out);
        }
    }
    // Always probed: the names `tweaks explain` reports and the evidence relies on.
    for n in ["dxvk:dxgi.maxFrameRate", "dxvk:dxvk.latencySleep", "dxvk:dxvk.enableNvLowLatency2", "dxvk:dxvk.disableNvLowLatency2", "nvapi:NGX_DLSSG_MODE", "proton:PROTON_ENABLE_HDR", "proton:PROTON_DLSS_UPGRADE"] {
        if !out.iter().any(|x| x == n) {
            out.push(n.into());
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Which files hold each component's names.
fn component_files(build: &Path, component: &str) -> Vec<PathBuf> {
    let wine = build.join("files/lib/wine");
    match component {
        "dxvk" => ["dxgi.dll", "d3d11.dll", "d3d9.dll"].iter().map(|f| wine.join("dxvk/x86_64-windows").join(f)).collect(),
        "nvapi" => vec![wine.join("nvapi/x86_64-windows/nvapi64.dll")],
        "vkd3d" => vec![wine.join("vkd3d-proton/x86_64-windows/d3d12core.dll"), wine.join("vkd3d-proton/x86_64-windows/d3d12.dll")],
        "proton" => {
            let mut v = vec![build.join("proton")];
            // GE keeps most switches in protonfixes.
            for f in ["protonfixes/util.py", "protonfixes/fix.py", "protonfixes/upscalers.py"] {
                v.push(build.join(f));
            }
            v
        }
        _ => Vec::new(),
    }
}

/// Probe a Proton build: component versions and the names of `wanted` (`dxvk:…`, `nvapi:…`,
/// `vkd3d:…`, `proton:…`) found in its files.
pub fn probe_proton(build: &Path, wanted: &[String]) -> ProtonCaps {
    let name = build.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let version = std::fs::read_to_string(build.join("version")).unwrap_or_default().trim().to_owned();
    let wine = build.join("files/lib/wine");
    let comp = |d: &str| std::fs::read_to_string(wine.join(d).join("version")).ok().and_then(|t| component_version(&t));
    let flavour = if name.starts_with("GE-Proton") || version.contains("GE-Proton") {
        "ge"
    } else if build.components().any(|c| c.as_os_str() == "common") && name.starts_with("Proton") {
        "valve"
    } else {
        "other"
    };
    let mut by_comp: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for w in wanted {
        if let Some((c, _)) = w.split_once(':') {
            by_comp.entry(match c {
                "dxvk" => "dxvk",
                "nvapi" => "nvapi",
                "vkd3d" => "vkd3d",
                _ => "proton",
            }).or_default().push(w);
        }
    }
    let mut has = Vec::new();
    for (c, names) in by_comp {
        let bytes: Vec<Vec<u8>> = component_files(build, c).iter().filter_map(|p| std::fs::read(p).ok()).collect();
        for w in names {
            let n = w.split_once(':').map_or(w.as_str(), |x| x.1);
            if bytes.iter().any(|b| contains(b, n.as_bytes())) {
                has.push(w.clone());
            }
        }
    }
    let script = std::fs::read(build.join("proton")).unwrap_or_default();
    ProtonCaps {
        name,
        path: Some(build.to_owned()),
        version,
        flavour: flavour.into(),
        dxvk: comp("dxvk"),
        vkd3d: comp("vkd3d-proton"),
        nvapi: comp("nvapi"),
        has,
        copies_updater: contains(&script, b"nvngx_update.exe"),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    builds: BTreeMap<String, (String, ProtonCaps)>,
}

/// [`probe_proton`], cached in `<data>/tweaks/probe-cache.json` by the build's path, its
/// `version` file and the names asked for.
pub fn probe_proton_cached(data: &Path, build: &Path, wanted: &[String]) -> ProtonCaps {
    let path = data.join("tweaks").join("probe-cache.json");
    let mut cache: Cache = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let stamp = format!("{}|{}", std::fs::read_to_string(build.join("version")).unwrap_or_default().trim(), wanted.join(","));
    let key = build.display().to_string();
    if let Some((s, caps)) = cache.builds.get(&key)
        && *s == stamp
    {
        return caps.clone();
    }
    let caps = probe_proton(build, wanted);
    cache.builds.insert(key, (stamp, caps.clone()));
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
        && let Ok(b) = serde_json::to_vec_pretty(&cache)
    {
        let _ = std::fs::write(&path, b);
    }
    caps
}

/// Where the driver keeps the NGX DLLs Proton copies into prefixes.
pub fn ngx(root: &Path) -> NgxCaps {
    for d in ["usr/lib/nvidia/wine", "usr/lib64/nvidia/wine", "usr/lib/x86_64-linux-gnu/nvidia/wine"] {
        let dir = root.join(d);
        if dir.join("nvngx.dll").is_file() || dir.join("_nvngx.dll").is_file() {
            return NgxCaps { core: dir.join("_nvngx.dll").is_file() && dir.join("nvngx.dll").is_file(), updater: dir.join("nvngx_update.exe").is_file(), dir: Some(dir) };
        }
    }
    NgxCaps::default()
}

/// The machine's part of the capability set (GPU, driver, NGX), read under `root` (`/`).
pub fn machine(root: &Path) -> Capabilities {
    let (gpus, gpu) = gpus(root);
    let driver = if gpu.as_ref().is_some_and(|g| g.vendor == "nvidia") { nvidia_driver(root) } else { None };
    let mut notes = Vec::new();
    if gpu.is_none() {
        notes.push("no GPU found under /sys/class/drm".into());
    }
    if gpu.as_ref().is_some_and(|g| g.vendor == "nvidia") && driver.is_none() {
        notes.push("the NVIDIA driver's version isn't readable (/proc/driver/nvidia/version)".into());
    }
    Capabilities { gpus, gpu, driver, ngx: ngx(root), api: "unknown".into(), notes, ..Default::default() }
}
