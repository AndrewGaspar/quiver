# Quiver Tweaks: platform-level graphics and latency overrides (design notes)

Status: proposal (2026-11-17), nothing built. Builds on [MOD-CONFIG.md](MOD-CONFIG.md) (the
Options engine, built in phase 1), [SIGHT.md](SIGHT.md) and [SIGHT-HUD.md](SIGHT-HUD.md) (the
Sight Vulkan layer, the session dir, the metrics hub) and [STORES.md](STORES.md) (launch plans
across stores, the anti-cheat data, the legitimacy policy). Steers are in section 11.

**Name.** **Quiver Tweaks** (*tweaks* in the CLI and the API) is the feature: per-game overrides
for upscaling, frame generation, frame rate, sync and latency that Quiver applies to any game it
launches, whatever the store, the way the NVIDIA App's per-program graphics settings apply to
any game on Windows. *Options* stays the engine and the word for a mod's or a game's own
settings; a tweak is an option of Quiver's built-in **platform surface**. *Mechanism* is how a
tweak reaches the game (an environment variable, a DXVK option, a driver setting via
dxvk-nvapi, a Sight layer control, a wrapper's argument). *Provider* is a mod or game that
implements a tweak itself (aco-dlss provides its own frame rate cap and DLSS model).

---

## 0. Summary

- **Tweaks are an Options surface, not a new subsystem.** One built-in, catalog-shipped
  `quiver-options.toml` (`options/quiver-tweaks.toml`, surface `quiver`, kind `platform`) with
  the same types, pages, `when`, presets, `level`, `apply` classes, revisions, provenance,
  undo, outcomes, app form, CLI and in-game panel that mods already get. Two things are new in
  the engine: a **`via` binding** (a list of mechanisms per option, chosen per launch by
  capability) instead of a file key, and **scopes** (global → game → profile, like the HUD)
  for platform surfaces.
- **Mechanisms are built-in Rust backends** (no scripting, as MOD-CONFIG decided): `env`,
  `dxvk` (entries merged into `DXVK_CONFIG`), `vkd3d` (`VKD3D_*`), `nvapi-drs`
  (`DXVK_NVAPI_DRS_<SETTING>`, the same driver-profile settings the NVIDIA App writes on
  Windows), `ngx` (NVIDIA's NGX updater and indicator), `layer` (a control block the Sight
  layer reads per present: live), `vk-layer` (third-party layers with a generated config),
  `wrapper` (gamescope arguments), and `mod` (a provider's own option).
- **A capability probe picks the mechanism** for each tweak at launch: GPU vendor and
  architecture, driver version, what the Proton build contains (DXVK, dxvk-nvapi and
  vkd3d-proton versions, which `PROTON_*` switches its script knows), the route (Quiver's
  `proton run` or Steam via `steam-run`), the game's graphics API as the Sight layer saw it
  last session, the anti-cheat flag, and the mod stack's providers. Anything that can't apply
  says why ("NVIDIA only", "this Proton's dxvk-nvapi is older than 0.9", "DLSS mod controls
  this").
- **Truthful outcomes.** Beyond Options' save outcomes, every tweak gets a per-session
  **effect**: *confirmed* with evidence (a dxvk-nvapi log line saying NGX read the DLSS preset
  override, DXVK's "Engaging frame rate limiter", the layer's status block, a mod metric),
  *unconfirmed* (applied, no evidence: "the game didn't use DLSS this session"),
  *provided by* (a mod), *unavailable* (reason). Quiver never says "DLSS preset K" unless
  something in the game said so.
- **Mods that implement a tweak declare it** (`[provides."fps.cap"] option =
  "reflex.fps_limit"`). Default: the mod's implementation wins and Quiver's mechanism stands
  down, the tweak's value flows into the mod's option as a default layer (so "cap 160 fps
  everywhere" reaches aco-dlss as `reflex.fps_limit = 160`), and the tweak row says
  "Controlled by DLSS (aco-dlss)". `conflicts` marks mechanisms that would double-apply or
  break the mod (DXVK's frame limiter on a mod that presents two frames per game frame, DXVK's
  LOD bias on top of the mod's). Mods keep everything beyond the tweak set (FG pacing, crowd
  density) as their own options.
- **v1 (launch-time, no new in-game code):** frame rate cap, VSync, maximum frame latency,
  low-latency sleep, DLSS Super Resolution / Ray Reconstruction / Frame Generation overrides
  (model, render scale, DLAA, multi-frame count, dynamic target), DLSS updater and indicator,
  NVIDIA Smooth Motion, anisotropic filtering, LOD bias, HDR. **v2 (Sight layer controls, live):** frame rate cap
  changed while playing, background cap, present-mode override, lsfg-vk and vkBasalt with
  user-supplied pieces, gamescope scaling. **v3 (research):** Reflex-aware capping and
  low-latency injection through `VK_NV_low_latency2` in the layer, a sharpening pass.
- **Legitimacy.** Quiver ships no NVIDIA, AMD or Intel binaries and downloads none from
  third-party mirrors. Newer DLSS comes only from NVIDIA's own NGX updater, launched by the
  driver's NGX core through Proton (`PROTON_ENABLE_NGX_UPDATER=1`). Layer-based tweaks follow
  the Sight layer's anti-cheat rule (off for flagged games); configuration-only tweaks (env,
  DXVK options, driver settings) apply everywhere, because they only change how components
  Proton already loads behave.

---

## 1. What the NVIDIA App does, and what Linux has instead

On Windows, the NVIDIA App's **Graphics → Program Settings** (it replaced the Control Panel's
"Manage 3D Settings"; the Control Panel stopped being installed with driver 610.47) writes
**driver profile (DRS) settings** per executable. Games' NGX and Streamline runtimes read the
DLSS ones when they create a DLSS feature; the driver applies the rest. The setting ids are
public in NVAPI's `NvApiDriverSettings.h`. Under Proton, dxvk-nvapi answers the same DRS queries
from `DXVK_NVAPI_DRS_*` variables, so the DLSS half of the NVIDIA App maps one to one onto
Linux. The driver-side half (limiter, low latency mode, power, RTX HDR) mostly doesn't: the
Linux driver has no equivalent, and DXVK, vkd3d-proton, gamescope or a layer stand in.

| NVIDIA App setting (Windows) | Values | GPUs | DRS key | Linux equivalent | Vendors on Linux |
|---|---|---|---|---|---|
| DLSS Override: Super Resolution model | app setting, **Recommended** (it was "Latest" until App 11.0.6: M for Performance, L for Ultra Performance, K otherwise), a preset (K, L, M), Custom | RTX | `NGX_DLSS_SR_OVERRIDE`, `…_RENDER_PRESET_SELECTION` (A=1…Z=26, Default, Latest) | `DXVK_NVAPI_DRS_` + the same key (dxvk-nvapi ≥ 0.9; presets K–O need a newer commit) | NVIDIA |
| DLSS Override: Ray Reconstruction model | same | RTX | `NGX_DLSS_RR_*` | same | NVIDIA |
| DLSS Override: Frame Generation model | Latest, Preset B (enhanced FG), Model C (FG performance, beta) | RTX 40/50 | `NGX_DLSS_FG_OVERRIDE`, `…_RENDER_PRESET_SELECTION` | same (dxvk-nvapi 0.9.3) | NVIDIA |
| DLSS Override: render resolution | DLAA, Ultra Performance, Custom 33–100 % (driver ≥ 572.83) | RTX | `NGX_DLSS_SR_MODE`, `NGX_DLSS_SR_OVERRIDE_SCALING_RATIO`, `NGX_DLAA_OVERRIDE` | same | NVIDIA |
| DLSS Override: Frame Generation mode | Fixed 2×–6× (6× needs a newer FG DLL in the game); **Dynamic** to max refresh or a custom fps (driver ≥ 595.97; not with a frame limiter or V-Sync) | MFG, Dynamic and 6× RTX 50 | `NGX_DLSSG_MODE`, `NGX_DLSSG_MULTI_FRAME_COUNT`, `NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE`, `NGX_DLSSG_DYNAMIC_MULTI_FRAME_COUNT_MAX` | same (dynamic: "limited/incomplete" in dxvk-nvapi 0.9.2; 0.9.3 knows the keys) | NVIDIA |
| Global DLSS Override (App 11.0.5) | all compatible games | RTX | — | the global scope (section 3.5) | NVIDIA |
| DLSS Override **allow-list** | only listed titles show the override, and only when DLSS is on in the game | | | no allow-list: dxvk-nvapi answers any game (steer 10) | |
| Newer DLSS in old games | the driver downloads snippets (NGX updater, "DLSS Override" model updates) | RTX | | `PROTON_ENABLE_NGX_UPDATER=1` (driver's NGX core + `nvngx_update.exe`; section 2) | NVIDIA |
| Smooth Motion (driver frame generation) | on/off | RTX 50, then RTX 40 (App 11.0.5); D3D11/12, Vulkan | | **`NVPRESENT_ENABLE_SMOOTH_MOTION=1`**: the driver's implicit `VK_LAYER_NV_present` (575.51 beta for RTX 50, RTX 40 since 580.82); Vulkan only, which covers DXVK and vkd3d-proton games; NVIDIA says not to combine it with DLSS FG | NVIDIA RTX 40+ |
| Max Frame Rate | off or a cap | all | `FRL_FPS` | no driver limiter on Linux; DXVK `dxgi.maxFrameRate`/`d3d9.maxFrameRate`, `VKD3D_FRAME_RATE`, gamescope `-r`, MangoHud, the Sight layer (v2) | all |
| Background Application Max Frame Rate | a cap | all | `APPIDLE_DYNAMIC_FRL_FPS` (unverified) | the Sight layer's background cap (v2) | all |
| Low Latency Mode | Off, On (one queued frame), Ultra (just-in-time submission); D3D9/11 only | all | | `dxgi.maxFrameLatency`/`d3d9.maxFrameLatency` (On), DXVK's `dxvk.latencySleep` (closest to Ultra; DXVK 2.6–3.1, removed on master 2026-11-06), `VKD3D_SWAPCHAIN_LATENCY_FRAMES` | all |
| NVIDIA Reflex | in the game, not the App | all | | the game's Reflex through dxvk-nvapi → DXVK (≥ 2.6; `dxvk.disableNvLowLatency2` in 3.1, renamed `dxvk.enableNvLowLatency2` on master) or vkd3d-proton (≥ 2.12) → `VK_NV_low_latency2`; Windows-Vulkan games through dxvk-nvapi 0.9.3 + driver R615 (`VK_NV_low_latency` rev 2), or its older `DXVK_NVAPI_VKREFLEX` layer | NVIDIA (LatencyFleX for others) |
| Vertical sync | app, off, on, half/third/quarter refresh, Fast | all | `VSYNCMODE` | DXVK `dxgi.syncInterval`/`d3d9.presentInterval`, `VKD3D_SWAPCHAIN_PRESENT_MODE`, the layer's present-mode override (v2); `__GL_SYNC_DISPLAY_DEVICE` for Vulkan FIFO only | all |
| Anisotropic filtering, texture LOD bias, "no negative LOD bias", texture quality | legacy settings | all | `ANISO_MODE_*`, `LODBIASADJUST`, `PS_TEXFILTER_NO_NEG_LODBIAS`, `QUALITY_ENHANCEMENTS` | DXVK `d3d11.samplerAnisotropy`, `d3d11.samplerLodBias`, `d3d11.clampNegativeLodBias` (and `d3d9.`); none for D3D12 | all |
| Power management mode | Optimal, Adaptive, Prefer maximum performance, … | all | `PREFERRED_PSTATE` | no per-application control on Linux (nvidia-smi clock locks are system-wide and need root) | — |
| Shader cache size | default … unlimited | all | | Quiver already sets `__GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1` for every launch (whether Vulkan honours it is unverified) | NVIDIA |
| RTX HDR, RTX Dynamic Vibrance | per game, RTX 20+ | RTX | | **none** on Linux. Native HDR: `PROTON_ENABLE_HDR=1` → `DXVK_HDR=1` with Wine-Wayland or gamescope `--hdr-enabled` | — |
| Image Scaling (NIS) | global, sharpening | all | | gamescope `-F nis`/`-F fsr`; `__GL_SHARPEN_ENABLE` (documented for OpenGL and Vulkan) | gamescope: all |
| Resizable BAR per app, Automatic shader compilation, Optimal settings ("Optimize") | | | | no equivalent; out of scope | — |

Sources: NVIDIA App release highlights (<https://www.nvidia.com/en-us/software/nvidia-app/release-highlights/>),
DLSS 4.5 dynamic MFG (<https://www.nvidia.com/en-us/geforce/news/dlss-4-5-dynamic-multi-frame-generation-6x-mode-released/>),
DLSS 4 overrides KB (<https://nvidia.custhelp.com/app/answers/detail/a_id/5620>),
Global overrides and Smooth Motion for RTX 40 (<https://www.nvidia.com/en-us/geforce/news/nvidia-app-global-dlss-overrides-rtx-40-series-smooth-motion/>),
RTX HDR KB (<https://nvidia.custhelp.com/app/answers/detail/a_id/5521>),
`NvApiDriverSettings.h` (<https://github.com/NVIDIA/nvapi/blob/main/NvApiDriverSettings.h>),
Low Latency Mode (<https://www.nvidia.com/en-us/geforce/news/gamescom-2019-game-ready-driver/>),
the Linux driver README for 575.51.02, 580.82.07 and 615.78.08 (Smooth Motion, application profiles;
<https://download.nvidia.com/XFree86/Linux-x86_64/615.78.08/README/README.txt>), NVIDIA's Linux
gaming guide (<https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/gaming.html>),
dxvk-nvapi releases (<https://github.com/jp7677/dxvk-nvapi/releases>), DXVK 2.6
(<https://github.com/doitsujin/dxvk/releases/tag/v2.6>), vkd3d-proton 2.12
(<https://github.com/HansKristian-Work/vkd3d-proton/releases/tag/v2.12>), driver 615.78.08
(`VK_NV_low_latency2` rev 3 for vkd3d-proton with FG;
<https://www.gamingonlinux.com/2026/10/nvidia-driver-615-78-08-released-for-linux-notes-a-new-feature-to-help-vkd3d-proton-performance/>).

**Linux driver application profiles** (`~/.nv/nvidia-application-profiles-rc`) don't help: the
README says they're "only supported by the NVIDIA Linux GLX implementation", and the keys are
OpenGL ones (`GLSyncToVblank`, `GLShaderDiskCache`, `GLSharpenEnable`, `GLVRRAllowed`, …). No
DLSS, limiter, latency or power keys. Quiver doesn't use them.

Unverified: the App's exact V-Sync, power and NIS labels; whether the driver (not only the App)
enforces the DLSS allow-list; the first Linux driver for DLSS FG and MFG under Proton (user
reports show 4× MFG working on 580).

---

## 2. Facts this design rests on

Found while preparing this (read-only), on the user's machine: Arch, `nvidia-utils-beta
620.70.06`, RTX 5070 Laptop (Blackwell, `GB200` in dxvk-nvapi's terms), Hyprland.

**How Quiver runs games today**

- `launcher::play` runs `python3 <build>/proton run …` directly (no umu), with
  `STEAM_COMPAT_DATA_PATH`, a stub client path, `SteamAppId=0`, the shader cache variables, the
  Steam Input `PROTON_DISABLE_HIDRAW` list, then `Config.env` (default
  `PROTON_ENABLE_NVAPI=1`, `DXVK_ENABLE_NVAPI=1`), then the resolved launch settings (`env`,
  `WINEDLLOVERRIDES`, `args`, `wrappers` before `python3`). The build is the game's setting or
  the newest GE-Proton in `~/.local/share/Steam/compatibilitytools.d` (GE-Proton11-12 here).
  The bridge, the first Wine process, gets the same `settings.env`, so prefix-wide state
  (wineserver, winebus) sees it too.
- The perf hub (`crates/quiverd/src/perf/launch.rs`) adds `QUIVER_SESSION`, `QUIVER_PERF_DIR`,
  `QUIVER_SIGHT=1` and an explicit `VK_INSTANCE_LAYERS` order (the user's layers, then vkBasalt
  and lsfg-vk when their variables are on, then Sight, with obs-vkcapture on the side the
  capture setting says), **only while the HUD is enabled** for the game. The session dir
  `/dev/shm/quiver-<uid>/<session>/` likewise exists only then.
- Steam games go through `quiver steam-run %command%`, which runs on the host in front of
  Steam's command and applies `env`, `dll_overrides`, `args` and `wrappers` from
  `Request::SteamLaunchPlan`; **Steam picks the Proton build**, so a tweak can't assume GE
  there. Which variables pressure-vessel passes through is unverified (STEAM-COPIES §4).
- The Sight layer (`crates/quiver-sight-layer`) hooks instance and device creation (it records
  the application and **engine name**, which is `DXVK` or `vkd3d` under Proton, and whether the
  device enabled `VK_NV_low_latency2` and present-wait), swapchain creation and
  `vkQueuePresentKHR`. It never sleeps in present today (SIGHT §3.5: "Reflex and FG pacing own
  that").

**What the Proton builds contain** (strings and scripts, read-only)

| | GE-Proton11-12 (Quiver's default) | Proton Experimental (11.0-20261112) | Proton 10.0-4b |
|---|---|---|---|
| DXVK | v3.1.1-174 | (not checked) | (not checked) |
| vkd3d-proton | 1.1-5687 | | |
| dxvk-nvapi | **v0.9.3** | **v0.9.3** | v0.9.0-10 |
| DRS settings dxvk-nvapi knows by name | SR/RR/FG overrides and presets, scaling ratios, `NGX_DLSSG_MODE`, `MULTI_FRAME_COUNT`, `DYNAMIC_TARGET_FRAME_RATE`, `DYNAMIC_MULTI_FRAME_COUNT_MAX`, NR, `SL_DLSS_OVERRIDE` | same | SR/RR overrides and presets, `MULTI_FRAME_COUNT`, `FG_OVERRIDE`; no FG presets, no dynamic MFG |
| Copies the driver's `nvngx_update.exe` into the prefix | **no** (only `_nvngx.dll`, `nvngx.dll`) | **yes** (`_nvngx.dll`, `nvngx.dll`, `nvngx_update.exe`) | no |
| dxvk-nvapi Reflex layer for native Vulkan (`DXVK_NVAPI_VKREFLEX`) | yes (`share/dxvk-nvapi-vkreflex-layer`) | | |
| Upscaler DLL downloads (`PROTON_DLSS_UPGRADE`, `_XESS_`, `_FSR4_`, `_STREAMLINE_`, `PROTON_USE_OPTISCALER`) | yes, from `loathingkernel.github.io/proton-upscalers` (a third-party GitHub Pages mirror of vendor DLLs); `PROTON_DLSS_UPGRADE` also sets `DXVK_NVAPI_DRS_SETTINGS` SR/RR/FG overrides; `PROTONFIXES_DISABLE_UPSCALERS` turns the whole thing off | no | no |
| `PROTON_DLSS_INDICATOR` | sets `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS=DLSSIndicator=1024,DLSSGIndicator=2,`, **else defaults it to `=0,…=0`** (resets the registry each launch) | no | no |
| HDR | `PROTON_ENABLE_HDR` / `PROTON_USE_HDR` → `DXVK_HDR=1`; `PROTON_FORCE_HDR` adds `WAYLANDDRV_FORCE_HDR=1` | | |
| Frame rate | `DXVK_FRAME_RATE` / `VKD3D_FRAME_RATE` / `PROTON_FRAME_RATE` rewritten into `DXVK_CONFIG` as `dxgi.maxFrameRate` and `d3d9.maxFrameRate` (protonfixes `setup_frame_rate`, "HACK") | | |

- **DXVK 3.1 has no `DXVK_FRAME_RATE` variable any more** (removed in DXVK 3.0 per its release
  notes; it isn't among the binary's
  `DXVK_*` strings: `DXVK_CONFIG`, `DXVK_CONFIG_FILE`, `DXVK_HDR`, `DXVK_NO_HDR`, `DXVK_HUD`,
  `DXVK_LOG_*`, `DXVK_FILTER_DEVICE_*`, …), which is why GE rewrites it. Quiver writes
  `DXVK_CONFIG` itself and doesn't depend on that hack. Option names present in this DXVK:
  `dxgi.maxFrameRate`, `d3d9.maxFrameRate`, `dxvk.maxFrameRate`, `dxgi.maxFrameLatency`,
  `d3d9.maxFrameLatency`, `dxgi.syncInterval`, `d3d9.presentInterval`, `dxvk.tearFree`,
  `dxvk.latencySleep`, `dxvk.latencyTolerance`, `dxvk.enablePresentTiming`,
  `d3d11.samplerAnisotropy`, `d3d11.samplerLodBias`, `d3d11.clampNegativeLodBias` (and the
  `d3d9.` equivalents), `dxgi.enableHDR`, `dxvk.disableNvLowLatency2`. **DXVK's options move
  between releases:** master removed `dxvk.latencySleep`/`latencyTolerance` on 2026-11-06 and
  renamed `disableNvLowLatency2` to `enableNvLowLatency2`
  ([dxvk.conf history](https://github.com/doitsujin/dxvk/commits/master/dxvk.conf)). So bindings
  `require` an option by name, found in the build probe's string table of DXVK's DLLs, rather
  than a version range.
- DXVK logs `Found config file:` and `Engaging frame rate limiter.`; dxvk-nvapi logs `Applying
  the following DRS settings when requested by the application (…)`. Those are the evidence
  lines section 6 uses.
- vkd3d-proton has `VKD3D_FRAME_RATE`, `VKD3D_SWAPCHAIN_LATENCY_FRAMES`,
  `VKD3D_SWAPCHAIN_PRESENT_MODE`, `VKD3D_SWAPCHAIN_IMAGES`, and a swapchain
  `SetTargetFrameRate` that DXVK's `dxgi.dll` calls with `dxgi.maxFrameRate`, so one
  `DXVK_CONFIG` entry caps D3D12 games too (inferred from the symbols; spike T2).
- **The NGX updater is in the driver.** The driver's `_nvngx.dll` (Proton's NGX core) contains
  `PROTON_ENABLE_NGX_UPDATER`, `Enabling updater for WINE`, `\nvngx_update.exe`, and
  `unable to launch NGX Updater … Can only use files that have already been downloaded to the
  cache`. NVIDIA's driver README documents the variable ("set to 1 to enable the updater for
  Wine/Proton"). **But the user's driver package doesn't ship `nvngx_update.exe`**
  (`pacman -Ql nvidia-utils-beta`: `/usr/lib/nvidia/wine/{_nvngx.dll,nvngx.dll,
  nvngx_dlssd.dll}` only), and GE-Proton wouldn't copy it anyway. So on this machine today,
  newer DLSS through NVIDIA's own channel doesn't work (spike T1).
- dxvk-nvapi's README: driver settings are passed with `DXVK_NVAPI_DRS_SETTINGS=k=v,k=v` or one
  `DXVK_NVAPI_DRS_<KEY>=value` per setting (the dedicated variable wins; names and values are
  case-insensitive, the variable name isn't); `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS` "permanently
  modifies the registry"; and "applying driver settings does not override NGX snippets/dlls",
  i.e. a preset override can only pick presets the DLL the game loads contains, unless the NGX
  updater fetched a newer one.
  ([README](https://github.com/jp7677/dxvk-nvapi), [Passing driver settings](https://github.com/jp7677/dxvk-nvapi/wiki/Passing-driver-settings))

**aco-dlss** (read-only) loads NGX from the driver (`_nvngx.dll`) and ships the DLSS SDK's
`nvngx_dlss.dll` 310.9.16; it injects Reflex (`NvAPI_D3D_Sleep` and markers through dxvk-nvapi
to `VK_NV_low_latency2`), caps through Reflex or a CPU wait (`reflex.fps_limit`,
`reflex.limiter`), runs DLSS-G on a second, vkd3d-proton device and presents two frames per
game frame through the game's DXGI swapchain, and biases texture LOD itself
(`upscaler.mip_bias`). Every one of those overlaps a tweak, which is why section 7 exists.

---

## 3. Model

### 3.1 Why the Options engine (and what it needs)

| Option | For | Against |
|---|---|---|
| **(a) A built-in Options surface with mechanism bindings** (recommended) | one schema format, one form generator (app, CLI, Sight), one store with revisions, provenance and undo, one outcome model, `when`/presets/levels for free; mods' `provides` can point at a tweak id in the same namespace; agents already know `options set` | the engine needs scopes beyond profile and a `via` binding; the platform surface's resolution differs from a mod surface's |
| (b) A separate subsystem like HUD settings (`tweaks.global`, `tweaks.game.<id>` in quiverd's config) | scopes come naturally | a second form generator, history and outcome model; no shared vocabulary with mods, so "controlled by DLSS" needs glue both ways |
| (c) Presets of launch settings (named env bundles) | almost free: launch settings exist | no types, no availability per vendor, no evidence, no live path, no mod interplay; that's what people do by hand in Steam launch options today |

What (a) adds to the engine:

1. **Surface kind `platform`**, key `quiver/tweaks`, always present for every game (it's in
   the catalog with an empty `[match]`), listed first.
2. **Scopes** for platform surfaces only: `global` → `game` → `profile` (vanilla is a
   profile), resolved like HUD layouts. Mod and game surfaces stay per profile (MOD-CONFIG
   steer 5). A value can be set at any scope; the row shows where the effective value comes
   from ("Global", "This game", "DLSS profile") and where a lower scope overrides it.
3. **`via` bindings**: an ordered list of mechanisms per option, each with parameters, a
   `requires` predicate over the capability set, a value map, its apply class and its
   evidence. The resolver picks the first binding whose `requires` holds (or several, for
   tweaks that need more than one write, like a DLSS preset that is SR override + preset
   selection).
4. **Effects**: a per-session record per tweak (section 6), next to Options' save outcomes.
5. **`provides`** in mod and game schemas (section 7).

Everything else (validation in quiverd, revisions in `.jsonl`, `danger`, crash correlation,
"known good", safe launch with `--options defaults`, which also skips tweaks) is reused as is.

### 3.2 The schema: `options/quiver-tweaks.toml`

The catalog file compiles in like `aco-dlss.toml`. An excerpt (full list in section 4):

```toml
format = 1
surface = "quiver"
kind = "platform"                 # new: scopes global/game/profile; `via` bindings
title = "Quiver Tweaks"
help = "Overrides Quiver applies to any game it launches: upscaling, frame generation, frame rate and latency."

[match]                           # every game

[[page]]
id = "frames"
title = "Frame rate and sync"
options = ["fps.cap", "fps.cap_background", "sync.vsync", "sync.max_latency", "latency.mode"]

[[page]]
id = "dlss"
title = "NVIDIA DLSS"
options = ["dlss.sr.model", "dlss.sr.scale", "dlss.rr.model", "dlss.fg.model", "dlss.fg.frames",
           "dlss.fg.target", "dlss.runtime", "dlss.indicator"]

[option."fps.cap"]
type = "float"
label = "Max frame rate"
help = "Caps the frame rate. With a VRR display, a few frames under the refresh rate keeps VRR engaged."
unit = "fps"
min = 20
max = 500
step = 1
special = [{ value = 0, label = "No cap" }, { value = "vrr", label = "Refresh rate − 3" }]
level = "basic"
# One mechanism is used; the first whose `requires` holds. A provider (section 7) comes first
# implicitly.
via = [
  { mech = "dxvk", set = { "dxgi.maxFrameRate" = "{v}", "d3d9.maxFrameRate" = "{v}" },
    apply = "launch", requires = { api = ["d3d8", "d3d9", "d3d10", "d3d11", "d3d12", "unknown"] },
    confirm = { log = "dxvk", line = "Engaging frame rate limiter" } },
  { mech = "env", set = { VKD3D_FRAME_RATE = "{v}" }, apply = "launch",
    requires = { api = ["d3d12"], dxgi = "not-dxvk" } },
  # Native Vulkan games at launch, and every game for a change made while it runs (steer 4).
  { mech = "layer", control = "limiter.fps", apply = "live", phase = 2,
    requires = { layer = true, any = [{ api = ["vulkan"] }, { session = "running" }] } },
  { mech = "wrapper", wrapper = "gamescope", args = ["-r", "{v}"], apply = "launch",
    requires = { tweak = { "scale.gamescope" = "@on" } } },
]

[option."dlss.sr.model"]
type = "enum"
label = "DLSS Super Resolution model"
help = "Overrides the DLSS preset the game asks for, as the NVIDIA App's DLSS Override does. Only presets in the DLSS version the game loads are available unless the updater is on."
values = [
  { value = "app", label = "Game's choice" },
  { value = "latest", label = "Recommended (latest)" },
  { value = "K", label = "K: DLSS 4 transformer" },
  { value = "L", label = "L: DLSS 4.5, ultra performance" },
  { value = "M", label = "M: DLSS 4.5, performance" },
]
open = true                       # other preset letters, A–O
level = "basic"
via = [
  { mech = "nvapi-drs", apply = "launch",
    requires = { vendor = "nvidia", rtx = true, nvapi = ">=0.9.0" },
    set = { NGX_DLSS_SR_OVERRIDE = "on",
            NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION = "render_preset_{v}" },
    map = { app = "@unset", latest = "render_preset_latest" },
    confirm = { log = "nvapi", line = "Applying the following DRS settings when requested",
                key = "NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION" } },
]
```

New schema fields, all validated by `options lint`: `kind`, `via` (`mech`, mechanism
parameters, `requires`, `map` with `@unset` meaning "write nothing", `apply`, `at`,
`confirm`), and `phase` (a binding a Quiver build doesn't implement yet is ignored with a
lint note, so the catalog can describe v2 mechanisms early). `{v}` is the value as text after
`map`.

### 3.3 Mechanisms

| `mech` | What it writes | Reaches | Apply | Evidence | Routes |
|---|---|---|---|---|---|
| `env` | launch env (`PROTON_*`, `VKD3D_*`, `WINE_*`, …) | Proton's script, Wine, vkd3d-proton | launch | Proton's `Options:` log line with `PROTON_LOG=1` when asked for, else none | Quiver; `steam-run` (subject to pressure-vessel) |
| `dxvk` | entries merged into `DXVK_CONFIG` (`k = v; k = v`), after the user's own | DXVK's d3d8/9/10/11 and dxgi (dxgi also sets vkd3d-proton's target frame rate) | launch (read at device or swapchain creation) | DXVK log: `DXVK_LOG_PATH` pointed at the session's log dir; layer status (present mode, present intervals) | both |
| `vkd3d` | `VKD3D_CONFIG` flags merged, `VKD3D_SWAPCHAIN_*` | vkd3d-proton | launch | vkd3d log (`VKD3D_DEBUG=info` only when the user asks for diagnostics); layer status | both |
| `nvapi-drs` | one `DXVK_NVAPI_DRS_<KEY>` per setting (never `DXVK_NVAPI_DRS_SETTINGS`, which the user or GE's script may own) | dxvk-nvapi, which answers the driver-settings queries NGX and Streamline make | launch; read when the game creates a DLSS feature | dxvk-nvapi log (`DXVK_NVAPI_LOG_LEVEL=info`, `DXVK_NVAPI_LOG_PATH` = session log dir): which settings came from the environment and which the application requested | both |
| `ngx` | `PROTON_ENABLE_NGX_UPDATER=1`; `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS` | the driver's NGX core in the prefix | launch; the indicator persists in the prefix's registry | models appearing under the prefix's `ProgramData/NVIDIA/NGX/models`; the indicator itself | both (Steam: only with a Proton that copies `nvngx_update.exe`) |
| `layer` | a control block in the session dir, `tweaks.ctl` (section 5) | the Sight layer, per present | live, or `at` swapchain creation | the layer's status block echoes the control generation it applied and what it did | both, where the layer runs |
| `vk-layer` | enable variable + a generated config in the session dir (lsfg-vk, vkBasalt), and their place in `VK_INSTANCE_LAYERS` | third-party Vulkan layers | launch; live where the layer re-reads its config | their own log files, pointed at the session's log dir (`LSFGVK` `log_file`, `VKBASALT_LOG_FILE`, `NVPRESENT_LOG_FILE`); `VK_LOADER_DEBUG=layer` when diagnosing order | both |
| `wrapper` | arguments to a wrapper Quiver adds (gamescope) | the process tree | launch | the wrapper process exists; its log | Quiver; `steam-run` puts wrappers before Steam's command |
| `mangohud` | entries in a per-session config file (`MANGOHUD_CONFIGFILE`), rewritten for live changes: MangoHud re-reads it on change (inotify); its `control=` socket has no fps-limit command | MangoHud, if the launch runs it | launch; live (`fps_limit`, `fps_limit_method`) | `show_fps_limit` on screen; present ring `measured` | both |
| `mod` | a provider's option (section 7) | the mod | the mod option's own apply class | the mod's metrics | both |

Merging rules, all in one place (the launch plan builder that already merges mods' `[launch]`
and the profile's explicit settings):

- Tweaks compile **after** mods' `[launch]` and **before** the profile's explicit launch
  settings. If the person set `DXVK_CONFIG` or a `DXVK_NVAPI_DRS_*` by hand, their value
  wins for that key and the tweak row says "your launch setting `DXVK_CONFIG` sets
  dxgi.maxFrameRate = 120; this tweak is ignored" (a setting conflict, as MOD-CONFIG §9
  does for options).
- `DXVK_CONFIG` is merged per key: the user's entries, then Quiver's, `;`-separated, no
  duplicates. A `dxvk.conf` in the game folder (or a mod's) still applies; spike T2 confirms
  that `DXVK_CONFIG` overrides the file per key, as the design assumes.
- Quiver never sets GE's convenience switches (`PROTON_DLSS_UPGRADE`, `PROTON_DLSS_INDICATOR`,
  `DXVK_FRAME_RATE`) for tweaks: they behave differently per build and the first downloads
  vendor DLLs from a mirror (steer 5). It writes the underlying variables directly, which every
  Proton with that component understands.

### 3.4 The capability set

Computed per launch, cached where it's slow, shown by `quiver tweaks explain`:

| Capability | Source |
|---|---|
| GPU vendor(s), architecture, RTX or not | NVML for NVIDIA (quiverd's system producer already opens it); `/sys/class/drm/card*/device/vendor` and Vulkan device properties otherwise; hybrid laptops report the GPU games run on (the dGPU) |
| Driver version | NVML / `/proc/driver/nvidia/version`; Mesa from the Vulkan driver info |
| Proton build features | a probe per build directory, cached by its `version` file: DXVK, vkd3d-proton and dxvk-nvapi versions (the `version` files under `files/lib/wine/*`, as read in section 2), the option and variable names present in their DLLs (`dxvk.latencySleep`, `DXVK_NVAPI_DRS_`, `NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE`, …: bindings require names, not versions), and the `PROTON_*` names the `proton` script mentions; for Steam routes, the build Steam maps the app to (`CompatToolMapping` or the default), unknown if not determinable |
| NGX pieces | `/usr/lib/nvidia/wine/` (or wherever `libGLX_nvidia.so.0` resolves, as Proton finds it): `nvngx.dll`, `_nvngx.dll`, `nvngx_update.exe` |
| Route | `QuiverProton` or `SteamClient{hooked}` (STORES §3.6); plain Steam launches without `steam-run` get no tweaks |
| Game API | the Sight layer's last status for this game: engine name `DXVK` (D3D8–11), `vkd3d` (D3D12), anything else (native Vulkan), none (OpenGL or no layer); `unknown` on the first launch, when bindings that `require` an API apply only if they're harmless for the others (a `DXVK_CONFIG` entry is) |
| Anti-cheat | the curated list and install-time detection (STORES §3.11) |
| Layer | whether the Sight layer will run this launch (HUD or a layer tweak on, not anti-cheat-flagged) |
| Display | refresh rate, VRR and HDR state of the game's monitor (quiverd's display producer, Hyprland IPC) for `vrr` caps and HDR |
| Providers | the profile's resolved mod stack and game surfaces with `provides` (section 7) |

### 3.5 Scopes and storage

```
<data>/tweaks/global.jsonl                       global scope (revisions)
games/<dir>/options/@game.jsonl                  game scope, surface quiver/tweaks only
games/<dir>/options/<profile>.jsonl              profile scope (existing file; key "quiver/tweaks")
```

Resolution for a tweak in one launch, lowest to highest: the schema default (always "game's
choice" or "off": Quiver changes nothing unless asked), global, game, profile, session (an
in-game change saved later, MOD-CONFIG steer 12). Each revision file is whole-state like the
others, with the same `source` actors (`gui`, `cli`, agents, `in-game`). Play sessions record
the three revisions they launched with (`PlaySession.tweaks = {global, game, profile}`), so
"known good" and crash correlation cover tweaks, and `quiver play GAME --options defaults`
launches with none.

A **tweak preset** is a named set across pages ("Competitive": no cap, VSync off, low latency
on, max frame latency 1; "Quiet laptop": cap 60, background cap 15), applied at any scope as
one change, exactly like a schema `[[preset]]`.

---

## 4. Tweak catalog (v1 and later)

Every tweak defaults to "game's choice" or "off" (steer 15). Mechanisms are listed in the order
the resolver tries them; a provider (section 7) always comes first. "D3D9–11" means DXVK,
"D3D12" means vkd3d-proton, "Vulkan" means a Windows or native Vulkan game. Apply classes are
Options' (`launch` = next launch, `live`, `at` a moment).

### 4.1 v1: launch-time, configuration only

| Id · label | Values | Mechanisms | GPUs · APIs | Apply | Evidence | Risks and notes |
|---|---|---|---|---|---|---|
| `fps.cap` · Max frame rate | no cap, 20–500 fps, `vrr` (refresh − 3 on the game's monitor) | `dxvk` `dxgi.maxFrameRate` + `d3d9.maxFrameRate` (dxgi also drives vkd3d-proton's limiter); `env` `VKD3D_FRAME_RATE` (D3D12 without DXVK's dxgi); `layer` (v2; Vulkan, live) | all · D3D8–12 (v1), Vulkan (v2) | launch (v1), live (v2) | DXVK `Engaging frame rate limiter`; present ring `measured` | DXVK's own default (`0`) already limits to the refresh rate under VSync in emulated display modes, and its dxvk.conf warns against using it when an external limiter is available; caps **presented** frames: wrong with frame generation (aco-dlss: `conflicts`); NVIDIA says Dynamic MFG doesn't work with a limiter, so `dlss.fg.frames = dynamic` blocks it (built-in conflict) |
| `sync.vsync` · Vertical sync | game's, on, off, off without tearing | `dxvk` `dxgi.syncInterval` (0/1) + `d3d9.presentInterval` + `dxvk.tearFree` (off without tearing = `syncInterval 0`, `tearFree True` → MAILBOX); `env` `VKD3D_SWAPCHAIN_PRESENT_MODE` (`IMMEDIATE`, `MAILBOX`, `FIFO`, `FIFO_RELAXED`, `FIFO_LATEST_READY`; logs `Overriding swapchain present mode to …`) for D3D12 games whose swapchain DXVK's dxgi doesn't already set (`dxgi.syncInterval` also reaches vkd3d-proton through DXVK's dxgi); `layer` present-mode override (v2, Vulkan) | all | launch (v1), at swapchain creation (v2) | Sight layer's present mode in the status block | with VRR, "off + cap under refresh" is the usual best; tearing needs Hyprland's `allow_tearing` and the window rule (the display producer already reports it) |
| `sync.max_latency` · Max frames in flight | game's, 1, 2, 3 | `dxvk` `dxgi.maxFrameLatency` + `d3d9.maxFrameLatency`; `env` `VKD3D_SWAPCHAIN_LATENCY_FRAMES` | all · D3D9–12 | launch | none beyond the layer's present intervals (unconfirmed) | the NVIDIA App's "Low Latency Mode: On" equivalent; 1 can cost throughput in CPU-bound games |
| `latency.mode` · Low latency | game's, off, on | provider (aco-dlss `reflex.mode`); `dxvk` `dxvk.latencySleep = True` for games without Reflex (D3D9–11, any vendor: `VK_NV_low_latency2` where present, DXVK's own algorithm otherwise; no effect with an in-game or external limiter; **only in DXVK 2.6–3.1.x**: DXVK master removed it on 2026-11-06, so the binding `requires` the option string in the build, not a version); for games with Reflex: nothing to force (the game's own Reflex switch decides), Quiver only makes sure NVAPI is exposed (`PROTON_ENABLE_NVAPI=1`, already the default) and Reflex isn't disabled in DXVK; `off` sets `dxvk.latencySleep = False` and `dxvk.disableNvLowLatency2 = True` (3.1's name; `enableNvLowLatency2 = False` on newer DXVK); AMD and Intel, v2: `vk-layer` **low_latency_layer** (Korthos Software, MIT, 2026; implements `VK_NV_low_latency2` and `VK_AMD_anti_lag` on Mesa; `LOW_LATENCY_LAYER=1`), which lets games' own Reflex or Anti-Lag 2 work there | all (latency sleep), NVIDIA (Reflex; AMD/Intel with low_latency_layer) · D3D9–11; D3D12 only through the game's Reflex or Anti-Lag 2 | launch | dxvk-nvapi log (`VkNvLowLatency2 implementation`), DXVK log, layer: `VK_NV_low_latency2` enabled and sleep mode set | the NVIDIA App's "Ultra" is closest to DXVK's latency sleep; two sleeps per frame are prevented by the provider rule; v3 adds layer injection for D3D12/Vulkan (section 5). **AMD:** vkd3d-proton implements the AMD Anti-Lag 2 game API on `VK_AMD_anti_lag` (`d3d12_amd_ext_anti_lag_UpdateAntiLagState` in GE-Proton11-12's `d3d12core.dll`), which Mesa provides through its implicit `VK_LAYER_MESA_anti_lag` (`ENABLE_LAYER_MESA_ANTI_LAG=1`, installed here by `vulkan-mesa-implicit-layers`); `on` sets it on AMD for D3D12 games that integrate Anti-Lag 2 (whether it also needs `PROTON_NATIVE_AGS`-style AGS exposure is unverified, T9) |
| `dlss.sr.model` · DLSS Super Resolution model | game's, recommended (latest), K, L, M, other letters A–O | `nvapi-drs` `NGX_DLSS_SR_OVERRIDE=on` + `NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION=render_preset_<x>`, plus `SL_DLSS_OVERRIDE=on` for Streamline games (dxvk-nvapi ≥ 0.9.3) | NVIDIA RTX · any game using NGX/Streamline | launch | dxvk-nvapi: the application requested the key; mod metric; the indicator | only presets in the DLSS DLL the game loads, unless `dlss.runtime = updater`; overrides a mod's own preset (provider rule) |
| `dlss.sr.scale` · DLSS render resolution | game's, DLAA, quality, balanced, performance, ultra performance, custom 33–100 % | `nvapi-drs` `NGX_DLSS_SR_MODE` (+ `NGX_DLSS_SR_OVERRIDE_SCALING_RATIO` for custom), `NGX_DLAA_OVERRIDE=dlaa_on` for DLAA | NVIDIA RTX | launch | as above | the game's UI and internal buffers may not expect the changed ratio (some games letterbox or blur the UI); per-game deny-list |
| `dlss.rr.model` · Ray Reconstruction model | game's, recommended, presets | `nvapi-drs` `NGX_DLSS_RR_OVERRIDE` + `…_RENDER_PRESET_SELECTION` | NVIDIA RTX · games with RR | launch | as above | |
| `dlss.fg.model` · Frame Generation model | game's, latest, B, C | `nvapi-drs` `NGX_DLSS_FG_OVERRIDE` + `…_RENDER_PRESET_SELECTION` (dxvk-nvapi ≥ 0.9.3) | NVIDIA RTX 40/50 · games with DLSS-G | launch | as above | Model C is beta on Windows |
| `dlss.fg.frames` · Frame Generation mode | game's, 2×, 3×, 4×, 5×, 6×, dynamic | `nvapi-drs` `NGX_DLSSG_MULTI_FRAME_COUNT` (1 = 2× … 5 = 6×); dynamic: `NGX_DLSSG_MODE=dynamic` | NVIDIA RTX 50 for > 2× and dynamic | launch | as above; presented ÷ base when a provider fills `frame.base` | needs a game that already has DLSS-G on; dynamic is "limited/incomplete" in older dxvk-nvapi (`requires nvapi >= 0.9.3`); conflicts with `fps.cap` (dynamic) and with mods that expect 2× |
| `dlss.fg.target` · Dynamic target | max refresh, custom fps | `nvapi-drs` `NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE` (auto = `0x1000000`), `NGX_DLSSG_DYNAMIC_MULTI_FRAME_COUNT_MAX` | NVIDIA RTX 50 | launch | as above | `when = { "dlss.fg.frames" = "dynamic" }` |
| `dlss.runtime` · DLSS version | game's, NVIDIA updater | `ngx` `PROTON_ENABLE_NGX_UPDATER=1` (+ copying the driver's `nvngx_update.exe`, steer 6) | NVIDIA RTX | launch (the download happens in the background; it may take a launch to arrive) | the prefix's NGX models folder | some Unreal games crashed on second launch with the updater (NVIDIA bug 4776234, Proton forum) and older drivers had issues before R570: `experimental`, per-game deny-list; unavailable on this machine today (no `nvngx_update.exe`) |
| `dlss.indicator` · DLSS indicator | off, on | `ngx` `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS=DLSSIndicator=1024,DLSSGIndicator=2`; off writes `=0,=0` once after it was on | NVIDIA RTX | launch (persists in the prefix registry: section 9) | the indicator itself | `level = "advanced"`; the way to see what NGX really runs |
| `fg.smooth_motion` · NVIDIA Smooth Motion | off, on | `env` `NVPRESENT_ENABLE_SMOOTH_MOTION=1` (logs to the session dir with `NVPRESENT_LOG_FILE`); `VK_LAYER_NV_present` joins the layers that must stay above Sight | NVIDIA RTX 40+ (driver ≥ 580.82; RTX 50 since 575.51) · Vulkan, so every DXVK and vkd3d-proton game | launch | `NVPRESENT_LOG_LEVEL` log; presented ÷ base (no base source without a provider) | not with DLSS FG (NVIDIA's guidance) or a mod's FG: conflicts with `dlss.fg.*` overrides and with aco-dlss `fg.mode = d3d12`; frame caps cap presented frames |
| `tex.anisotropy` · Anisotropic filtering | game's, 2×, 4×, 8×, 16× | `dxvk` `d3d11.samplerAnisotropy`, `d3d9.samplerAnisotropy` | all · D3D9–11 | launch | none (unconfirmed) | none for D3D12 |
| `tex.lod_bias` · Texture LOD bias | game's, −2.0 … +1.0 (added to the game's own bias), "no negative bias" | `dxvk` `d3d11.samplerLodBias` / `d3d9.samplerLodBias`, `d3d11.clampNegativeLodBias` | all · D3D9–11 | launch | none | adds to a mod's bias (aco-dlss `provides` it, mode `own`) |
| `hdr` · HDR output | game's, on, off | `env` `PROTON_ENABLE_HDR=1` where the build knows it (GE), else `DXVK_HDR=1`; off: `DXVK_NO_HDR=1`; needs Wine-Wayland (`PROTON_ENABLE_WAYLAND=1`) or gamescope with `--hdr-enabled` on an HDR output | all | launch | layer: swapchain colour space (`HDR10_ST2084`, scRGB); DXVK log | Hyprland's HDR state comes from the display producer; `requires display.hdr` |

### 4.2 v2: live through the Sight layer, and third-party tools

| Id · label | Values | Mechanisms | GPUs · APIs | Apply | Evidence | Risks and notes |
|---|---|---|---|---|---|---|
| `fps.cap` (live) | as above | `layer` limiter: in-game changes take effect immediately; at the next launch the value goes back to the v1 mechanism (steer 4) | all · Vulkan, so every Proton game | live | layer status `limiting` + measured | stands down if the app's Reflex sleep mode has an interval; conflicts with present-time FG (aco-dlss) |
| `fps.cap_background` · Max frame rate in the background | off, 5–120 | `layer` limiter with quiverd's focus signal; MangoHud's has no focus input | all · Vulkan | live | layer status | the NVIDIA App's "Background Application Max Frame Rate" |
| `sync.vsync` (Vulkan) | as above | `layer` present-mode override | all · Vulkan | at swapchain creation | present mode in the status block | spike T4 |
| `fg.lsfg` · Lossless Scaling frame generation | off, 2×, 3×, 4×; flow scale; performance mode | `vk-layer` **lsfg-vk v2** (`VK_LAYER_LSFGVK_frame_generation`): a generated `version = 2` TOML in the session dir passed with `LSFGVK_CONFIG` (`[global] dll` = the person's `lsfg-vk.dll`, one `[[profile]]` with `active_in` = the game's exe, `multiplier`, `flow_scale`, `performance_mode`); configless `LSFGVK_ENV=1` + `LSFGVK_*` is the fallback | all GPUs (Vulkan 1.2; biggest FP16 gains on AMD) · Vulkan | launch; **live** for `multiplier`, `flow_scale`, `performance_mode` (lsfg-vk hot-reloads those: Quiver rewrites the file) | `log_file` in the session dir; presented ÷ base needs a base source | legitimacy rule 4; lsfg-vk forces FIFO by default (`override_present_mode`), which interacts with `sync.vsync`; conflicts with every other FG; must stay above Sight: **`launch.rs` names the v1 layer `VK_LAYER_LS_frame_generation` with `ENABLE_LSFG`, which v2 no longer uses** (fix with this work) |
| `post.sharpen` · Sharpening (CAS) | off, 0–1 | `vk-layer` vkBasalt (`ENABLE_VKBASALT=1`, `VKBASALT_CONFIG_FILE` = a generated file with `effects = cas`, `casSharpness`); gamescope `--sharpness` when `scale.gamescope` is on | all · Vulkan | launch (vkBasalt doesn't hot-reload) | `VKBASALT_LOG_FILE` in the session dir | vkBasalt is dormant upstream (last commit 2023-05) and its toggle key is X11-only: one reason to consider the layer's own CAS (v3); vkBasalt above Sight (already in `launch.rs`) |
| `scale.gamescope` · Spatial upscaling (gamescope) | off, FSR, NIS, integer, linear; render resolution; sharpness | `wrapper` gamescope `-w/-h` (game resolution), `-W/-H` (output), `-F` (`fsr`, `nis`, `sgsr`, `pixel`, `linear`, `nearest`), `-S` (`integer`, `fit`, …), `--sharpness` (0 = sharpest … 20), `-f`, `--adaptive-sync`, `--hdr-enabled`, `--backend`; `fps.cap` then may use gamescope's `-r`, and `fps.cap_background` its `-o` | all · any | launch | the gamescope process; layer: swapchain extent | nested gamescope on NVIDIA is fragile in 2026 (upscaling crash with explicit sync, gamescope issue #1662, still open; `--backend wayland` reported to help on Hyprland; HDR needs the `VK_LAYER_FROG_gamescope_wsi` layer, `ENABLE_GAMESCOPE_WSI`): probe and mark `experimental`; changes how Sight's panel and the HUD's desktop fallback reach the screen (SIGHT §8: gamescope nested works, the in-layer HUD works) |
| `fps.cap` (MangoHud) | as above | `mangohud` `fps_limit` (+ `fps_limit_method = late`) in a per-session `MANGOHUD_CONFIGFILE` that Quiver rewrites for live changes (MangoHud re-reads it on change; applying a new `fps_limit` on reload is likely but unverified), only when the launch runs MangoHud and the person picked it as the limiter | all | live | `show_fps_limit`; measured | MangoHud is a Linux layer: off for anti-cheat games |

### 4.3 v3: research

| Id | What | Mechanism | Notes |
|---|---|---|---|
| `fps.cap` (Reflex) | the cap as Reflex's minimum interval, live | `layer` rewriting `vkSetLatencySleepModeNV` | lowest-latency cap on NVIDIA for Reflex games; section 5 |
| `latency.mode` (inject) | low latency for D3D12/Vulkan games without Reflex | `layer` + `VK_NV_low_latency2` | only if measurements show a gain over `sync.max_latency = 1` |
| `post.sharpen` (built in) | CAS inside the Sight layer | `layer` | if vkBasalt proves awkward |
| — | Reflex latency readout for every Reflex game | `layer` `vkGetLatencyTimingsNV` | telemetry, fills `latency.frame` |

### 4.4 Not on Linux (shown greyed with the closest thing)

RTX HDR and RTX Dynamic Vibrance (no driver support; native HDR via `hdr`), power management
mode (no per-application control), Resizable BAR per app, "Optimal settings" (writes games' own
settings; that's what catalog game surfaces and their presets are for), automatic shader
compilation. **LatencyFleX** is not used: unmaintained since 2023, and dxvk-nvapi 0.9 dropped its
Reflex shim; low_latency_layer (above) is the live successor for AMD and Intel.

**Considered and left out as tweaks.** OptiScaler (GPL-3.0; swaps DLSS, FSR and XeSS inputs and
outputs and can add FSR3/XeFG frame generation; its own README says not to use it with online
games) is a DLL-proxy mod, not a configuration: if wanted, it belongs in the mod catalog with its
own options, installed by the person. GE-Proton's fullscreen FSR (`WINE_FULLSCREEN_FSR`,
`_STRENGTH` 0–5, `_MODE` performance/balanced/quality/ultra, `_CUSTOM_MODE=WxH`; Vulkan games in
true fullscreen only, in GE builds only, untested under Wine-Wayland) is a possible second
`scale.*` mechanism for GE builds where gamescope misbehaves (`requires proton.knows =
"WINE_FULLSCREEN_FSR"`), v2 at the earliest.

---

## 5. What the Sight layer can do

The layer sits at `vkQueuePresentKHR`, below DXVK, vkd3d-proton, any mod's frame generation
and any post-processing layer (SIGHT-HUD §2.4), in every Vulkan game Quiver launches with it.
That makes it the one vendor- and API-neutral place for **live** tweaks. Each capability, with
cost and risk:

| Capability | How | Vendors | Cost in the game | Risk | Verdict |
|---|---|---|---|---|---|
| **Frame rate limiter** | sleep in `vkQueuePresentKHR` before calling down, to an absolute deadline (`clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`, no spinning), next deadline = max(previous + interval, now − slack); MangoHud's `late` method | all | no GPU work; one syscall per frame | a second pacer fighting the game's Reflex, a mod's pacing (aco-dlss's present thread) or DXVK's `latencySleep`; under DXVK the present runs on DXVK's thread, so back-pressure reaches the game through the frame-latency wait, same as DXVK's own limiter | **v2**, opt-in, stands down by itself when the app enabled a low-latency sleep mode with a minimum interval or a provider owns `fps.cap` (steer 3) |
| **Background cap** | the same limiter with a second interval, engaged while quiverd says the game's window isn't focused (Hyprland IPC, already watched by the display producer) | all | as above | as above | **v2** |
| **Present-mode override** (VSync) | rewrite `VkSwapchainCreateInfoKHR::presentMode` (and `VkSwapchainPresentModesCreateInfoEXT` / per-present `VkSwapchainPresentModeInfoEXT` when DXVK switches modes with `VK_EXT_swapchain_maintenance1`) to a mode the surface supports | all | none | DXVK and vkd3d-proton switch present modes themselves; a forced mode they didn't ask for must still be in the surface's list or creation fails; applies at the next swapchain creation | **v2**, `apply = "at"`; `dxvk` mechanism preferred for D3D games |
| **Reflex-aware cap** | intercept `vkSetLatencySleepModeNV` (called by DXVK or vkd3d-proton on the game's behalf) and raise `minimumIntervalUs` to the cap; re-issue the app's last call with the new interval when the cap changes | NVIDIA | none | games that never call `SetSleepMode`; the app's own interval must win when stricter; multi-device (aco-dlss's second device) | **v3**: the lowest-latency cap on NVIDIA, live |
| **Latency readout** | call `vkGetLatencyTimingsNV` on the swapchain (rate-limited) when the app sets latency markers, and write the frame reports into the layer's ring | NVIDIA | one call per few frames | must not consume reports the app reads itself (spike T7: are reports per-caller or a queue?) | **v3**: fills the `latency.frame` role for every Reflex game, no mod needed |
| **Injected low latency** (game without Reflex) | enable `VK_NV_low_latency2` at device creation if supported and absent, chain `VkSwapchainLatencyCreateInfoNV`, call `vkSetLatencySleepModeNV` and `vkLatencySleepNV` around present | NVIDIA | small | the sleep belongs before input sampling on the game's thread, which the layer can't see: at present it's a weaker LatencyFleX; DXVK's `latencySleep` does this properly for D3D9–11 on the game's thread | **v3, research**; D3D9–11 use `dxvk.latencySleep` instead |
| **Sharpening** (CAS) | a fullscreen pass on the swapchain image after any FG and before the HUD composite; needs `STORAGE` or `COLOR_ATTACHMENT` usage added at swapchain creation (the HUD already adds the latter) | all | ~0.1–0.3 ms at 1440p (inferred from CAS's cost; spike) | sRGB/HDR format handling, a second pipeline per swapchain | **v3**; until then vkBasalt via `vk-layer` |
| Spatial upscaling | the game would have to render to a smaller swapchain than the window; the layer would have to lie about surface capabilities | | | high: that's gamescope's and Wine's job | **no**: `wrapper` gamescope |
| HDR, RTX-HDR-style inverse tone mapping | | | | quality is the whole point and needs a model | **no** |
| Frame generation | | | | lsfg-vk exists and is large | **no**: `vk-layer` lsfg-vk |

**Control plane.** A new 4 KiB file in the session dir, `tweaks.ctl`, written by quiverd and
mapped read-only by the layer, like `hud.image` (same validation rules, SIGHT-HUD §3.16):

```c
/* crates/quiver-perf (layout.rs), mirrored in include/quiver_perf.h */
#define QV_TWEAKS_MAGIC   0x4B41455754565551ull   /* "QUVTWEAK" */
#define QV_TWEAKS_VERSION 1
struct qv_tweaks_ctl {
    uint64_t magic; uint32_t version, size;
    uint64_t seq;                  /* seqlock: odd while quiverd writes */
    uint32_t flags;                /* QV_TW_LIMIT, QV_TW_LIMIT_BG, QV_TW_PRESENT_MODE, QV_TW_REFLEX_INTERVAL */
    uint32_t present_mode;         /* VkPresentModeKHR, when QV_TW_PRESENT_MODE */
    uint64_t interval_ns;          /* foreground cap; 0 = none */
    uint64_t bg_interval_ns;       /* background cap; 0 = none */
    uint32_t focused;              /* quiverd's view of the game's window */
    uint32_t reserved[1003];
};
```

The layer checks `seq` once per present (one atomic load, as for `hud.image`) and reports in
its status block: the `seq` it applied, the limiter state (`off`, `limiting`, `standing down:
app sleep mode with interval 6250 µs`, `standing down: no swapchain`), sleeps and overshoot
counters, the present mode in use and whether it was forced, and what it saw the app do with
`VK_NV_low_latency2` (enabled, sleep mode set, markers seen). That status is the evidence for
every layer tweak and for "the game has Reflex" in general.

The layer needs the session dir, so it runs when the HUD **or** any layer tweak is on (the
same `QUIVER_SIGHT=1`, the same layer order). With both off it isn't loaded at all, as today.

**Layer order gains one entry.** NVIDIA's Smooth Motion is itself an implicit Vulkan layer
(`VK_LAYER_NV_present`, enabled by `NVPRESENT_ENABLE_SMOOTH_MOTION=1`) that generates frames
at present. Like lsfg-vk it must sit above Sight, or the HUD gets interpolated. `ABOVE_SIGHT`
in `crates/quiverd/src/perf/launch.rs` lists only vkBasalt and lsfg-vk today; it should gain
`("VK_LAYER_NV_present", "NVPRESENT_ENABLE_SMOOTH_MOTION")` whether or not Tweaks is built
(a person can set the variable by hand today).

---

## 6. Detection: did it take effect?

Options already distinguishes *saved* from *applied live*. A tweak needs one more question
answered, after the launch: **did the program use it?** An env variable the game never reads,
a DLSS preset override in a game that never creates a DLSS feature, or a 160 fps cap in a game
that runs at 90 all look identical from the launcher. So every active tweak gets an **effect**
per session:

| Effect | Meaning | Example |
|---|---|---|
| `applied` | written to its mechanism; nothing observed yet | env set, game still loading |
| `confirmed { evidence, at }` | the program showed it used it | "NGX read the SR preset override (dxvk-nvapi, 21:04:12)"; "DXVK engaged its frame rate limiter"; "Sight layer limiting at 160 fps" |
| `measured { value }` | for caps: the present ring agrees | "160.0 fps mean, p50 6.25 ms"; or "not limiting: the game runs below 160" |
| `unconfirmed { why }` | applied, no evidence by session end | "the game created no DLSS feature this session"; "no DXVK log: not a D3D9–11 game?" |
| `provided { by, value }` | a mod or game implements it (section 7) | "DLSS (aco-dlss): reflex.fps_limit = 160" |
| `superseded { by }` | the person's own launch setting won | "your DXVK_CONFIG sets dxgi.maxFrameRate" |
| `unavailable { reason }` | no binding's `requires` held | "NVIDIA only"; "needs dxvk-nvapi 0.9.3 (this Proton has 0.9.0)"; "off for anti-cheat games" |
| `failed { reason }` | the mechanism refused | "the layer can't force IMMEDIATE: the surface doesn't support it" |

**Evidence sources**

| Source | How Quiver reads it | What it proves |
|---|---|---|
| DXVK log | launches with a tweak on the `dxvk` mechanism set `DXVK_LOG_PATH` to the session's log dir (a Windows path, `Z:\…`, as the bridge log is passed today); quiverd tails `<exe>_dxgi.log`, `<exe>_d3d11.log` | `Found config file`, the options it applied, `Engaging frame rate limiter`, the present mode and HDR colour space it chose |
| dxvk-nvapi log | `DXVK_NVAPI_LOG_LEVEL=info`, `DXVK_NVAPI_LOG_PATH` (same dir) whenever an `nvapi-drs` tweak is on | which DRS settings came from the environment and which the application (NGX, Streamline) requested, and Reflex initialization (`VkNvLowLatency2 implementation`) |
| Sight layer status block | already mapped by quiverd | engine name (the game's API), present mode, present intervals, `VK_NV_low_latency2` use, the tweaks control `seq` it applied, limiter state |
| Present ring | already in quiverd | measured frame rate against a cap |
| NGX models folder | stat `drive_c/ProgramData/NVIDIA/NGX/models` in the prefix at session end | the updater ran and fetched something |
| Mod metrics | SIGHT-HUD's registry (`aco-dlss.upscaler`, `aco-dlss.reflex.mode`) | what a provider actually runs with |
| The DLSS indicator | the person's eyes | the DLSS version, preset and resolution NGX uses; the only direct evidence for games without a mod |

Logs are only parsed for fixed, known lines (no free-form parsing in the hot path; quiverd
reads them at a few Hz while the session runs and once at the end). A log dir costs nothing
when no tweak needs it. Wording rule, as in Options: the UI says exactly what the evidence
says ("NGX read the override") and never more ("DLSS is using preset K" only when a mod
metric or the indicator says so).

**In the HUD and metrics.** A quiverd-side producer `tweaks` declares, per active tweak, a
`text` metric `tweaks.<id>` ("160 · DXVK", "K · override read", "by DLSS mod") and a `state`
metric `tweaks.<id>.effect`. The Full preset gains a row of compact chips (`CAP 160`, `DLSS K`,
`LL on`, `VSYNC off`), dimmed while unconfirmed. The canonical roles stay with the producers
that know (SIGHT-HUD §3.4): Quiver never fills `upscaler` from an override it can't see; a
mod's `upscaler` metric does. Recordings (`quiver perf record`) store the tweak values and
effects in their header, so comparisons say "run A: cap 160 via DXVK; run B: cap 160 via
Reflex".

---

## 7. Mods and games that implement a tweak

aco-dlss shows every way a platform tweak and a mod can collide:

| Tweak | What aco-dlss does | What happens if Quiver applies its own mechanism too |
|---|---|---|
| Max frame rate | `reflex.fps_limit` through Reflex's minimum interval or a CPU wait, plus `fg.auto_cap` while FG runs | DXVK's limiter caps **presented** frames, and with FG on the mod presents two per game frame: a 160 cap halves the base rate to 80 and fights the mod's pacing. Two caps either way |
| Low latency | injects Reflex (`reflex.mode`) | DXVK's `latencySleep` or a layer sleep would be a second sleep in the same frame |
| DLSS SR model | `upscaler.preset` (asks NGX for M, L or K) | the driver-settings override **wins inside NGX**: the game would run preset K while the mod's own `upscaler` metric says M |
| DLSS render scale | `upscaler.render_scale` writes the game's `PixelDensity` | `NGX_DLSS_SR_MODE`/scaling ratio would disagree with the resolution the game renders at |
| Multi-frame generation | its present thread expects exactly one generated frame per game frame | `NGX_DLSSG_MULTI_FRAME_COUNT=3` would break it |
| Texture LOD bias | `upscaler.mip_bias` biases the game's samplers | DXVK's `samplerLodBias` adds to it: double bias |

### 7.1 `provides` and `conflicts` in a schema

Any mod or game surface can declare them, in its `quiver-options.toml` (author or catalog):

```toml
# aco-dlss's quiver-options.toml (excerpt)

[provides."fps.cap"]
option = "reflex.fps_limit"        # the mod's option that implements the tweak
off = 0                            # its "not capping" value: lets the person prefer Quiver's cap
                                   # (numbers map one to one; Quiver resolves `vrr` to a number first)
[provides."latency.mode"]
option = "reflex.mode"
map = { off = "off", on = "on", boost = "boost", app = "@keep" }
off = "off"

[provides."dlss.sr.model"]
option = "upscaler.preset"
map = { app = "@keep", latest = "default", K = "K", L = "L", M = "M" }

[provides."dlss.sr.scale"]
option = "upscaler.render_scale"
map = { app = "@keep", dlaa = 1.0, quality = 0.667, balanced = 0.58, performance = 0.5,
        ultra_performance = 0.333, custom = "{percent / 100}" }

[provides."tex.lod_bias"]
option = "upscaler.mip_bias"
mode = "own"                       # the mod's value only; the tweak row is read-only here

[[conflicts]]
tweak = "dlss.fg.frames"
when = { "fg.mode" = "d3d12" }
reason = "DLSS mod generates exactly one frame per game frame; a multi-frame override breaks its present thread."

[[conflicts]]
mech = "layer:limiter"
when = { "fg.mode" = "d3d12" }
reason = "Frame generation presents two frames per game frame; a present-time cap would cap generated frames."

[option."fg.auto_cap"]
# ...
tweak_page = "frames"              # also shown on the Tweaks page, under "From DLSS"
```

| Field | Meaning |
|---|---|
| `[provides."<tweak>"] option` | the provider's option that implements the tweak |
| `map` | tweak value → option value; `@keep` = don't touch the option (the provider's own default or the person's value stands); `{…}` a tiny arithmetic on `percent`, `fps`, `v` only |
| `mode` | `bind` (default): the tweak's value becomes the option's value (7.2); `own`: the provider decides, the tweak row only shows its value |
| `off` | the option value that disables the provider's implementation; without it the person can't choose Quiver's mechanism instead |
| `when` | the provides or conflict holds only under these option values of the provider (aco-dlss's FG conflicts only with `fg.mode = d3d12`) |
| `[[conflicts]] tweak` / `mech` | a tweak, or one mechanism of a tweak (`dxvk:dxgi.maxFrameRate`, `layer:limiter`, `nvapi-drs:NGX_DLSSG_MULTI_FRAME_COUNT`), that must not apply while the provider is active, with the reason shown |
| `tweak_page` on an option | lists a provider's own option on a Tweaks page ("Automatic frame rate cap, from DLSS"), so mods can go beyond the tweak set where people look for it |

Lint checks that every `provides` names an existing option, that `map` values fit its type, and
that tweak ids exist in the platform catalog (a provides for an unknown tweak is a warning, so
mods can name tweaks a newer Quiver adds).

### 7.2 Resolution with a provider

For each tweak, at launch and on every live change:

1. Collect providers from the resolved stack (mod surfaces, then game surfaces) whose
   `provides` and `when` hold. Two providers for one tweak is a mod conflict, shown like
   MODS-V2's; the topmost wins.
2. **Who implements it** follows `prefer` (stored with the tweak at any scope, default
   `provider`): with `provider`, Quiver's mechanisms for that tweak stand down; with `quiver`,
   Quiver applies its mechanism and sets the provider's option to `off` (only offered when
   `off` exists). `[[conflicts]]` are always enforced: the conflicting mechanism doesn't
   apply, the next binding is tried, and the row says why.
3. **The value flows** (`mode = "bind"`): the tweak's resolved value, mapped, becomes a
   **default layer** in the provider option's resolution, between mod config patches and the
   profile's own value (MOD-CONFIG "Resolution"):

   ```
   option default < mod config patches < bound tweak (global/game/profile) < profile's option value < session
   ```

   So "cap 160 everywhere" reaches aco-dlss as `reflex.fps_limit = 160` with provenance
   "Quiver Tweaks (global)", and a profile that set `reflex.fps_limit = 120` in the DLSS form
   keeps 120, shown on the tweak row as "DLSS profile: 120 (set in DLSS options)". Live tweak
   changes reach the mod through its live channel (file rewrite today, SDK later), so they
   are as live as the mod's option is.
4. The effect is `provided { by, value }`, confirmed by the provider's metrics where it has
   them (aco-dlss's `reflex.mode` state and `upscaler` text).

The app and the CLI show both sides: the tweak row says "Controlled by DLSS (aco-dlss): 160 ·
Use Quiver's instead", and the mod's option row says "from Quiver Tweaks (global) ·
override here". One control, no double caps, and the person can still pick.

### 7.3 Beyond the tweak set

Mods keep everything Quiver doesn't generalise (FG pacing, auto cap, crowd density, debug
views) as their own options, schema-described, live where they can be. `tweak_page` puts the
relevant ones next to the tweaks. Through the SDK (phase 2), `welcome` also carries a read-only
`tweaks` snapshot (`{"fps.cap": 160, "latency.mode": "on", …}`, plus changes as `tweaks`
messages) so a mod can adapt to the person's platform choices for things it doesn't bind
(aco-dlss could pick its FG cap headroom from the display's VRR range). Games' own settings get
the same treatment from catalog game surfaces: a game with its own limiter in its ini can
`provide` `fps.cap` with `mode = "own"`, and Quiver stops stacking a second cap on it.

---

## 8. Surfaces: app, CLI, Sight, HUD

### 8.1 Daemon requests

Tweaks reuse `Request::Options(OptionsRequest)` with the surface `quiver/tweaks`; the
additions are appended (MOD-CONFIG C9: never renamed):

| Request / field | Notes |
|---|---|
| `scope: Option<Scope>` on `list`, `set`, `diff`, `history`, `restore` | `Global`, `Game { game }`, `Profile { game, profile }` (the existing addressing); only platform surfaces accept the first two |
| `OptionState.from` gains `scope(global \| game \| profile, rev, source)` and `provider(surface, option)` | where the effective value comes from |
| `OptionState.via` | the binding the resolver picked for the next launch, or why none: `{ mech, writes: ["DXVK_CONFIG: dxgi.maxFrameRate = 160"], unavailable? }` |
| `TweaksExplain { game, profile }` → `TweaksPlan` | the capability set, per tweak the bindings considered and why each was or wasn't chosen, the exact env and `DXVK_CONFIG` the next launch gets |
| `TweaksEffects { session }` → per tweak `Effect` | section 6 |
| `Event::TweaksEffect { session, id, effect }` | effects as they're confirmed |
| `OptionOp::Prefer { surface, id, prefer }` | `provider` or `quiver` (7.2) |

Launch plans (`LaunchPlan`, STORES §3.6; `SteamLaunchPlan` for `steam-run`) carry the compiled
tweaks as a `tweaks: { env, layer_ctl, wrappers, notes }` part, so both routes apply exactly
the same thing and `quiver tweaks plan` shows it.

### 8.2 CLI: `quiver tweaks …`

A front door over the same requests (`quiver mods <game> options … quiver` works too):

```
$ quiver tweaks origins --profile DLSS
Quiver Tweaks · AC Origins · profile DLSS · NVIDIA RTX 5070 Laptop (620.70) · GE-Proton11-12
  Frame rate and sync
    fps.cap            160 fps        global, r3          by DLSS mod (reflex.fps_limit)    live
    sync.vsync         off            this game, r1       DXVK dxgi.syncInterval = 0        next launch
    sync.max_latency   app            default
    latency.mode       on             global, r3          by DLSS mod (reflex.mode)         live
  NVIDIA DLSS
    dlss.sr.model      latest         global, r2          by DLSS mod (upscaler.preset = default)
    dlss.fg.frames     4x             global, r2          blocked: DLSS mod generates one frame per game frame
    dlss.runtime       updater        global, r2          unavailable: your driver package has no nvngx_update.exe
  Textures
    tex.anisotropy     16x            global, r1          DXVK d3d11.samplerAnisotropy = 16  next launch
  Last session (s-4kq2, 21:04–22:31): fps.cap provided (aco-dlss reported 160);
    tex.anisotropy unconfirmed (DXVK logs no line for it); sync.vsync confirmed
    (Sight layer: present mode IMMEDIATE)

$ quiver tweaks set --global fps.cap=vrr --reason "VRR panel, keep it engaged"
global r4: fps.cap 160 → refresh rate − 3 (162 fps on eDP-1 at 165 Hz)

$ quiver tweaks set farcry4 dlss.sr.model=K
r1 (Far Cry 4, this game): unavailable here: Far Cry 4 has no DLSS (saved anyway; applies if a mod adds it)

$ quiver tweaks prefer origins fps.cap quiver --profile DLSS
r2: fps.cap implemented by Quiver (sets DLSS reflex.fps_limit = 0); blocked while fg.mode = d3d12: DXVK's limiter would cap generated frames

$ quiver tweaks explain origins fps.cap        # bindings tried, capabilities, what gets written
$ quiver tweaks plan origins --profile DLSS    # the env / DXVK_CONFIG / layer control of the next launch
$ quiver tweaks status                         # running sessions: effects with evidence
$ quiver tweaks preset --global competitive
$ quiver tweaks history --global
$ quiver tweaks reset origins sync.vsync
```

`--json` everywhere, `--live-only` / `--no-live` as for options, source labels and approvals as
usual. Agents: game and profile tweaks are Allow (like options on profiles); **global** tweaks,
live changes to a running game, restores and `danger` tweaks are Ask (steer 12).

### 8.3 App

- **Settings → Tweaks**: the global scope, the generated form from the catalog, with the
  machine's capabilities in the header ("NVIDIA RTX 5070 Laptop · driver 620.70 · default
  Proton GE-Proton11-12") and an availability chip on every row ("NVIDIA", "D3D9–11", "Live
  with the Sight layer", "Needs lsfg-vk").
- **Game page → Tweaks** tab, with the profile cards the Mods tab uses picking the scope
  (This game / a profile). Rows show the inherited value and where it comes from, the
  mechanism, provider notices, and a **Last session** column with the effect:

```
┌ Tweaks · AC Origins ─────────── Scope: [This game | DLSS● | vanilla] ─── Running · DLSS ──┐
│ Frame rate and sync                                                                       │
│  Max frame rate      [ 160 ]  Global              Controlled by DLSS · Use Quiver's  ✓ 160│
│  VSync               [ App | On | Off● ]  This game  via DXVK         Next launch  ✓ DXVK │
│  Low latency         [ App | Off | On● | Boost ]  Global   by DLSS (Reflex)         ✓ on  │
│ NVIDIA DLSS                                         dxvk-nvapi 0.9.3 · NGX updater ✗      │
│  Super Resolution    [ Game's | Latest● | K | L | M ]  Global  by DLSS mod (default)      │
│  Multi frame gen     [ Game's | 2x | 3x | 4x● ]  Global   ⚠ blocked by DLSS (one frame)   │
│  DLSS version        [ Game's | NVIDIA updater● ]  ✗ driver package lacks nvngx_update   │
│ From DLSS                                                                                 │
│  Automatic frame rate cap   [■ On ]   (DLSS option)                                Live   │
├───────────────────────────────────────────────────────────────────────────────────────────┤
│ What this scope changes (4) · Explain next launch · History · Presets: Competitive ▾       │
└───────────────────────────────────────────────────────────────────────────────────────────┘
```

- The Options view of the Mods tab lists **Quiver Tweaks** as the first surface (it is one),
  and "What this profile changes" includes tweak values at the profile scope.
- Gamepad behaviour, staging bar, danger confirmation and live notices are Options' (§6 App).

### 8.4 Sight

- The panel's **Performance** section gets a **Tweaks** group: live tweaks editable (frame
  rate cap and background cap through the layer, MangoHud's limit when it runs (by rewriting its config file), any bound
  provider option that is live), launch tweaks shown with "Next launch". Changes follow C3/C4:
  outcomes shown, saved to the profile scope by default ("Save for this game" and "for all
  games" as a choice in the row's menu).
- **Quick toggles** (SIGHT §3.6): any tweak can be pinned; "Max frame rate" with a few stops
  (30 / 60 / VRR / off) is the obvious one for a laptop on battery.
- Toasts on effect problems at session start, rate-limited: "DLSS override not read: the game
  didn't create a DLSS feature" only when the person asked to be told (off by default; the
  app's Last session column is the normal place).

---

## 9. Legitimacy and anti-cheat

Tweaks follow STORES §7 and add these rules:

1. **Quiver ships and downloads no vendor binaries.** No `nvngx_dlss*.dll`, Streamline
   `sl.*.dll`, FSR/FidelityFX, XeSS, OptiScaler or `Lossless.dll`, from Quiver's own servers or
   anyone's mirror. Quiver configures what the person's driver, Proton build and games already
   contain.
2. **Newer DLSS only through NVIDIA's channel.** `dlss.runtime = updater` sets
   `PROTON_ENABLE_NGX_UPDATER=1`; the driver's NGX core then runs NVIDIA's
   `nvngx_update.exe`, which downloads NVIDIA-signed snippets from NVIDIA into the prefix's
   NGX models folder, exactly as on Windows. The updater binary comes from the person's own
   driver install: Quiver copies it into the prefix's `system32` like Proton Experimental does
   (steer 6), and says plainly when the driver package doesn't have it (this machine). NGX's
   signature check (`__NV_SIGNED_LOAD_CHECK`) is never touched.
3. **GE-Proton's upscaler downloads are not used.** `PROTON_DLSS_UPGRADE`, `PROTON_XESS_UPGRADE`,
   `PROTON_FSR4_UPGRADE`, `PROTON_STREAMLINE_UPGRADE` and `PROTON_USE_OPTISCALER` fetch vendor
   DLLs from `loathingkernel.github.io/proton-upscalers`, which redistributes them without being
   the vendor. Quiver never sets them; if the person's own launch settings do, `tweaks explain`
   and the Tweaks page say so ("your launch settings download DLSS DLLs from a third-party
   mirror"). Whether Quiver also sets `PROTONFIXES_DISABLE_UPSCALERS=1` by default is steer 5.
4. **lsfg-vk only with the person's own Lossless Scaling.** lsfg-vk v2 needs `lsfg-vk.dll` from a
   purchased copy of Lossless Scaling on Steam, switched to its `lsfg-vk` beta branch (v1 used
   `Lossless.dll`); Quiver finds it in the person's Steam library (or a path they pick), points
   lsfg-vk's config at it and never copies or moves it. Without it the tweak is unavailable with
   "needs Lossless Scaling (Steam, lsfg-vk branch)". lsfg-vk v2 itself is CC BY-NC-ND 4.0 (v1
   was MIT) and now hosted at lsfg-vk.dev: Quiver neither bundles nor modifies it, only writes
   its documented config ([lsfg-vk.dev](https://lsfg-vk.dev),
   [configuration](https://lsfg-vk.dev/docs/configuration/)).
5. **Third-party layers are the person's installs.** Quiver doesn't package lsfg-vk, vkBasalt,
   gamescope or MangoHud; it detects their manifests or binaries and configures them.
6. **The registry side effect of the DLSS indicator is undone.** `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS`
   writes the prefix's registry permanently; Quiver records that it turned the indicator on in
   a prefix and writes the off values on the next launch without it (GE already resets it to
   0 on every launch, Valve's Proton doesn't).

**Anti-cheat.** Mechanisms fall into exposure classes; the class decides what runs for a game
flagged in the anti-cheat data (STORES §3.11), and a catalog deny-list can switch individual
tweaks off per game for anything else:

| Class | Mechanisms | In the game process | Anti-cheat-flagged games |
|---|---|---|---|
| Configuration | `env`, `dxvk`, `vkd3d`, `nvapi-drs` | nothing new: components Proton loads anyway behave differently (the Windows equivalent is the NVIDIA App writing driver profile settings, which it does for anti-cheat games too) | **allowed** |
| NVIDIA snippet update | `ngx` updater | a newer NVIDIA-signed DLSS snippet, loaded by NVIDIA's NGX core, as on Windows | allowed; deny-list per game if a title objects |
| Linux layer | `layer` (Sight), `vk-layer` (lsfg-vk, vkBasalt, low_latency_layer), `mangohud` | Linux code in the process, outside the Windows address space | **off**, as for the Sight layer (SIGHT steer 13, SIGHT-HUD decision 16); the row says "off for anti-cheat games" |
| Outside the process | `wrapper` (gamescope) | none | allowed |
| Provider | `mod` | the mod | mods are refused for online anti-cheat titles anyway (STORES §3.11) |

Nothing in Tweaks hooks Windows APIs, injects a Windows DLL, patches game code or touches
anti-cheat runtimes.

---

## 10. Phasing, spikes, risks

### 10.1 Spikes

| # | Question | How | Effort |
|---|---|---|---|
| T1 | Does the DLSS SR preset override take effect under Proton with the game's own DLL, and does `PROTON_ENABLE_NGX_UPDATER` work with this driver if `nvngx_update.exe` is present? Is `nvngx_update.exe` in NVIDIA's `.run` for 620.x (and just not packaged by Arch)? | a DLSS game (or aco-dlss's dev container) with `DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE=on`, `…RENDER_PRESET_SELECTION=render_preset_k`, the indicator on, dxvk-nvapi log at info; extract the `.run` with `--extract-only` in the scratch dir | S |
| T2 | `DXVK_CONFIG` vs a `dxvk.conf` in the game folder: which wins per key? Does `dxgi.maxFrameRate` reach vkd3d-proton's `SetTargetFrameRate` for D3D12? | a D3D11 and a D3D12 test app, DXVK log | S |
| T3 | Present-time limiter in the layer vs DXVK's limiter: frame-time variance and input latency | aco-dlss's `tools/latency-bench` with FG off, both limiters at 120 | S–M |
| T4 | Forcing a present mode at swapchain creation under DXVK 3.1 (which switches modes with swapchain_maintenance1) | layer debug build, DXVK log, present ring | S |
| T5 | dxvk-nvapi's log lines as evidence: exact text for "requested by the application" per setting, and whether NGX queries happen at feature creation or once at init | log at info in T1 | S (with T1) |
| T6 | Which `DXVK_*`, `DXVK_NVAPI_*`, `PROTON_*` variables survive pressure-vessel on the `steam-run` route | a Steam game with `steam-run`, `env` dumped by a wrapper inside the container | S |
| T7 | `vkGetLatencyTimingsNV` from the layer: does it consume reports the app reads? | a Vulkan Reflex sample through dxvk-nvapi's vkreflex layer | M (v3) |
| T8 | lsfg-vk v2 with a session-dir `LSFGVK_CONFIG`: hot reload of `multiplier`, the `pacing`/`pacing_mode` key name the docs disagree on, how its advertised "auto-enable" decides to load, and its place above Sight | its docs and source; a test with the user's `lsfg-vk.dll` (ask first; they may not own Lossless Scaling) | S (v2) |
| T9 | AMD Anti-Lag 2 under Proton: does a D3D12 game with Anti-Lag 2 reach `VK_AMD_anti_lag` through vkd3d-proton with Mesa's layer enabled, and what must be exposed (AGS)? | needs an AMD machine; source reading first | S (reading) |

### 10.2 Phases

**Phase 1: launch-time tweaks, the engine, evidence (≈ 2–3 weeks).**
- Engine: `kind = "platform"`, scopes (`global`, `game`) and their stores, `via` bindings with
  `requires`/`map`/`confirm`, `provides`/`conflicts`/`tweak_page` in schemas, lint, the bound
  default layer in option resolution, `prefer` (≈ 5 days).
- Capability probe (GPU and driver via NVML/sysfs, Proton build probe cached per build, NGX
  pieces, route, anti-cheat, last-seen API from the layer's status) (≈ 2 days).
- Mechanisms `env`, `dxvk`, `vkd3d`, `nvapi-drs`, `ngx` (including copying the driver's
  `nvngx_update.exe` if steer 6 says so and the indicator's registry clean-up), compiled into
  the launch plan for `proton run` and `SteamLaunchPlan` (≈ 3 days).
- A session log dir for every launch with a tweak; DXVK and dxvk-nvapi log tailing; effects,
  `TweaksEffects`, `Event::TweaksEffect`; the `tweaks` metrics producer and the Full preset's
  chips (≈ 3 days).
- `options/quiver-tweaks.toml` with the v1 catalog (section 4) (≈ 1 day).
- CLI `quiver tweaks …`; app Settings → Tweaks and the game page's Tweaks tab (≈ 4 days).
- aco-dlss adds its `provides`/`conflicts` block to `quiver-options.toml` (in its repo and the
  catalog copy; ≈ half a day).

**Phase 2: live tweaks through the Sight layer, third-party tools (≈ 2 weeks).**
- `tweaks.ctl`, the layer's limiter (foreground and background), present-mode override, status
  reporting; quiverd's focus-driven background cap; "layer runs when the HUD or a layer tweak
  is on".
- Sight panel Tweaks group and quick toggles; MangoHud's live `fps_limit` (config-file rewrite) when it runs.
- `vk-layer`: lsfg-vk v2 (with the person's `lsfg-vk.dll`), vkBasalt CAS and low_latency_layer
  (AMD/Intel), with generated configs and the layer order; `wrapper`: gamescope scaling and its own frame limit.
- SDK `tweaks` snapshot for mods.

**Phase 3: NVIDIA latency work and polish (research, ≈ 2–3 weeks if all of it lands).**
- Reflex-aware cap (`vkSetLatencySleepModeNV` interception), the Reflex latency readout for
  every Reflex game (`latency.frame` role), injected low latency for vkd3d-proton and native
  Vulkan games without Reflex (only if T3/T7-style measurements show a real gain).
- A CAS pass in the layer if vkBasalt proves awkward.
- Catalog deny-lists and game-surface `provides` for games with their own limiters or Reflex
  toggles.

### 10.3 Risks

| Risk | Mitigation |
|---|---|
| Proton builds change variable names or behaviour (GE's frame-rate hack, DXVK dropping `DXVK_FRAME_RATE`) | Quiver writes the components' own variables, not Proton's conveniences; the build probe and `requires` versions; `tweaks explain` shows exactly what was written |
| A driver-settings override breaks a game (DLSS override in a title that misbehaves with a newer preset; MFG in a game that can't pace it) | effects and crash correlation (a crash within 60 s of launch with a new tweak offers "launch without this tweak"); safe launch skips tweaks; per-game deny-list |
| Two pacers (layer limiter, DXVK, Reflex, a mod) | providers and conflicts; the layer's limiter stands down when the app's sleep mode carries an interval; one limiter per launch by construction |
| Layer limiter adds a stall in the present path | absolute-deadline sleep only, no locks, bounded; disabled on the first panic like the compositor; off unless asked for |
| Steam route drops variables in pressure-vessel | T6; effects say `unconfirmed` with the reason instead of claiming success |
| People expect NVIDIA App parity where Linux has none (Smooth Motion on older GPUs, RTX HDR, power management) | the catalog says "not on Linux" with a pointer to the closest thing, rather than a dead control |

---

## 11. Steers wanted

Each steer lists the options and a recommendation; none is decided yet.

| # | Decision | Options | Recommendation |
|---|---|---|---|
| 1 | **Engine** | (a) a built-in Options surface with `via` bindings and scopes; (b) a separate subsystem like HUD settings; (c) named presets of launch settings | **(a)**: one schema, form, history and outcome model; mods' `provides` lives in the same namespace |
| 2 | **Scopes** | (a) global → game → profile for the platform surface only; (b) per profile only, like mod options; (c) add global and game scopes to every surface | **(a)**: tweaks are "for all my games" settings by nature; mod options stay as MOD-CONFIG steer 5 decided |
| 3 | **Sleeping in the Sight layer's present** (reverses SIGHT §3.5's "the layer doesn't sleep") | (a) an opt-in limiter (live cap, background cap) that stands down when the app's Reflex sleep mode has an interval or a provider owns the cap; (b) never: launch-time DXVK/vkd3d caps plus MangoHud's live limit; (c) the layer is the default limiter for every game | **(a)** in phase 2 |
| 4 | **Default limiter for D3D games** | (a) DXVK's / vkd3d-proton's own limiter at launch; the layer only for live changes in a session and for native Vulkan; (b) the layer whenever it runs (one implementation, always live) | **(a)**, revisited after T3 measures both |
| 5 | **GE-Proton's own upscaler downloads** (from a third-party mirror) | (a) Quiver never sets those switches and warns when the person's launch settings do; (b) (a) plus `PROTONFIXES_DISABLE_UPSCALERS=1` on every Quiver launch unless the person sets it otherwise; (c) offer `PROTON_DLSS_UPGRADE` as an opt-in tweak with a warning | **(a)**; (b) if you'd rather Quiver launches never fetch from that mirror, even through protonfixes' per-game fixes |
| 6 | **NVIDIA's updater binary** (GE-Proton doesn't copy `nvngx_update.exe`; this driver package doesn't ship it) | (a) Quiver copies it from the driver's wine dir into the prefix when present (as Proton Experimental does), reports its absence otherwise; (b) leave it to Proton (works only with Experimental); (c) suggest Proton Experimental for games where the DLSS updater is on | **(a)**, plus T1 to learn whether the `.run` driver has it |
| 7 | **Who implements a tweak a mod provides** | (a) the provider by default; the person can prefer Quiver's mechanism where the provider declares an `off` value; conflicts always enforced; (b) Quiver always; (c) ask at first conflict | **(a)** |
| 8 | **How a bound value flows** | (a) the tweak's value becomes a default layer in the provider option's resolution (a profile's own mod value wins); (b) one stored value shared by the tweak and the option; (c) no flow, two separate controls | **(a)**: global choices reach mods, explicit per-mod choices stay |
| 9 | **Anti-cheat games** | (a) configuration tweaks everywhere, layer and third-party-layer tweaks off, per-game deny-list; (b) no tweaks for flagged games; (c) everything everywhere | **(a)**: matches Sight's rule and what the NVIDIA App does with driver settings |
| 10 | **DLSS override reach** (the NVIDIA App limits DLSS Override to an allow-list of games) | (a) any game (dxvk-nvapi answers any application), with a catalog "known bad" list; (b) only games on a Quiver allow-list | **(a)** |
| 11 | **Third-party layers** (lsfg-vk, vkBasalt) | (a) phase 2, using the person's installs, lsfg-vk only with their own `lsfg-vk.dll`; (b) Quiver-managed downloads of those tools; (c) out of scope | **(a)** |
| 12 | **Agents** | (a) game and profile tweaks Allow; global tweaks, live changes, restores and `danger` Ask; (b) all Allow; (c) all Ask | **(a)**: a global change touches every game |
| 13 | **Where Tweaks live in the app** | (a) Settings → Tweaks (global) and a game-page Tweaks tab, also listed as the first surface in the Mods tab's Options view; (b) only inside the Options view; (c) inside Launch options | **(a)** |
| 14 | **Session dir and logs** | (a) every launch with a tweak gets a session log dir (and `tweaks.ctl` when a layer tweak is on), independent of the HUD; (b) only when the HUD is on (no evidence otherwise) | **(a)** |
| 15 | **Default values** | (a) every tweak defaults to "game's choice / off": Quiver changes nothing until asked; (b) ship opinionated defaults (16x AF, VRR cap) | **(a)**; presets cover the opinionated sets |

---

## 12. Corrections this research implies elsewhere (not made here)

| Where | What it says | What's true |
|---|---|---|
| SIGHT §3.5, "FPS limiter" row | "set `DXVK_FRAME_RATE` at launch / MangoHud `fps_limit` live via its control socket" | `DXVK_FRAME_RATE` was removed in DXVK 3.0 (GE rewrites it into `DXVK_CONFIG`, Valve's Proton doesn't); MangoHud's `control=` socket handles only `hud`, `logging` and `fcat`, and live limits go through its inotify-watched config file. This document's `fps.cap` replaces the row |
| `crates/quiverd/src/perf/launch.rs`, `ABOVE_SIGHT` | vkBasalt (`ENABLE_VKBASALT`) and lsfg-vk as `VK_LAYER_LS_frame_generation` (`ENABLE_LSFG`) | lsfg-vk v2's layer is `VK_LAYER_LSFGVK_frame_generation` and it no longer uses `ENABLE_LSFG` (profiles in its config decide; `DISABLE_LSFGVK` vetoes); NVIDIA Smooth Motion's `VK_LAYER_NV_present` (`NVPRESENT_ENABLE_SMOOTH_MOTION`) is missing. Both generate frames, so today a person using either gets an interpolated HUD |
| SIGHT §8, "vkBasalt, lsfg-vk" row | | add NVIDIA Smooth Motion and low_latency_layer to the coexistence list |

## Sources (tools)

DXVK `dxvk.conf` and history (<https://github.com/doitsujin/dxvk/blob/master/dxvk.conf>,
<https://raw.githubusercontent.com/doitsujin/dxvk/v2.7.1/dxvk.conf>,
<https://github.com/doitsujin/dxvk/releases/tag/v3.0>); vkd3d-proton README and `swapchain.c`
(<https://github.com/HansKristian-Work/vkd3d-proton>); dxvk-nvapi README, wiki and releases
(<https://github.com/jp7677/dxvk-nvapi>, <https://github.com/jp7677/dxvk-nvapi/wiki/Passing-driver-settings>,
<https://github.com/jp7677/dxvk-nvapi/releases/tag/v0.9.3>); NVIDIA NGX README
(<https://download.nvidia.com/XFree86/Linux-x86_64/550.67/README/ngx.html>) and the NGX updater
crash report (<https://forums.developer.nvidia.com/t/proton-enable-ngx-updater-1-causing-crashes-in-some-ue-games-on-2nd-run/301572>);
lsfg-vk (<https://lsfg-vk.dev/blog/release-v2.0.0/>, <https://lsfg-vk.dev/blog/important-changes-to-lsfg-vk/>,
<https://lsfg-vk.dev/docs/configuration/configuration-options/>,
<https://lsfg-vk.dev/docs/configuration/environment-variables/>); LatencyFleX
(<https://github.com/ishitatsuyuki/LatencyFleX>); low_latency_layer
(<https://github.com/Korthos-Software/low_latency_layer>); vkBasalt
(<https://github.com/DadSchoorse/vkBasalt>); gamescope (`src/main.cpp` help text, issue #1662,
<https://github.com/ValveSoftware/gamescope/issues/1662>); GE-Proton fullscreen FSR patch and
OptiScaler integration (<https://github.com/GloriousEggroll/proton-ge-custom>,
<https://github.com/GloriousEggroll/proton-ge-custom/releases/tag/GE-Proton11-1>); MangoHud
README, `control.cpp`, `notify.cpp` (<https://github.com/flightlessmango/MangoHud>); OptiScaler
(<https://github.com/optiscaler/OptiScaler>). Local facts (Proton scripts, DLL strings, driver
files) were read on this machine on 2026-11-17 and are marked as such in section 2.
