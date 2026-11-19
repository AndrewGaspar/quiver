//! What can be learned about a game from its files, for a modding brief and for the lab's
//! own decisions (is a Steam game DRM-free?): engine hints, graphics API and SDKs from the
//! executable's imports and the DLLs beside it, anti-cheat markers, and where the game
//! keeps its config files and saves in its prefix. Read-only and bounded.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A hint and why (`Unreal Engine 4/5` because of `Engine/Binaries/`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hint {
    pub name: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pe {
    /// `x64`, `x86`, or the machine number.
    pub arch: String,
    /// Imported DLLs, lowercase.
    pub imports: Vec<String>,
    pub sections: Vec<String>,
}

/// Parse a PE file's machine, section names and import table (enough for hints).
pub fn parse_pe(b: &[u8]) -> Option<Pe> {
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    if b.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let machine = u16_at(pe + 4)?;
    let nsec = u16_at(pe + 6)? as usize;
    let opt_size = u16_at(pe + 20)? as usize;
    let opt = pe + 24;
    let magic = u16_at(opt)?;
    // Data directory 1 (imports): after the fixed optional header fields.
    let dd = match magic {
        0x20b => opt + 112,
        0x10b => opt + 96,
        _ => return None,
    };
    let import_rva = u32_at(dd + 8)? as usize;
    let sec_table = opt + opt_size;
    let mut sections = Vec::new();
    let mut map = Vec::new(); // (va, vsize, raw, rawsize)
    for i in 0..nsec.min(96) {
        let s = sec_table + i * 40;
        let name = b.get(s..s + 8)?;
        let name = String::from_utf8_lossy(name.split(|&c| c == 0).next().unwrap_or_default()).into_owned();
        let vsize = u32_at(s + 8)? as usize;
        let va = u32_at(s + 12)? as usize;
        let rawsize = u32_at(s + 16)? as usize;
        let raw = u32_at(s + 20)? as usize;
        sections.push(name);
        map.push((va, vsize.max(rawsize), raw, rawsize));
    }
    let off = |rva: usize| map.iter().find(|(va, vs, _, _)| rva >= *va && rva < va + vs).map(|(va, _, raw, _)| rva - va + raw);
    let mut imports = Vec::new();
    if import_rva != 0
        && let Some(mut d) = off(import_rva)
    {
        for _ in 0..512 {
            let name_rva = u32_at(d + 12).unwrap_or(0) as usize;
            if name_rva == 0 {
                break;
            }
            if let Some(o) = off(name_rva) {
                let end = b.get(o..).and_then(|s| s.iter().take(256).position(|&c| c == 0)).unwrap_or(0);
                imports.push(String::from_utf8_lossy(&b[o..o + end]).to_ascii_lowercase());
            }
            d += 20;
        }
    }
    let arch = match machine {
        0x8664 => "x64".into(),
        0x14c => "x86".into(),
        0xaa64 => "arm64".into(),
        m => format!("0x{m:x}"),
    };
    Some(Pe { arch, imports, sections })
}

/// Graphics APIs an executable imports directly (a game may load others at run time).
pub fn graphics_apis(pe: &Pe) -> Vec<String> {
    let mut out = Vec::new();
    for (dll, api) in [
        ("d3d12.dll", "Direct3D 12"),
        ("d3d11.dll", "Direct3D 11"),
        ("d3d10.dll", "Direct3D 10"),
        ("d3d10_1.dll", "Direct3D 10"),
        ("d3d9.dll", "Direct3D 9"),
        ("d3d8.dll", "Direct3D 8"),
        ("vulkan-1.dll", "Vulkan"),
        ("opengl32.dll", "OpenGL"),
        ("dxgi.dll", "DXGI"),
    ] {
        if pe.imports.iter().any(|i| i == dll) && !out.iter().any(|o| o == api) {
            out.push(api.to_owned());
        }
    }
    out
}

/// Entries of a folder tree, shallow first, at most `max` entries and `depth` levels.
pub fn walk(root: &Path, depth: usize, max: usize) -> Vec<(PathBuf, bool)> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([(PathBuf::new(), 0usize)]);
    while let Some((rel, d)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if out.len() >= max {
                return out;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = rel.join(e.file_name());
            if ft.is_dir() && d + 1 < depth {
                queue.push_back((p.clone(), d + 1));
            }
            out.push((p, ft.is_dir()));
        }
    }
    out
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Engine hints from an install's files.
pub fn engine_hints(files: &[(PathBuf, bool)], exe: &str) -> Vec<Hint> {
    let mut out = BTreeSet::new();
    let has = |f: &dyn Fn(&Path, bool) -> bool| files.iter().find(|(p, d)| f(p, *d)).map(|(p, _)| p.display().to_string());
    let ext = |p: &Path, e: &str| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(e));
    let mut add = |name: &str, ev: Option<String>| {
        if let Some(ev) = ev {
            out.insert(Hint { name: name.into(), evidence: ev });
        }
    };
    let exe_l = exe.to_ascii_lowercase();
    add("Unreal Engine 4/5", has(&|p, d| d && p.ends_with("Engine/Binaries")).or_else(|| (exe_l.contains("-win64-shipping") || exe_l.contains("-wingdk-shipping")).then(|| exe.to_owned())));
    add("Unreal Engine 4/5 (paks)", has(&|p, d| !d && ext(p, "pak") && p.to_string_lossy().contains("Content/Paks")));
    add("Unreal Engine 3", has(&|p, d| !d && (ext(p, "upk") || ext(p, "xxx"))).or_else(|| has(&|p, d| d && name_of(p) == "cookedpc")).or_else(|| exe_l.contains("-win32-shipping").then(|| exe.to_owned())));
    add("Unity", has(&|p, d| !d && name_of(p) == "unityplayer.dll"));
    add("Unity (IL2CPP)", has(&|p, d| !d && name_of(p) == "gameassembly.dll"));
    add("Unity (Mono)", has(&|p, d| d && name_of(p) == "monobleedingedge"));
    add("Anvil (Ubisoft)", has(&|p, d| !d && ext(p, "forge")));
    add("Dunia (Ubisoft)", has(&|p, d| !d && ext(p, "fat")));
    add("Snowdrop (Ubisoft/Massive)", has(&|p, d| !d && ext(p, "sdf")));
    add("Northlight (Remedy)", has(&|p, d| !d && ext(p, "rmdp")));
    add("Frostbite", has(&|p, d| !d && ext(p, "cas")));
    add("Source", has(&|p, d| !d && ext(p, "vpk")));
    add("RE Engine", has(&|p, d| !d && name_of(p).starts_with("re_chunk_000")));
    add("REDengine", has(&|p, d| !d && ext(p, "archive")));
    add("id Tech", has(&|p, d| !d && ext(p, "resources")));
    add("CryEngine", has(&|p, d| !d && name_of(p) == "crysystem.dll"));
    let stem = Path::new(exe).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    add("Godot", has(&|p, d| !d && ext(p, "pck") && p.file_stem().is_some_and(|s| s.to_string_lossy().to_ascii_lowercase() == stem)));
    add("Creation Engine (Bethesda)", has(&|p, d| !d && (ext(p, "ba2") || ext(p, "bsa"))));
    add("GameMaker", has(&|p, d| !d && name_of(p) == "data.win"));
    out.into_iter().collect()
}

/// SDKs and middleware by the DLLs an install ships.
pub fn sdk_hints(files: &[(PathBuf, bool)]) -> Vec<Hint> {
    const DLLS: &[(&str, &str)] = &[
        ("steam_api64.dll", "Steamworks"),
        ("steam_api.dll", "Steamworks"),
        ("eossdk-win64-shipping.dll", "Epic Online Services"),
        ("eossdk-win32-shipping.dll", "Epic Online Services"),
        ("galaxy64.dll", "GOG Galaxy SDK"),
        ("galaxy.dll", "GOG Galaxy SDK"),
        ("uplay_r1_loader64.dll", "Ubisoft SDK (R1)"),
        ("uplay_r1_loader.dll", "Ubisoft SDK (R1)"),
        ("upc_r2_loader64.dll", "Ubisoft SDK (R2)"),
        ("nvngx_dlss.dll", "NVIDIA DLSS"),
        ("nvngx_dlssg.dll", "NVIDIA DLSS Frame Generation"),
        ("sl.interposer.dll", "NVIDIA Streamline"),
        ("libxess.dll", "Intel XeSS"),
        ("amd_fidelityfx_dx12.dll", "AMD FidelityFX (FSR)"),
        ("ffx_fsr2_api_x64.dll", "AMD FSR 2"),
        ("bink2w64.dll", "Bink video"),
        ("fmod.dll", "FMOD"),
        ("fmodstudio.dll", "FMOD Studio"),
        ("aksoundengine.dll", "Wwise"),
        ("physx3_x64.dll", "PhysX"),
        ("d3dcompiler_47.dll", "D3DCompiler (shipped)"),
    ];
    let mut out = BTreeSet::new();
    for (p, d) in files {
        if *d {
            continue;
        }
        let n = name_of(p);
        if let Some((_, what)) = DLLS.iter().find(|(dll, _)| *dll == n) {
            out.insert(Hint { name: (*what).into(), evidence: p.display().to_string() });
        }
    }
    out.into_iter().collect()
}

/// Steam's DRM in an install: Steamworks (a running Steam client is needed) and SteamStub
/// (the exe's `.bind` section: it won't start without Steam).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SteamDrm {
    pub steamworks: Option<String>,
    pub steamstub: bool,
}

impl SteamDrm {
    pub fn drm_free(&self) -> bool {
        self.steamworks.is_none() && !self.steamstub
    }
}

pub fn steam_drm(files: &[(PathBuf, bool)], pe: Option<&Pe>) -> SteamDrm {
    let steamworks = files.iter().find(|(p, d)| !d && matches!(name_of(p).as_str(), "steam_api.dll" | "steam_api64.dll")).map(|(p, _)| p.display().to_string());
    let steamworks = steamworks.or_else(|| pe.and_then(|pe| pe.imports.iter().find(|i| i.starts_with("steam_api")).cloned()));
    SteamDrm { steamworks, steamstub: pe.is_some_and(|pe| pe.sections.iter().any(|s| s == ".bind")) }
}

/// Folders in a prefix where games keep user files (relative to the prefix).
const USER_DIRS: &[&str] = &["Documents", "My Documents", "Saved Games", "AppData/Local", "AppData/LocalLow", "AppData/Roaming"];
/// Wine's and Windows' own folders there, skipped.
const SKIP: &[&str] = &["microsoft", "temp", "openwine", "wine", "nvidia", "nvidia corporation", "d3dscache", "cache", "crashdumps", "ubisoft game launcher", "epicgameslauncher", "steam", "vkd3d-proton", "dxvk"];

/// Config-looking and save-looking files in the prefix's user folders, newest first
/// (paths relative to the prefix).
pub fn user_files(prefix: &Path, limit: usize) -> (Vec<String>, Vec<String>) {
    let mut configs = Vec::new();
    let mut saves = Vec::new();
    let users = std::fs::read_dir(prefix.join("drive_c/users")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && name_of(p) != "public");
    for user in users {
        for dir in USER_DIRS {
            let base = user.join(dir);
            for (rel, is_dir) in walk(&base, 5, 4000) {
                if is_dir || rel.components().next().is_some_and(|c| SKIP.contains(&c.as_os_str().to_string_lossy().to_ascii_lowercase().as_str())) {
                    continue;
                }
                let full = base.join(&rel);
                let n = name_of(&rel);
                let path_l = rel.to_string_lossy().to_ascii_lowercase();
                let mtime = full.metadata().and_then(|m| m.modified()).ok();
                let shown = full.strip_prefix(prefix).unwrap_or(&full).display().to_string();
                if [".ini", ".cfg", ".xml", ".json", ".toml", ".lua", ".yaml", ".conf", ".txt"].iter().any(|e| n.ends_with(e)) && !path_l.contains("savegame") {
                    configs.push((mtime, shown));
                } else if [".sav", ".save", ".savegame", ".sl2", ".dat"].iter().any(|e| n.ends_with(e)) || path_l.contains("save") {
                    saves.push((mtime, shown));
                }
            }
        }
    }
    // Ubisoft SDK saves live in the Connect folder: `savegames/<user>/<product>/`.
    let sdk = prefix.join(crate::launcher::UPC_REL).join("savegames");
    for (rel, is_dir) in walk(&sdk, 3, 2000) {
        if !is_dir {
            let full = sdk.join(&rel);
            let mtime = full.metadata().and_then(|m| m.modified()).ok();
            saves.push((mtime, full.strip_prefix(prefix).unwrap_or(&full).display().to_string()));
        }
    }
    let finish = |mut v: Vec<(Option<std::time::SystemTime>, String)>| {
        v.sort_by_key(|e| std::cmp::Reverse(e.0));
        v.into_iter().take(limit).map(|(_, s)| s).collect::<Vec<_>>()
    };
    (finish(configs), finish(saves))
}

/// Everything a brief says about a game's files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    pub exe: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pe: Option<Pe>,
    pub graphics_apis: Vec<String>,
    pub engines: Vec<Hint>,
    pub sdks: Vec<Hint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anticheat: Option<String>,
    pub steam: SteamDrm,
    /// Config files beside the executable or in the install's top levels.
    pub install_configs: Vec<String>,
    pub prefix_configs: Vec<String>,
    pub prefix_saves: Vec<String>,
}

/// Look at an install (and its prefix, when given).
pub fn gather(install: &Path, exe: &str, prefix: Option<&Path>) -> Facts {
    let files = walk(install, 5, 30_000);
    let exe_bytes = std::fs::read(install.join(exe)).ok();
    let pe = exe_bytes.as_deref().and_then(parse_pe);
    let install_configs = files
        .iter()
        .filter(|(p, d)| !d && p.components().count() <= 3 && !p.starts_with(".quiver-install") && [".ini", ".cfg", ".toml", ".json", ".xml"].iter().any(|e| name_of(p).ends_with(e)))
        .take(40)
        .map(|(p, _)| p.display().to_string())
        .collect();
    let (prefix_configs, prefix_saves) = prefix.map(|p| user_files(p, 40)).unwrap_or_default();
    Facts {
        exe: exe.to_owned(),
        graphics_apis: pe.as_ref().map(graphics_apis).unwrap_or_default(),
        engines: engine_hints(&files, exe),
        sdks: sdk_hints(&files),
        anticheat: crate::anticheat::detect(install).map(|d| d.describe()),
        steam: steam_drm(&files, pe.as_ref()),
        pe,
        install_configs,
        prefix_configs,
        prefix_saves,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal PE32+ with one section holding an import table naming `dlls`.
    pub(crate) fn fake_pe(dlls: &[&str], extra_section: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 0x400];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80;
        b[pe..pe + 4].copy_from_slice(b"PE\0\0");
        b[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes());
        let nsec: u16 = if extra_section.is_some() { 2 } else { 1 };
        b[pe + 6..pe + 8].copy_from_slice(&nsec.to_le_bytes());
        let opt_size: u16 = 240;
        b[pe + 20..pe + 22].copy_from_slice(&opt_size.to_le_bytes());
        let opt = pe + 24;
        b[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        // Import directory RVA 0x1000 (data directory 1).
        b[opt + 112 + 8..opt + 112 + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        let sec = opt + opt_size as usize;
        b[sec..sec + 6].copy_from_slice(b".idata");
        b[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes()); // vsize
        b[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // va
        b[sec + 16..sec + 20].copy_from_slice(&0x400u32.to_le_bytes()); // raw size
        b[sec + 20..sec + 24].copy_from_slice(&0x400u32.to_le_bytes()); // raw ptr
        if let Some(name) = extra_section {
            let s2 = sec + 40;
            b[s2..s2 + name.len()].copy_from_slice(name.as_bytes());
        }
        let mut raw = vec![0u8; 0x400];
        // Descriptors at 0, names after them.
        let mut name_off = 20 * (dlls.len() + 1);
        for (i, d) in dlls.iter().enumerate() {
            let rva = 0x1000 + name_off as u32;
            raw[i * 20 + 12..i * 20 + 16].copy_from_slice(&rva.to_le_bytes());
            raw[name_off..name_off + d.len()].copy_from_slice(d.as_bytes());
            name_off += d.len() + 1;
        }
        b.extend(raw);
        b
    }

    #[test]
    fn pe_imports() {
        let pe = parse_pe(&fake_pe(&["KERNEL32.dll", "d3d11.dll", "dxgi.dll", "steam_api64.dll"], Some(".bind"))).unwrap();
        assert_eq!(pe.arch, "x64");
        assert_eq!(pe.imports, ["kernel32.dll", "d3d11.dll", "dxgi.dll", "steam_api64.dll"]);
        assert_eq!(graphics_apis(&pe), ["Direct3D 11", "DXGI"]);
        let drm = steam_drm(&[], Some(&pe));
        assert!(drm.steamstub && drm.steamworks.is_some() && !drm.drm_free());
        assert!(parse_pe(b"not a pe").is_none());
        assert!(steam_drm(&[], Some(&parse_pe(&fake_pe(&["d3d12.dll"], None)).unwrap())).drm_free());
    }

    #[test]
    fn hints_from_files() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        for f in ["Engine/Binaries/Win64/x.dll", "Game/Content/Paks/Game-WindowsNoEditor.pak", "Game/Binaries/Win64/nvngx_dlss.dll", "EOSSDK-Win64-Shipping.dll", "Game.ini"] {
            let p = r.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
        }
        let files = walk(r, 5, 1000);
        let e: Vec<String> = engine_hints(&files, "Game/Binaries/Win64/Game-Win64-Shipping.exe").into_iter().map(|h| h.name).collect();
        assert!(e.contains(&"Unreal Engine 4/5".to_owned()) && e.contains(&"Unreal Engine 4/5 (paks)".to_owned()), "{e:?}");
        let s: Vec<String> = sdk_hints(&files).into_iter().map(|h| h.name).collect();
        assert_eq!(s, ["Epic Online Services", "NVIDIA DLSS"]);
        let f = gather(r, "missing.exe", None);
        assert!(f.pe.is_none() && f.install_configs.contains(&"Game.ini".to_owned()) && f.steam.drm_free());
    }

    #[test]
    fn user_files_in_a_prefix() {
        let t = tempfile::tempdir().unwrap();
        let docs = t.path().join("drive_c/users/steamuser/Documents");
        for f in ["Assassin's Creed Origins/ACO.ini", "My Games/Game/Saves/slot1.sav", "Microsoft/x.ini"] {
            let p = docs.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
        }
        let (c, s) = user_files(t.path(), 10);
        assert_eq!(c, ["drive_c/users/steamuser/Documents/Assassin's Creed Origins/ACO.ini"]);
        assert_eq!(s, ["drive_c/users/steamuser/Documents/My Games/Game/Saves/slot1.sav"]);
    }
}
