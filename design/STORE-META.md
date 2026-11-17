# Meta PC VR store as a Quiver store (scoping notes)

Status: scoping only (2026-11-16). Nothing here is built. This is one of five store scopes written
in parallel (STORE-STEAM, STORE-EPIC, STORE-GOG, STORE-EA, STORE-META); the shared store
interfaces are sketched briefly in section 5 and are settled in STORES.md.

The ask: integrate the Meta (Oculus/Rift) PC VR store "with all the same great capabilities we
built for Ubisoft Connect", using the user's Heroic fork as prior art.

Terms used below:

| Term | Meaning |
|---|---|
| **Meta stack** | Meta's Windows PC software ("Meta Horizon Link", formerly the Oculus app): `OVRServer_x64.exe`, the OAF (Oculus App Framework) service inside it, the Electron `Client.exe`, `OVRLibraryService`, `oculus-platform-runtime.exe` |
| **OAF** | the local service in `OculusAppFramework.dll` (loaded by OVRServer). Clients talk to it over a named pipe through Meta's native addon `OafIpc.node`, with JSON requests such as `/library/launch` |
| **Platform SDK** | the Oculus Platform SDK games link (`LibOVRPlatform64_1.dll`, loaded through `OVR_PlatformLoader` with signature checks): entitlement, user, achievements, cloud storage, IAP, asset files |
| **LibOVR** | the Oculus PC runtime API (`LibOVRRT64_1.dll`, "CAPI"), the pre-OpenXR rendering and tracking API |
| **OVRPlugin** | Meta's Unity/Unreal integration. Newer builds use an OpenXR backend but refuse non-Meta runtimes |
| **FRL token** | a Meta user access token, prefixed `FRL` |

---

## 1. Summary and recommendation

**Feasible, but two parts of it are on a different footing from Ubisoft.** Library, art, install,
verify and update are proven by the user's Heroic fork and can be built natively in Rust with
little risk. Running games is the hard part, for two independent reasons:

1. **Rendering.** Most of the catalogue targets LibOVR, which nothing on Linux implements. The
   only proven path is a Revive-derived LibOVR→OpenXR runtime under GE-Proton, as RiftLift ships
   (self-reported 14 of 17 titles working). The newer titles use OpenXR, often through OVRPlugin,
   which refuses non-Meta runtimes until a binary patch is applied.
2. **Licensing.** Games check ownership through the Platform SDK, which talks to Meta's local
   service. The fork's working answer is to run **Meta's own stack** in Wine, so Meta's code
   answers the entitlement check against Meta's backend. That needs two in-memory patches to
   Meta's signed `OculusAppFramework.dll`, a byte-patched copy of `OVRServer_x64.exe`, and a
   service shim. None of them touches entitlement logic, but they are modifications of Meta's
   software. A ubi-bridge-style "meta-bridge" that implements the service side of the Platform
   SDK's IPC is the clean architectural analogue, but nobody has done it and it is a large
   reverse-engineering job.

**Recommendation:**

- **Build Meta in Quiver in two tracks.**
  - **Track A, now: everything outside the game process.** This covers sign-in, library, art,
    the download queue, verify, repair, update, uninstall, exports, local play time, mod
    layers, and the CLI and agents. All of it is native in quiverd and follows the fork's proven
    manifest and segment downloader.
  - **Track B, gated by spikes: play.**
    - Licensing uses the genuine Meta stack in a Quiver-managed **runtime prefix**, launched
      through OAF `/library/launch` the way the fork does it.
    - Rendering uses a Revive-derived LibOVR runtime for LibOVR titles and Proton's
      `wineopenxr` for OpenXR titles, against the user's WiVRn or Monado.
    - Before any of this ships, spike S3 must find out what the fork's `OAF_LAUNCH_CHECK` patch
      bypasses.
- **Never stub entitlement.** Anything that answers `ovr_Entitlement_GetIsViewerEntitled` must
  have a real "yes" from Meta for this user and this app behind it. In practice today that means
  Meta's own code. See [The line](#the-line-for-metas-platform-service).
- **The VR HUD goes to hypxrhud.** hypxrhud is an OpenXR overlay session, not the flat Sight
  Vulkan layer. The metrics for it come from a new OpenXR-side producer.
- **Be honest in the UI about per-title status.** VR compatibility varies by title far more than
  Ubisoft compatibility does. Quiver should classify each title's VR API at install time (a cheap
  file scan) and show "OpenXR", "LibOVR via compatibility runtime" or "untested" from day one.

Effort: Track A is about M. Track B is L, with risk that only spikes can retire. The meta-bridge
research track is L+ with no guarantee.

---

## 2. What the Heroic fork established

Source: `/home/ajg/code/HeroicGamesLauncher`, branch `hypxr-meta-store`, commit `7e7d7883`
("wip: Meta store integration snapshot", 4,119 lines across 68 files). The earlier commit
`7c76a999` ("initial groundwork (metadata, images)") is upstream's sideload dialog work
(metadata and image steps), not Meta-specific. The prefix it built is at
`~/Games/Heroic/Prefixes/MetaHorizon`. I listed it and hashed binaries; I read no session or
database files.

### 2.1 Architecture of the fork

- **One shared Wine prefix** (`MetaHorizon/pfx`, GE-Proton with `wineopenxr`, preferring
  `GE-Proton10-32`). Meta Horizon Link is installed into it with Meta's official installer (the
  prefix has an `OculusSetup-DownloadCache`). All games install into one Meta library folder,
  `C:\MetaGames\{Software,Manifests,Downloads}`.
- **Meta's client is repurposed, not replaced.** `oculus-client/resources/app.asar` is set aside
  as `app.asar.meta-original` and replaced with Heroic's own `meta/oafHelper/main.js`. That
  script is an invisible Electron app that reuses Meta's Electron binary so it can `dlopen`
  Meta's native `OafIpc.node` and speak OAF over its named pipe. A hard-linked clone of the
  original client (`HeroicMetaAuthClient`) is kept for sign-in.
- **The helper writes OAF traffic as JSON lines** (`oaf.jsonl`), and Heroic parses them.
  Observed payload types: `PACKAGE_INFO`, `APP_TRACKER_UPDATE`, `LIBRARY_UPDATE`,
  `PROFILE_UPDATE`, `PACKAGE_LOCATIONS`, `NOTIFICATION`, `SCREEN_IS_ON_UPDATE`, `PLAIN_STRING`.
  The only request it sends is `/library/launch` with
  `{launch_details:{type:"NORMAL"}, mode:"VR", title:<canonical name>}`. "Launch accepted" is
  `RESPONSE` with value `OK`. "Platform ready" is an `APP_TRACKER_UPDATE` in which the tracker
  `oculus-platform-runtime` has `isIpcReady`.
- **The library is read from Meta's own cache.** The fork reads OAF's SQLite store
  (`AppData/Roaming/Oculus/sessions/_oaf/data.sqlite`, table `Objects(hashkey, typename,
  value)`). Types used: `User`, `Entitlement`, `Application`, `PCBinary`, `LocalMachine` and
  `OafOfflineData`. Values are a tagged binary encoding (field name, `01 01`, u64 length,
  bytes; references carry a type name). Because Wine's SQLite locks don't share with the host,
  the fork copies the database and WAL to a temporary directory before reading.

### 2.2 What works (proven by the fork, on the user's machine)

| Capability | How | Evidence |
|---|---|---|
| **Sign-in** | The cloned Meta client runs the browser login. Heroic registers the `oculus://` URL scheme and forwards the callback, which carries an "encrypted OAuth blob and request token" (the code is careful never to log it), to the clone. Heroic then polls the OAF DB until an offline token appears. | `user.ts`, `oaf.ts::forwardMetaLoginCallback` |
| **Library and art** | `Entitlement` rows → `Application` (`canonical_name`, `display_name`, `category`, `cover_landscape_image.uri`, `cover_square_image.uri`, `icon_image.uri`, descriptions) → `latest_supported_binary` → `PCBinary` (`size`, `required_space`). Only `category == GAMES` is kept. | `cachedLibrary.ts` |
| **Direct install, without Meta's installer** | (below) | `directInstall.ts` and its tests; six titles installed under `C:\MetaGames\Software` |
| **Verify and repair** | Re-running the install skips files whose size and SHA-256 already match | `installFile` |
| **Uninstall** | Removes `Software/<canonical>` and `Manifests/<canonical>.json`, with path-safety checks | `uninstall.ts` |
| **Launch with real licensing, for at least one title** | OAF `/library/launch`. Meta's platform runtime starts and becomes IPC-ready, so the game's Platform SDK talks to Meta's genuine service. | `oaf.ts::launchMetaGame`; the Walkabout Mini Golf flow (`launch-walkabout.jsonl` exists) |
| **OpenXR through Proton** | GE-Proton `wineopenxr` to the host runtime, after the explicit `vrclient_init_registry` preflight that Steam normally does (`Software\Wine\VR` `state == 1`) | `runtimePatch.c vr` |

**The install path is proven and simple:**

1. Read `PCBinary.manifest_uri` and append `?access_token=<FRL token>`. The response is a ZIP
   containing `manifest.json`.
2. The manifest is
   `{appId, canonicalName, launchFile, version, versionCode, files: {path: {sha256, size, segments: [[n, sha256, compressedSize]]}}}`.
   The first field of each segment entry is presumably an offset; the fork never uses it.
3. Each segment comes from
   `https://securecdn.oculus.com/binaries/segment/?access_token=…&binary_id=…&segment_sha256=…`
   with user agent `Oculus Client`. The body is a 2-byte zlib header followed by raw deflate.
   Segments are concatenated in order.
4. Check each file's size and SHA-256, write it to `.partial`, rename it into place, and
   finally write the manifest to `Manifests/`.

The fork runs six files at a time, with five retries and a linear backoff. The manifest's own
identity (`appId`, `canonicalName`) is checked against the request.

### 2.3 What is hacked, stubbed or missing

**Patches to Meta's software.** These are the important caveat.

- `runtimePatch.c` writes into the running `OVRServer` process.
  - At `OculusAppFramework.dll+0x315eec` it turns `jz +5` into `jmp +5`. It checks the
    signature first and refuses unknown builds. The comment calls this the "launch lifecycle
    check" and says the jump reaches the "existing success path". The commit does not say what
    condition it skips.
  - At `+0x48d7d0` it stubs the `Windows.Devices.WiFi` activation factory, which Wine lacks.
    Without the stub, OAF's observer tears down before the platform runtime can answer
    entitlement and profile calls.
  - Both offsets are pinned to one Meta build (hashes in `meta/oafHelper/README.md`).
- `OVRServer_x64.winefix.exe` is a byte-patched copy of `OVRServer_x64.exe`. Its hash differs
  from the README's known original. At file offset `0x159fe8`, 8 bytes become
  `xor r13d,r13d; jmp +0x1cc`, which skips a block. Its purpose is **not documented** in the
  commit (unverified).
- `libraryServiceShim.c` wraps `OVRLibraryService`. When OAF installs a `CORE_DATA` asset
  package, the shim copies the signed archive and `.sig` to `C:\MetaAssetCapture` and returns
  failure. OAF's installer uses Windows TxF (transactional NTFS), which Wine does not
  implement. Heroic would install the captured archive itself, but that step is not wired.
- `configureOaf.js` flips `allow_auto_updates=false` and `disable_core_updates=true` in the
  `LocalMachine` row. This stops OAF from trying to install every entitled game's asset pack at
  startup (TxF again) and stops Meta from updating itself past the pinned build.

**A patch to a game.** `compatibility.ts` flips one byte in Walkabout's `OVRPlugin.dll` (the
runtime capability bit) and two in `OculusXRPlugin.dll` (to enable the "non-Oculus runtime"
path), only for `versionCode == 780`. This is the OVRPlugin vendor lock (section 4.3). The fork
writes into the install folder and keeps `.heroic-original` backups.

**Not built in the fork:**

- Updates: `listUpdateableGames` returns `[]`, though `update()` = reinstall works.
- DLC and asset files: only captured, not installed.
- Cloud saves: "managed by Meta Horizon".
- Achievements, play time and per-game prefixes.
- Importing existing installs, and moving installs.
- Anything for LibOVR titles: there is no Revive in the prefix, so nothing in the fork can
  render a LibOVR game.
- A fresh-install bootstrap: the README says the prefix "is prepared already". A distributable
  build would need a versioned helper and a bootstrap step.
- `mojang-ab-minecraft-oculus-rift` is fulfilled by the Microsoft Store and is marked
  third-party.

**Hygiene the fork gets wrong.** Quiver must not copy these:

- It copies the token-bearing OAF database to `/tmp` on every read.
- It carries the FRL token in URL query strings. Meta's API requires that, but every logger
  must redact it.
- It keeps the token only in Meta's prefix database, not a keyring.

**Where the fork keeps credentials.** I located these from the code only and did not read them:

- The OAF DB: `pfx/drive_c/users/steamuser/AppData/Roaming/Oculus/sessions/_oaf/data.sqlite`,
  row `typename='OafOfflineData'`, field `last_valid_auth_token`.
- Heroic's `meta_store` electron-store, which holds only the user name and `isLoggedIn`.

### 2.4 What the installed titles say about the catalogue

I scanned binary strings in the six titles the fork installed (names only, no execution).

| Title | VR API | Platform SDK |
|---|---|---|
| Walkabout Mini Golf | OpenXR via **OVRPlugin** (vendor-locked; needs the patch) | not checked |
| BONELAB (Oculus build) | OpenXR via **Unity's built-in OpenXR** (`UnityOpenXR.dll`, `openxr_loader.dll`), so no vendor lock | not checked |
| Lone Echo (2017) | LibOVR, by era and engine (inferred) | yes (`pnsovr.dll` imports `ovr_PlatformInitializeWindows`, `ovr_Entitlement_GetIsViewerEntitled`) |
| Psychonauts: Rhombus of Ruin (2017) | LibOVR (inferred) | yes |
| Spider-Man: Far From Home VR (2019) | LibOVR (inferred) | yes |
| Minecraft (Rift) | n/a (Microsoft Store) | n/a |

My string search for the LibOVR runtime DLL name found nothing, since games build that name
with a format string. The "inferred" rows need confirming by import or behaviour (spike S4).
Every title I checked carries the Platform SDK entitlement call.

---

## 3. Capability matrix

The Ubisoft column is what Quiver does today (README). "Proven" means shown by the fork or by a
public project. "Spec." means speculative.

| Capability | Quiver for Ubisoft | Meta approach | Prior art | Effort | Risk |
|---|---|---|---|---|---|
| **Auth** | UbiServices login, 2FA, keyring, several accounts, "signed in elsewhere" | **MVP:** sign in through Meta's client in the runtime prefix, as the fork does: Quiver owns the `oculus://` handler and forwards the callback. Quiver then needs the FRL token for downloads (a keyring copy). **Later:** native sign-in (section 6) | fork (proven); RiftLift, up to 4 accounts (proven, method not documented) | M | Med: depends on Meta's client; native login is spec. |
| **Library and art** | demux ownership, product configs, art CDN | Entitlement → Application → PCBinary. **MVP** reads the OAF cache through a snapshot that stays in memory (never `/tmp`). **Later** queries Meta's GraphQL directly with the user's token. Art from `cover_landscape`, `cover_square` and `icon` URIs | fork (proven, cache); Oculus-PCVR-Linux, `ovrl library` via token (proven by its README) | S–M | Low (cache) / Med (GraphQL is undocumented) |
| **Install** | queue, slices, resume, space check, retries | Fork's manifest ZIP plus securecdn segments, in Quiver's queue. A segment is the resume unit. | fork (proven) | M | Low |
| **Verify and repair** | per-slice hash, repair only bad slices | Per-file SHA-256 plus per-segment SHA-256. Repair re-fetches only the bad segments, if the segment field is an offset (unverified; otherwise refetch the whole file) | fork (file level, proven) | S | Low |
| **Update** | manifest diff, reuse slices | Compare the installed `versionCode` with `latest_supported_binary`, then download changed files and reuse matching ones. Version pinning (`--build-id`) is possible: Oculus-PCVR-Linux and Oculus Downgrader fetch older builds | fork (reinstall), Oculus-PCVR-Linux (proven, per its README) | S–M | Low |
| **DLC and asset files** | owned and unowned add-ons, store links | Two kinds. **IAP add-ons** are entitlements and can be listed. **Asset files** (`*_assets.zip` plus `.sig`, and DLC packs) are downloaded by Meta's platform (`ovr_AssetFile_*`). Install the captured signed archive without TxF, or fetch it ourselves | fork captures (partial) | M | Med: Meta's install semantics and `.sig` handling are unknown |
| **Per-game prefixes** | one Proton prefix per game, reflinked | The Meta stack must run in the **same wineserver** as the game, because the Platform SDK reaches it over Windows IPC. Either clone a per-game prefix from a template runtime prefix (reflinks) or keep one shared runtime prefix. See steer 3 | fork (shared prefix only) | M | Med |
| **Launch with licensing** | ubi-bridge owns Connect's pipes; quiverd brokers Denuvo | Meta stack in-prefix, plus OAF `/library/launch`, plus platform runtime start. Meta's code checks ownership against Meta. Needs the patches in §2.3 | fork (proven for Walkabout) | L | **High**: binary patches pinned to one Meta build; the purpose of one patch is unknown; Meta updates must be frozen |
| **Rendering: OpenXR titles** | n/a | Proton `wineopenxr` to WiVRn or Monado, with the `vrclient_init_registry` preflight. OVRPlugin titles also need a per-build unlock patch, shipped as a fix mod | fork (proven, 1 title); LVRA guides | M | Med (per title) |
| **Rendering: LibOVR titles** | n/a | Revive-derived LibOVR→OpenXR/OpenVR runtime in the prefix (RiftLift's runtime, MIT, from Revive) | RiftLift (self-reported 14/17 on Linux and Windows); Oculus-PCVR-Linux (own shim to SteamVR, early) | L | **High**: per-title; no Quiver experience yet |
| **Cloud saves** | cloudsave_service V2, pull before and push after, save stores | With the Meta stack running, Meta's **Rift Cloud Storage v1** (deprecated in 2019, still used by older titles) syncs inside the platform runtime, if it works under Wine (unverified). Quiver can't sync it itself without that service. **Locally,** save stores, backups and merge work wherever the save path is known (a per-game table) | none | M | Med |
| **Achievements** | (Ubisoft: not wired yet) | Earned through the Platform SDK, handled by Meta's runtime. **Display:** read definitions and progress for the user from Meta (GraphQL or the OAF cache; unverified which types hold them) | none | M | Med (data source unverified) |
| **Play time** | Ubisoft playsession API plus local sessions | Local sessions (quiverd) only. I found no Meta play-time reporting API for PC (unverified). Meta's platform may record "recently played" on its own | n/a | S | Low |
| **Mods and Options** | overlayfs layers, profiles, mods v2, config patches, MOD-CONFIG | The same machinery: the install folder is plain files. Unity VR titles have MelonLoader/BepInEx scenes (BONELAB has a big one). The **OVRPlugin unlock is a built-in `fix` mod** with `builds = [manifest hash]`, mounted over the install, never written into it | Quiver mods v2 (built) | S | Low |
| **Sight panel and toasts** | layer-shell panel, toasts; hypxrhud toasts in XR (SIGHT v2) | In the headset the desktop doesn't exist (section 4.5). Toasts go to hypxrhud's `toast` slot. The full panel in VR is "later" | hypxrhud (built, D-Bus) | S (toasts) | Low |
| **HUD** | Sight Vulkan layer (game presenter) or layer-shell (desktop presenter) | **XR presenter:** hypxrhud panels, fed from quiverd's HUD model. Metrics come from an **XR producer** (an OpenXR API layer, the runtime, WiVRn stats), not `vkQueuePresentKHR` | hypxrhud; Monado overlay sessions | M | Med |
| **Exports** | .desktop, Steam shortcuts | The same. A Steam shortcut to a VR game is fine (RiftLift does it) | RiftLift | S | Low |
| **CLI and agents** | every action is a daemon command; agent policy | Same registry; `quiver install lone-echo` works the same. New VR read-outs (`quiver xr status`). Agents may install and play; sign-in is Deny for agents, as today | Quiver (built) | S | Low |
| **First-run UX** | sign in → import → folder → Proton → Steam | Adds an XR runtime check (WiVRn/Monado present, which is active) and a one-time **Meta runtime setup** (download Meta's installer, install it into the template prefix, pin it). Sign-in through Meta's page. Import from an existing Heroic or Meta prefix | fork (manual) | M | Med (Meta's installer under Wine, unverified from scratch) |
| **Multiple accounts** | several, merged | Several Meta accounts, merged (RiftLift does 4). But the Meta stack holds **one signed-in user per prefix**, so launch account means "which runtime prefix" | RiftLift | M | Med |
| **Import** | Lutris, Connect prefixes | Import the fork's `MetaGames` library: manifests in `Manifests/` make it a verified adopt | fork layout | S | Low |

---

## 4. VR runtime path

### 4.1 The user's runtimes

- **WiVRn** (`wivrn-hypxr`, 26.6.2, Monado-based) streams to a standalone headset. For a Meta
  store library this is the realistic headset path: a Quest over WiVRn. Meta's own Link isn't
  available on Linux.
- **Monado** (`monado-xreal`, for XREAL Air/Ultra glasses). These are glasses without tracked
  controllers, so most store titles won't be playable on them. They are fine for seated or
  gamepad titles, and for testing.
- Both runtimes are installed under `/usr/share/openxr/1/`. I found no `active_runtime.json` in
  `~/.config` or `/usr/share`. The HypXRland session scripts set `XR_RUNTIME_JSON` per session
  (`containers/session/session-launch.sh`). So Quiver should pass `XR_RUNTIME_JSON` explicitly
  per launch, defaulting to the one HypXRland is using.
- HypXRland is itself a Monado overlay client (monitor quads), with hypxrpaper as the primary
  session and hypxrhud as a second overlay. A VR game becomes the focused primary session, and
  the overlays keep compositing on top. Spike S7 decides whether Quiver should ask HypXRland to
  hide its quads while a game is focused (unverified).

### 4.2 How a Windows VR game reaches the runtime

- **Through Wine:** game (PE) → `wineopenxr.dll` (PE side) → `wineopenxr.so` → the Linux OpenXR
  loader → WiVRn or Monado. OpenVR games go through `vrclient_x64` → SteamVR, or xrizer or
  OpenComposite → OpenXR.
- **Outside Steam,** the Wine VR registry must be initialised first. Steam does this through
  `vrclient_init_registry`, and the fork does it explicitly.
- **GE-Proton10-33** added patches for VR outside Steam and a `umu.exe` wrapper that imitates
  `steam.exe`.
- **32-bit titles** need `PROTON_USE_WOW64` with recent Proton Experimental.
- **No container for Quiver:** Quiver runs `proton run` directly (`STEAM_COMPAT_DATA_PATH`, no
  pressure-vessel), so the runtime's IPC socket is reachable without the
  `PRESSURE_VESSEL_FILESYSTEMS_RW` trick the LVRA guide needs for umu.

### 4.3 Which APIs the catalogue uses

**LibOVR (most of the 2016–2021 back catalogue).** Meta deprecated its proprietary PC APIs in
favour of OpenXR (announced 2021, support ending August 2022). It says LibOVR "remains available
only for PC VR applications targeting Rift hardware", and all new PC projects should use OpenXR.
Older titles were not rebuilt. Revive is "a compatibility layer between the Oculus SDK and
OpenVR/OpenXR". It's Windows-only upstream and needs Meta's software installed for the platform
side.

- **RiftLift** (GPL-3.0, 2026) ships a Revive-derived runtime ("implements the Oculus ABI").
  Calls cross into GE-Proton's `wineopenxr.so` or `vrclient_x64.so` and on to Monado, WiVRn or
  SteamVR. Its compatibility list shows 14 of 17 titles fully working, with problems in
  ARKTIKA.1 and Brass Tactics and a crash in Edge of Nowhere.
- **Oculus-PCVR-Linux** replaces `LibOVRRT64_1.dll` with its own shim to OpenVR. It reports one
  title playable, and says it is "early, in active bring-up".

**OpenXR through OVRPlugin.** Meta's Unity and Unreal OpenXR integrations only work against
Meta's runtime. UploadVR tested it (March 2025), and a Khronos contributor called it "not an
accident". Meta then said it would recommend the engines' built-in OpenXR support. Titles built
this way need a per-build unlock patch, as the fork did for Walkabout (patch byte offsets pinned
to `versionCode 780`).

**OpenXR through the engines' built-in support** (BONELAB) has no vendor lock and should run
like any Proton OpenXR game.

**What fraction is OpenXR?** I found no catalogue-wide breakdown (unverified). A rough sketch:

- The store had roughly 900 titles at its peak (Road to VR, undated snapshot).
- Everything that shipped before mid-2021 and wasn't updated is LibOVR.
- Titles updated since then are mostly OpenXR, the majority of them through OVRPlugin.

In the user's six installed titles: 2 OpenXR (1 locked, 1 clean), 3 most likely LibOVR, and 1
not a Meta build. Rather than guess, Quiver should **classify every title at install** and keep
the result per title:

- OpenXR: imports `xrCreateInstance` or ships `openxr_loader.dll`.
- OVRPlugin: ships `OVRPlugin.dll`.
- LibOVR: imports `ovr_Initialize` from `LibOVRRT*` or ships `OVRPlugin` with the LibOVR backend.
- Platform SDK: uses `LibOVRPlatform64_1`.

That gives the real fraction over the user's library and drives the UI badge and the launch
plan.

### 4.4 Recommended runtime plan

| Title kind | Launch plan |
|---|---|
| OpenXR (clean) | Proton `wineopenxr` → `XR_RUNTIME_JSON` (WiVRn by default); Wine VR registry preflight |
| OpenXR (OVRPlugin-locked) | As above, plus the built-in fix mod for that build. Without a known fix: "Needs a compatibility fix for this build" |
| LibOVR | Revive-derived LibOVR runtime DLLs (vendored from RiftLift's `runtime/` or upstream Revive, MIT) placed by the launch plan, never in the install folder → `wineopenxr` → runtime |
| Any title using the Platform SDK | The Meta stack running in the same prefix (section 6 and steer 2) |

SteamVR, OpenComposite and xrizer matter only for titles that use OpenVR, which a Meta store
build rarely does. They also matter if the user prefers SteamVR as the runtime. RiftLift
supports both routes.

### 4.5 Overlay and HUD in VR (implications for SIGHT and SIGHT-HUD)

A VR game's frames never pass through the desktop.

- The swapchains it submits through `xrEndFrame` go to Monado's or WiVRn's compositor.
- Its `vkQueuePresentKHR` calls, if any, are only the **mirror window** on the monitor.

Consequences:

1. **The Sight Vulkan layer is the wrong producer and the wrong presenter for VR.** `fps.base`,
   frame time and present ids measured at `vkQueuePresentKHR` would describe the mirror window.
   A HUD drawn into that swapchain is invisible in the headset. SIGHT-HUD's `auto` presenter
   choice needs a third arm: **XR presenter** when the session is a VR session (from the launch
   plan's title classification or a runtime-reported session).
2. **XR presenter = hypxrhud.** hypxrhud already owns one OpenXR **overlay session**
   (`XR_EXTX_overlay`, which Monado and WiVRn composite above the focused app) with
   head-locked, slot-based quads, a D-Bus API (`io.github.andrewgaspar.hypxrhud`) and Omarchy
   theming.
   - quiverd's HUD model (already "values formatted, no pixels") maps onto hypxrhud's panel
     model: coloured text lines, bars and named gauges.
   - The GPUI-rendered HUD image is not needed here, unless hypxrhud gains an image panel type.
     That would be a hypxrhud change, out of Quiver's scope.
   - This also keeps the VR HUD out of the game process, which matters more in VR, where a
     dropped frame is felt.
3. **The XR metrics producer is new.** It has two candidate sources.
   - **(a) A Linux OpenXR API layer.** It would be loaded by the host loader that `wineopenxr.so`
     uses, so it would be outside the Windows process and could see `xrWaitFrame`,
     `xrBeginFrame` and `xrEndFrame` timing, `predictedDisplayPeriod`, `shouldRender` and the
     layer count. That gives app frame time and missed-frame counts. Whether implicit layers
     load through `wineopenxr` is unverified (spike S6).
   - **(b) The runtime itself.** Monado has a metrics output (unverified for WiVRn). WiVRn knows
     encode and network latency and headset battery. Getting these needs a small producer that
     reads them into quiverd's registry, ideally as a hypxr-side component.
   - Either source is a SIGHT-HUD **producer**. Neither changes the metrics model, but VR needs
     its own roles: `xr.frame.app`, `xr.frame.missed`, `xr.reprojected`, `xr.latency.motion`,
     `xr.headset.battery`.
4. **Screen share and capture.** In VR, "capture" means WiVRn or Monado recording, and the
   user's `wivrn-wt-hypxrrecorder` exists. Leave the "HUD hidden in captures" rule
   (SIGHT-HUD 2.12) to the runtime side. Deferred, like the flat HUD and screen-share
   separation.
5. **Input.** hyprpad doesn't see VR controllers, and an overlay session doesn't get input
   focus (unverified). So the summonable Sight **panel** in VR is "later". MVP is HUD plus
   toasts, toggled from the desktop, the CLI (`quiver hud toggle`) or a hypxr chord.

---

## 5. Architecture

Proposed Quiver-side layout, briefly. The siblings will propose the shared traits too, and they
should be reconciled into one `store` module.

```
quiverd
 ├─ store registry ── Ubisoft (ubi-core)  Steam  Epic  GOG  EA  Meta (meta-core)
 │     each store implements the store-agnostic traits below
 ├─ download queue / verify / repair (generic over FileManifest + ChunkSource)
 ├─ launcher: LaunchPlan = prefix + proton + env + sidecars + license bridge + xr
 └─ Sight / HUD: producers incl. new xr producer; presenters incl. XR presenter (hypxrhud)

meta-core (Rust, host)                     meta-runtime (Windows side, in the prefix)
  auth (token in keyring)                    Meta stack (Meta's own binaries, pinned)
  library (OAF cache → GraphQL later)        Quiver's OAF client (replaces the fork's Electron helper)
  manifest + securecdn segments              compat runtime (Revive-derived LibOVR, vendored, MIT)
  title classifier (XR API, Platform SDK)
```

**Store-agnostic interfaces Meta needs.** These are names only; the shapes come from reconciling
all five scopes.

- **`GameRef { store, id }`** replaces `product_id: u32` as the key in `games.json`, history,
  mods and saves paths. Meta ids are the canonical name (`ready-at-dawn-lone-echo`) plus a
  numeric app id.
- **`StoreAuth`**: accounts, sign in, sign out, refresh, "signed in elsewhere". A browser
  callback flow (custom URL scheme) is a variant next to Ubisoft's password and 2FA.
- **`Library`**: entitlements → titles with art URIs, kind (game, app, add-on) and third-party
  fulfilment. Merged across accounts with owners.
- **`ContentSource`**: `manifest(title, build) → FileManifest {path, size, hash, chunks[{hash,
  size, codec}]}` and `fetch(chunk)`. Ubisoft slices and Meta segments are both "hashed chunks
  of a file", so queue, resume, verify and repair become generic.
- **`LicenseBridge`**: what must run beside the game for the store SDK. Today ubi-bridge
  (Ubisoft); for Meta, the Meta stack plus the OAF launch. It contributes sidecars, env and
  prefix requirements to the `LaunchPlan`, and reports "ready" and "license refused" events.
- **`XrRequirement`** in `LaunchPlan`: needs-XR, the chosen runtime (`XR_RUNTIME_JSON`), the
  compat runtime (LibOVR shim) and the VR registry preflight. This is new and Meta-driven, but
  Steam VR titles would use it too.
- **`CloudSaves` and `Progress`** providers: optional per store. Meta's are thin, or "handled
  inside the LicenseBridge".

---

## 6. Auth

**Proven (fork):**

1. The browser login runs in Meta's own client.
2. Meta redirects to `oculus://…` with an encrypted blob and a request token.
3. The client exchanges them, and OAF stores `last_valid_auth_token` (FRL) in its offline data.

The fork needs the original client clone only for this. Everything else reads OAF's cache.

**Quiver MVP:**

- quiverd registers `oculus://` (an `x-scheme-handler/oculus` desktop entry, through
  `quiver open`) and forwards the callback to the Meta client in the runtime prefix. The UI
  shows "Sign in with Meta" and opens the system browser.
- Quiver needs its **own copy** of the token for downloads and library refresh. Two options:
  read it once from OAF after sign-in into the Secret Service keyring, or (better) obtain it
  through the native flow below. The keyring entry is per Meta account. The OAF read is allowed
  only for the session Quiver's own sign-in just created in Quiver's runtime prefix, never from
  the Heroic fork's or any other prefix (STORES.md section 7, item 8).
- Logs redact `access_token=` everywhere, because Meta's CDN takes the token in the query
  string.
- **Session expiry:** a 401 or 403 from securecdn or graph means "Sign in again", the same flow
  as Ubisoft's session-expired card. Quiver never retries a login on its own.

**Native sign-in (later, spec.):** perform the browser OAuth exchange in quiverd without Meta's
client.

- What the encrypted blob contains and how the client turns it into a token is undocumented. It
  needs a spike against the user's own login, with the client's traffic observed the way the
  Connect TLS tap was done.
- Oculus-PCVR-Linux shows that a token taken from the user's own browser session (the
  `oc_ac_at` cookie or a captured FRL token) works for library and download. Its README warns
  Meta may stop accepting the cookie, and treats the token as "a password for your account". A
  paste-a-token flow is a fallback for the CLI, not the product.
- Even with native sign-in, **launching still needs the Meta stack signed in** for the Platform
  SDK, unless the meta-bridge track succeeds.

**Accounts:** one Meta user per runtime prefix. Multi-account means one runtime prefix per
account, or re-signing the shared one (steer 6).

### The line for Meta's platform service

The ubi-bridge rule carries over: **Quiver may replace the vendor's launcher. It may not change
what the vendor's licensing decides, and every "you own this" must come from the vendor's backend
for the user's own account.**

| Allowed | Not allowed |
|---|---|
| Running Meta's own stack unmodified in Wine, so Meta's code makes the entitlement decision | Stubbing or forcing `ovr_Entitlement_GetIsViewerEntitled` (Oculus-PCVR-Linux's "entitlement (always passes)" stub, which its author calls a "ToS gray area") |
| Wine-compatibility workarounds that leave the license decision alone, such as stubbing the missing WiFi factory or working around missing TxF | Patching a branch whose job is a license, ownership, signature or trusted-caller check, in Meta's software or the game |
| Translating rendering APIs (a LibOVR→OpenXR runtime, like DXVK for D3D) | Replacing `LibOVRPlatform64_1.dll` with a DLL that defeats `OVR_PlatformLoader`'s signature verification |
| A **meta-bridge** that implements the service side of the Platform SDK's IPC while Meta's DLLs stay unmodified, and answers entitlement by asking Meta's backend with the user's token (the ubi-bridge pattern) | Any cached or offline "yes" that Meta itself wouldn't give, or sharing entitlement across accounts |
| Downloading builds the account is entitled to with the account's own token, including older builds | Downloading anything the account isn't entitled to |
| Unlocking an OVRPlugin runtime check so a game runs on a non-Meta OpenXR runtime (steer 4) | |

**History supports this line.** In 2016 Oculus added a Rift-hardware check to its entitlement
check, and Revive bypassed it. The bypass also cut out the ownership check. Oculus then
"removed the check for Rift hardware from the entitlement check", and Revive's developer
reverted the patch and pulled the binaries that contained it. Hardware and runtime checks have
been treated as fair game for compatibility. Ownership has not.

**Open issue for the line.** The fork's `OAF_LAUNCH_CHECK` patch at `+0x315eec` is described
only as a "launch lifecycle check". The fork's comments say elsewhere that OAF launches are
"trusted launch request[s]". Until spike S3 shows what that branch tests (most likely a Wine
incompatibility such as a missing WinRT or WiFi or device API, but unverified), Quiver should not
ship it. Two more open items:

- The byte patch in `OVRServer_x64.winefix.exe` is undocumented and needs the same review.
- Modifying Meta's binaries may breach Meta's terms even where no licensing is touched (I
  haven't verified Meta's terms). Steer 2 asks the user to accept that, or not.

---

## 7. Phased plan

**Phase 0: spikes.** No product code, and run only with the user's OK, since they touch the
user's Meta account and prefix.

| # | Spike | Answers |
|---|---|---|
| S1 | One OpenXR title (BONELAB) and one LibOVR title (Lone Echo) in a Quiver-style prefix against WiVRn. Uses RiftLift's runtime build for LibOVR. | Does the render path work outside RiftLift's launcher? Which DLLs, env and registry does it need? |
| S2 | Download one title with Quiver's queue using a token held in the keyring (a CLI prototype of the fork's downloader) | Segment field 0 semantics; 401 behaviour; whether segment-level repair works |
| S3 | **What `OAF_LAUNCH_CHECK` and the winefix patch skip.** Disassemble around the RVAs and log which condition fails under Wine without the patch | Whether the MVP launch path is within the line |
| S4 | Classify the user's whole Meta library (entitled, not just installed) by API | The real OpenXR fraction; how many titles are OVRPlugin-locked |
| S5 | The platform runtime under Wine: entitlement, achievements unlock, cloud storage v1, asset-file download for one title | Which Platform features work through the genuine stack |
| S6 | A Linux implicit OpenXR API layer loaded through `wineopenxr.so` | Whether the XR metrics producer can live there |
| S7 | A VR game focused with HypXRland quads and hypxrhud overlays up | Coexistence; HUD legibility |
| S8 | Meta's installer into a fresh template prefix, then pinning the build | Whether first-run is reproducible |

**Phase 1 (MVP): library and installs, play for what's proven.**

- **The store:** `GameRef` with `store = meta`; a Meta tile badge; per-title XR classification.
- **Sign-in:** through the runtime prefix, with the token in the keyring and "Sign in again".
- **Library and art:** from the OAF cache, read from an in-memory snapshot.
- **Downloads:** installs, updates, verify and repair in the shared queue, with space checks,
  pause and resume.
- **Install layout:** installs stay in a Meta library folder inside the runtime prefix, because
  OAF launches by canonical name from its library. Import the fork's existing `MetaGames`.
- **Play:** in the shared runtime prefix through OAF launch (only if S3 clears the patches),
  for OpenXR titles and OVRPlugin titles with a built-in fix. WiVRn is the default runtime.
- **Around play:** local play time; mod layers and profiles (overlayfs over
  `Software/<canonical>`); exports; CLI parity (`quiver install`, `play`, `verify` and
  `update` take Meta titles); hypxrhud toasts ("Downloaded", "Meta runtime ready").

**Phase 2: more titles, Quiver-shaped prefixes, the VR HUD.**

- LibOVR titles through the vendored compat runtime.
- Per-game prefixes cloned from the template runtime prefix (steer 3).
- XR presenter and XR producer for the HUD.
- Achievements display (source from S5).
- Save stores and backups for titles with known save paths.
- Asset-file and DLC installs.

**Phase 3: less Meta client.**

- Native sign-in and a GraphQL library, so the Meta stack runs only for a launch.
- A Rust OAF client instead of the Electron/`OafIpc.node` helper (needs the pipe framing; spec.).

**Research track (maybe never): meta-bridge.** Implement the Platform SDK's service side, in
the shape of ubi-bridge, with real ownership from Meta's backend. Only worth it if the Meta stack
proves too brittle under Wine. Effort L+; the protocol is unknown and may be authenticated.

---

## 8. Open questions and steers

1. **Is a Meta store worth Track B at all, or is Track A plus "play through RiftLift" enough?**
   - (a) Full Track B.
   - (b) Track A only. Quiver installs and manages files, and Play hands off to RiftLift or the
     Heroic fork.
   - (c) Defer Meta entirely.
   - *Recommend (a), gated on S1 and S3. Fall back to (b) if S3 finds a licensing-relevant
     patch.*
2. **Modifying Meta's own binaries for Wine compatibility.** The fork needs in-memory patches to
   `OculusAppFramework.dll` and a patched `OVRServer`.
   - (a) Accept Wine-compatibility patches that S3 shows are outside licensing. Apply them at
     runtime only; never redistribute Meta binaries.
   - (b) Refuse all modification of Meta software. Then Track B waits for upstream Wine fixes
     (WinRT WiFi, TxF) or the meta-bridge.
   - (c) Accept the fork's patches as they are.
   - *Recommend (a). (c) is out until S3 is done.*
3. **Prefix model.**
   - (a) One shared Meta runtime prefix for all Meta games, as the fork does.
   - (b) A per-game prefix reflinked from a pinned template runtime prefix (matches Quiver's
     model; costs a Meta sign-in state per prefix, unless the OAF session store can be shared).
   - (c) Per-game prefixes for files and settings, with the Meta stack in one shared prefix.
     Impossible, since Windows IPC doesn't cross wineservers.
   - *Recommend (a) for MVP and (b) in Phase 2, if a cloned prefix keeps the sign-in.*
4. **OVRPlugin runtime unlock patches.**
   - (a) Ship them as audited built-in `fix` mods, per build hash, mounted over the install
     (never written into it).
   - (b) Don't ship them; mark those titles "needs Meta runtime".
   - (c) A generic runtime-name spoof in an OpenXR API layer, so the runtime reports itself as
     Oculus. That's a broader deception of the game, but needs no per-build work.
   - *Recommend (a). It's within the line (not licensing), matches the fork and fits mods v2. (c)
     is worth a spike later, but it's less honest and could change game behaviour beyond the
     lock.*
5. **Where the LibOVR compat runtime comes from.**
   - (a) Vendor RiftLift's Revive-derived runtime (it's GPL-3.0 as part of RiftLift; its
     `runtime/` is MIT from Revive; check licensing).
   - (b) Build from upstream Revive and port the Wine boundary ourselves.
   - (c) Depend on an installed RiftLift.
   - *Recommend (a) if the MIT runtime is separable, otherwise (b).*
6. **Several Meta accounts.**
   - (a) MVP: one Meta account.
   - (b) Several, with one runtime prefix per account; launch account picks the prefix.
   - *Recommend (a) now and (b) with steer 3(b).*
7. **Default XR runtime.**
   - (a) WiVRn (Quest).
   - (b) Whatever HypXRland's session uses (`XR_RUNTIME_JSON`), with a per-game override as a
     launch setting `xr_runtime=`.
   - *Recommend (b), which in practice is WiVRn for VR titles.*
8. **VR HUD content in MVP.**
   - (a) hypxrhud toasts only.
   - (b) Toasts plus a minimal HUD panel (app frame time, missed frames, WiVRn latency, headset
     battery) from runtime-side metrics.
   - (c) Wait for the OpenXR API layer producer.
   - *Recommend (a) in Phase 1 and (b) in Phase 2.*
9. **Meta updates.** The patches are pinned to one Meta build, and Meta self-updates.
   - (a) Pin the Meta stack. Quiver owns updates: test a new build, then roll it forward.
   - (b) Let Meta update and fail closed when signatures don't match.
   - *Recommend (a). The fork already disables core updates.*
10. **Native sign-in spike.** May Quiver observe the user's own Meta client sign-in traffic once
    (a TLS tap as was done for Connect, with the user's OK) to learn the token exchange?
    - (a) Yes, in Phase 3.
    - (b) No; Meta's client always does sign-in.
    - *Recommend (a), later. It isn't needed for MVP.*

---

## Unverified claims (collected)

- What the `OAF_LAUNCH_CHECK` branch and the `OVRServer` winefix byte patch skip.
- Whether LibOVR titles in the user's library are LibOVR. Inferred from era, Platform SDK use
  and the absence of OpenXR; the runtime DLL name is built at runtime, so the string search
  missed it.
- The catalogue-wide OpenXR fraction (no source found).
- The fork's segment field 0 is an offset.
- RiftLift's handling of the Platform SDK. Its `PlatformCompat.h` redirects
  `LibOVRPlatformImpl64_1.dll` to `RIFTLIFT_PLATFORM_DLL`. Its README says "not a DRM bypass"
  and "No Meta PC app", but how it answers entitlement isn't documented, and I didn't read its
  source.
- Rift Cloud Storage v1, achievements and asset files working through Meta's platform runtime
  under Wine.
- Monado or WiVRn metrics sources; implicit OpenXR API layers loading through `wineopenxr.so`;
  overlay sessions receiving input.
- A Meta PC play-time API.
- Meta's terms of service on modifying its client.
- Meta's installer working in a fresh prefix.

## Sources

- User's Heroic fork: `/home/ajg/code/HeroicGamesLauncher` @ `7e7d7883` (`src/backend/storeManagers/meta/*`, `meta/oafHelper/*`)
- User's XR stack: `/home/ajg/code/hypxr-pkgs/pkgbuilds/{monado-xreal,wivrn-hypxr,hypxrhud}`, `/home/ajg/code/hypxrhud/README.md`
- [RiftLift (Villagers654)](https://github.com/Villagers654/RiftLift), [its runtime README](https://github.com/Villagers654/RiftLift/tree/main/runtime), [COMPATIBILITY.md](https://raw.githubusercontent.com/Villagers654/RiftLift/main/docs/COMPATIBILITY.md), [PlatformCompat.h](https://raw.githubusercontent.com/Villagers654/RiftLift/main/runtime/PlatformCompat.h)
- [Oculus-PCVR-Linux (Breezyslasher)](https://github.com/Breezyslasher/Oculus-PCVR-Linux)
- [LibreVR/Revive](https://github.com/LibreVR/Revive)
- [Meta: Oculus all in on OpenXR, deprecates proprietary APIs](https://developers.meta.com/horizon/blog/oculus-all-in-on-openxr-deprecates-proprietary-apis/); [Windows Central on the deprecation dates](https://www.windowscentral.com/oculus-killing-its-proprietary-pc-vr-apis-favor-openxr); [Meta: OpenXR vs VrApi/LibOVR](https://developers.meta.com/horizon/documentation/unity/os-openxr-vrapi/)
- [UploadVR: Meta's Unity/Unreal OpenXR SDKs block other PC VR headsets (2025-03-05)](https://uploadvr.com/metas-unity-unreal-openxr-sdks-block-other-pc-vr-headsets)
- [UploadVR: Oculus store accepts OpenXR](https://uploadvr.com/oculus-store-accepts-openxr/)
- [Meta: Entitlement check](https://developers.meta.com/horizon/documentation/native/ps-entitlement-check/); [Rift Platform SDK get started](https://developers.meta.com/horizon/documentation/native/pc/ps-get-started/); [ovr_PlatformInitializeWithAccessTokenEx](https://developers.meta.com/horizon/reference/platform/v74/function_o_v_r_platform_8h_ovr_platform_initialize_with_access_token_ex/)
- [Meta: Achievements](https://developers.meta.com/horizon/documentation/native/ps-achievements/); [UploadVR: Cloud Storage v2, Rift v1 deprecated (2019)](https://uploadvr.com/oculus-cloud-storage-2/)
- [Revive 2016 DRM episode: Tom's Hardware](https://www.tomshardware.com/news/oculus-blocks-revive-vr,31862.html), [GameSpot](https://gamespot.com/articles/oculus-has-stopped-blocking-the-vive-from-running-/1100-6441232/), [MobileSyrup](https://mobilesyrup.com/2016/06/27/oculus-removes-drm-blocking-rift-games-from-running-on-vive-through-revive/)
- [LVRA: VR games with Proton without Steam on Monado](https://lvra.gitlab.io/docs/games/vr-no-steam); [LVRA WiVRn](https://lvra.gitlab.io/docs/fossvr/wivrn); [GE-Proton 10-33 VR outside Steam (GamingOnLinux)](https://www.gamingonlinux.com/2026/03/ge-proton-10-33-brings-fixes-for-vr-outside-of-steam-fsr-upgrades-and-more/page=1/); [Proton 32-bit OpenXR (Mixed)](https://mixed-news.com/en/proton-experimental-early-32-bit-openxr-support-wow64-flag/)
- [Road to VR: Rift store catalogue size](https://roadtovr.com/top-20-oculus-rift-games-best-rated-most/)
- [BSManager: obtaining an Oculus token (token sensitivity)](https://github.com/Zagrios/bs-manager/wiki/How-to-obtain-your-Oculus-Token)
