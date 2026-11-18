# Sight: Quiver's in-game overlay (design notes)

Status: designed 2026-11-16; the HUD (SIGHT-HUD.md) and the panel MVP are built, see
[§14 As built](#14-as-built-the-panel-mvp-2026-11-17). Companion to `design/MOD-CONFIG.md`
(pluggable mod configuration, designed in parallel), which Sight consumes; see
[Assumptions about MOD-CONFIG](#10-assumptions-about-mod-config). The pinned performance HUD and
the metrics model are designed, with their steers decided, in [SIGHT-HUD.md](SIGHT-HUD.md); this
document's own steers (section 13) were decided 2026-11-16.

**Name.** In this codebase "overlay" already means the overlayfs mod mounts
(`crates/quiver-core/src/overlay.rs`, "mod overlays"). The in-game layer is called **Sight**: on a
bow, the sight is the thing in front of your eye while you aim, which is where this lives. Names
used below:

| Name | What it is |
|---|---|
| **Sight** | the feature as a whole: everything Quiver shows or does while a game is running |
| `quiver-sight` | the UI process (GPUI), one per desktop session, a quiverd client; also renders the pinned HUD (SIGHT-HUD §2.9) |
| **Sight panel** | the full, summoned UI (sections, forms, lists) |
| **toast lane** | short notifications while playing |
| **HUD** | the pinned, always-on performance readout over the game ([SIGHT-HUD.md](SIGHT-HUD.md)); the clock / session timer / battery strip once called the "corner HUD" is its `clock` layout |
| **Sight layer** | `VK_LAYER_QUIVER_sight`, a Vulkan layer loaded into the game process |
| **presenter** | a way Sight gets pixels on screen (compositor surface, Sight layer, XR HUD, desktop notification) |

Ubisoft's own overlay is called "the Connect overlay" here.

---

## 1. Problem and vision

Quiver replaces Ubisoft Connect, so while you play nothing answers the things Connect's overlay
answered (achievements, friends, "Shift+F2"), and nothing Quiver knows (which profile and mods
are running, whether cloud saves synced, that a download finished) reaches you without alt-tabbing.
Mods with in-game state, aco-dlss first (DLSS mode, frame generation on F9, an ini of tunables
that it re-reads), are tuned today by editing an ini and pressing function keys blind.

Vision: one summonable, gamepad-first panel and a quiet toast lane, both in the Omarchy theme,
that (1) tell you what this session is (game, account, profile, mod versions, play time, saves,
achievements), (2) let you change what can be changed live (mod settings through MOD-CONFIG,
performance HUD, quick toggles), (3) do what Connect's overlay did where it matters (achievements,
friends, invites, the overlay calls games make), and (4) stay out of the way: never cost
measurable frame time when hidden, never crash the game, never hold a Ubisoft token inside the
game process, and follow Quiver's principles (every action is a quiverd command with a source
label, `sight`; agents are peers).

---

## 2. Facts this design rests on

Found while preparing this (read-only), and the ones the recommendation depends on.

**How games reach Quiver today**

- quiverd launches start `quiver-bridge.exe` as the prefix's *first Wine process* and the game as a
  separate `proton run` (`launcher::play`); the bridge is a sibling of the game, not its parent.
  Only `quiver steam-run` uses `quiver-bridge.exe --run … -- game.exe`, where the bridge is the
  game's parent. Either way the bridge is a separate Windows process: **nothing Quiver-owned runs
  inside the game process today** (mods like aco-dlss do, via `d3d11.dll`).
- The bridge talks to quiverd's broker over TCP (`127.0.0.1:47810`, JSON lines,
  `BrokerRequest::{DenuvoGameToken, PlaySession}`); the play-session lease is a held TCP
  connection. This works from inside Steam's pressure-vessel container too.
- The launcher sets `QUIVER_PROFILE` on the game. The other markers in PROPOSAL ("Marking launched
  games": `QUIVER_SESSION`, `QUIVER_GAME_ID`, a systemd scope, Hyprland `tag:quiver-game`) are
  designed, not built. `ActiveSession` has no session id, pids or window addresses yet.

**What games ask of an overlay** (`proto/proto_uplaydll/UplayDll.proto`, `crates/quiver-bridge/src/win/api.rs`)

- `InitProcessRsp` and `InitRsp.overlay` say `enabled: false`, `injection_method: None`. Enum:
  `None=1, Default=2, SDK=3`.
- `OverlayReq` carries: `OverlayShowReq{section}` (sections: `Show, Home, Achievements, Actions,
  Chat, Friends, Party, Rewards, Shop, ProductActivation, PendingGameInvites, Challenges,
  GameOptions`), `OverlaySetShopUrlReq`, `OverlayShowShopUrlReq`, `OverlayShowBrowserUrlReq`,
  `OverlayShowNotificationReq{notificationId}`, `OverlayShowFriendInvitationReq{userIds}`,
  `OverlayShowFriendSelectionReq`, `OverlayShowMicroAppReq{name, params}`. The response side has
  `OverlayStateChangedPush{Showing|Hidden|Error}`: Connect tells the game when its overlay opens
  and closes (games use this to pause and release the cursor).
- Friends API also has UI calls: `ShowFriendSelectionUIReq`, `ShowInviteFriendsToGameUIReq`; and
  pushes for invites (`FriendsGameInvitePush`, `MultiplayerInviteReceivedPush`,
  `MultiplayerJoiningRequestedPush`).
- Today the bridge answers `OverlayShowReq` with `Unavailable` and drops every other overlay
  request without a reply (`Reply::None`), so a game that waits for an `OverlayShowBrowserUrlRsp`
  would wait forever. Friends: empty list. Achievements: `GetAchievements` returns none,
  `EarnAchievement` is logged ("not reported to Ubisoft yet") and answered OK.
- Existing bridge logs (AC Origins, Far Cry 4, AC Chronicles China, Rabbids Coding, aco-dlss
  instances) show **no `overlay_req` at all**, but they do show: `EarnAchievementReq` bursts at
  save load (Origins sends ids 45, 48, 49, 53, 59… in a burst in every logged run, apparently re-earning because the bridge says none
  are earned), `SetRichPresenceReq` with `presence_id` and tokens (`Quest=4000005`,
  `Territory=650282`), `ClubReq.RefreshActionsReq`, `InitFriendsReq`. So: achievement and
  presence events are already flowing through the bridge and can feed Sight now; overlay calls
  are rare in these titles and need a catalog across more games.

**How Connect's overlay works** (strings in the recon prefix's `overlay64.dll`, `uplay_r164.dll`,
and `proto/proto_overlay/overlay.proto`)

- `overlay64.dll` is an in-process hook library: D3D9/10/11/12 renderers, DXGI hooks,
  `InitializeVulkanHooks`, DirectInput8 detours, `XInputGetState`, raw input
  (`RegisterRawInputDevices`, `GetRawInputData`), window class/proc, cursor APIs
  (`GetCursorPos`, `ClipCursor`, `ShowCursor`), `CreateProcess` (to follow children),
  `ShellExecute`. Its pipe protocol (`overlay.proto`): `StartReq{width,height,pid}` →
  `StartSuccessRsp{shared_memory_name, shared_memory_mutex_name, …}`; the UI is rendered by
  Connect (CEF) into shared memory, the hook composites it and forwards `Win32KeyMessageReq`,
  mouse events and IME. Plus `CaptureScreenshotPush`, `VideoFrameReadyReq` (recording),
  `UpdateFpsReq`, and streaming.
- That is the hybrid this document recommends in shape: **UI in a separate process, a thin
  in-game compositor, input forwarded**. Ubisoft injects the hook (method `Default`) or the SDK
  loads it (`uplay_r164.dll` contains the string `-overlay64.dll` near `LoadLibraryW`; method
  `SDK` presumably means the game's SDK DLL loads the overlay module). Unverified; see spike S7.

**The motivating mod (aco-dlss, read-only)**

- AC Origins renders D3D11 → DXVK. With `[fg] mode=d3d12`, the mod creates its own vkd3d-proton
  D3D12 device (a second VkDevice in the process), and a present thread presents **two frames per
  game frame through the game's DXGI swapchain** (generated, then real, half an interval apart).
  MangoHud counts both. The game renders into a proxy backbuffer; the mod's `GetBuffer` and
  `Present` hooks own the swapchain; the D3D11 immediate context is shared between threads behind
  the mod's own gate (`ctxgate`).
- Consequences: anything that hooks DXGI `Present` in-process would fight the mod's proxy, its
  present thread and its context lock, and whatever it draws before DLSS-G would be interpolated.
  A Vulkan layer at `vkQueuePresentKHR` sits below all of that, sees every present (real and
  generated) and draws after generation.
- Reflex is injected by the mod (NVAPI → dxvk-nvapi → `VK_NV_low_latency2`); DXVK already logs
  frame-id mismatches for the FG presents. Anything in the present path must pass `pNext` chains
  through untouched and add no sleeps.
- Input: the game reads the mouse as `WM_INPUT` raw input on its message thread; it resolves
  user32 privately (Denuvo/VMProtect), so only Wine's own user32 → win32u imports are hookable.
  The mod's toggles are F9 (FG) and F10 (DLSS ↔ TAA); most ini settings are re-read live.

**The desktop**

- Hyprland (HypXRland fork, 0.56), Omarchy, NVIDIA RTX 5070 laptop, Wayland. Games run through
  XWayland unless `PROTON_ENABLE_WAYLAND=1` (opt-in in GE-Proton). Not gamescope by default
  (aco-dlss's dev loop uses headless gamescope in containers).
- hyprpad (`~/code/hyprsc`) owns the Guide button: it strips Guide from the virtual pad it feeds
  games, replays a bare Guide tap to Steam, runs guide-chords everywhere (including in games),
  has modes chosen by rules over the focused window **and over which layer-shell namespaces are
  on screen** (`ctx.layers:has(...)`), and has an on-screen keyboard (`hyprpad-osk`). PROPOSAL
  already decided Quiver never handles Guide and never grabs devices.
- hypxrhud (`~/code/hypxrhud`) is a D-Bus HUD daemon for the XR setup
  (`io.github.andrewgaspar.hypxrhud`, `CreatePanel{slot: "toast", …}`), with Omarchy theming.
- GPUI 0.2.2 (the app's version) has neither wlr-layer-shell nor offscreen rendering. Upstream
  Zed GPUI gained layer-shell windows (`WindowKind::LayerShell`, Zed PR #35610, after a reverted
  first attempt #32651); a version bump or git dependency is needed. Upstream has also split
  into crates, moved its Linux renderer from blade to wgpu, and has a headless wgpu renderer
  behind test/bench features. A spike (SIGHT-HUD §2.10) patched 0.2.2 to render a window
  off-screen and read it back in well under a millisecond; the HUD builds that onto the bump.
- Prior art: MangoHud and vkBasalt (implicit Vulkan layers, env-enabled, ImGui drawn in-layer;
  MangoHud has a `control=` Unix socket); Steam's overlay on Proton (a Vulkan layer
  `VK_LAYER_VALVE_steam_overlay` plus `gameoverlayrenderer.so` preloaded for X11 input hooks;
  GE-Proton 11.6 moved it to a vkroots implicit layer with a focus/input bridge in lsteamclient so
  it works under Wine-Wayland); gamescope (overlay windows marked with `GAMESCOPE_EXTERNAL_OVERLAY`,
  used by mangoapp and Steam); OBS vkcapture (a Vulkan layer that exports swapchain images as
  dma-bufs to another process: the reverse of what Sight's layer would import); discover-overlay
  (Discord overlay on Linux as a layer-shell client); wlx-overlay-s (desktop overlays in VR);
  ReShade's add-on API and Special K (in-process D3D hooks with plugin APIs); Xbox Game Bar
  widgets (a plugin model: third-party panels in a system overlay).

---

## 3. Capability inventory

Priority: **MVP** (first release), **v2**, **later**, **no** (probably not). "Source" is where
the data or the mechanism comes from. "Unwired" means Quiver doesn't have the data yet,
independent of Sight.

### 3.1 Shell

| Capability | Priority | Source / mechanism | Notes |
|---|---|---|---|
| Summon / dismiss panel with a keyboard chord | MVP | Hyprland bind → `quiver sight toggle` | default chord is a steer (#4) |
| Summon with the controller | MVP | hyprpad chord (e.g. `guide+start`) → `quiver sight toggle` | never the bare Guide (hyprpad/Steam own it) |
| Gamepad-first navigation in the panel | MVP | the app's `nav.rs` spatial focus + `gamepad.rs` (gilrs, read-only) | same A/B/X/Y/LB/RB semantics as the app |
| Mouse and keyboard in the panel | MVP | compositor input to the Sight surface | |
| Toast lane | MVP | quiverd events → `quiver-sight` | no focus change, click-through |
| Clock / session timer / battery strip | v2 | local clock, quiverd session, UPower | the HUD's `clock` layout (SIGHT-HUD §1.1); off by default |
| Per-game enable/disable, per-capability toggles | MVP | game settings key `sight=on|off|toasts` | Steam copies too |
| "Do not disturb while playing" | v2 | Sight queue policy; optional mako mode | steer #12 |
| Theme follows Omarchy, live | MVP | the app's `theme.rs` (shared crate) | |
| UI scale / TV distance mode | MVP | per-output scale + user factor | |
| Plugin panels (third-party widgets) | later | Game-Bar-like: a panel kind backed by a MOD-CONFIG schema or a small declarative view | mods first, see 3.6 |

### 3.2 Notifications (toasts)

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Achievement unlocked | MVP | bridge `EarnAchievementReq` → broker → quiverd; names and icons from the achievement set (`progress.rs`) | dedupe against known-earned: Origins re-earns its unlocked set at every save load |
| Achievement progress (n/m) | later | unwired (UbiServices stats) | |
| Session reported / play time not reported | MVP | `ActiveSession.reported` | only on failure, quietly |
| Cloud saves: pulled before launch, push result after exit, conflict | MVP (pull, conflict) | launch job events; push happens after exit, so it is a desktop notification | |
| Download/update finished or failed | MVP | `Event::Job` | |
| Friend came online / started playing | v2 | unwired (friends service) | rate-limited, favorites only by default |
| Game invite / party request | v2 | bridge pushes + friends service | actionable toast (Accept with a button) |
| Battery low / charger unplugged | MVP | UPower over D-Bus | laptop: relevant |
| Low disk on the install or prefix volume | MVP | statvfs from quiverd | |
| GPU thermal throttle / VRAM nearly full | v2 | NVML / hwmon | |
| Mod setting applied / needs restart | MVP | MOD-CONFIG apply result | |
| Agent message ("note from agent X") | MVP | new quiverd request, see 3.15 | actor-coloured like the app |
| Approval needed (agent asked for an "Ask" action) | v2 | existing approval flow | approve/deny in Sight |
| Signed in elsewhere | MVP | existing account event | important: play time stops being reported |
| Screenshot saved / clip saved | v2 | capture pipeline | |
| Desktop notification mirroring (other apps) | no | | that's mako's job; DND instead |

### 3.3 Session and progress

| Capability | Priority | Source | Notes |
|---|---|---|---|
| This session: game, account, Steam or Quiver copy, profile, mod stack with resolved versions, save store, cloud sync state, Proton build | MVP | quiverd session + launch plan (`PlaySession.mods`, `ResolvedMod`) | needs a session id linking the plan to the running game |
| Session timer, today's play time, total (Ubisoft's when wired) | MVP | `PlaySessions`, `Playtime`, `RemotePlaytime` | |
| Clock, battery | MVP | local | |
| Achievements: list, earned/locked, recently unlocked, hidden ones spoiler-masked | MVP (definitions + this-session earns) / v2 (account unlock state) | `progress.rs` definitions; unlock state unwired | reveal hidden one by one with A |
| Rich presence ("Territory: Siwa") | v2 | bridge `SetRichPresenceReq` ids + tokens; strings need the presence definitions | unwired: id → text mapping |
| Club challenges / actions, Connect level, units | v2 | `GameProgress` (exists for the app) | read-only |
| Stats (player stats) | later | `GameProgress` | |
| Session history (last sessions, outcomes) | later | `Playtime.sessions` | the app does this better |

### 3.4 Social (Ubisoft friends)

All unwired today: the bridge returns an empty friend list and quiverd has no friends service.
Sight can't be ahead of quiverd here; the panel sections ship when quiverd has the data.

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Friends list with presence, "playing this game" first | v2 | demux `friends_service` (`proto_friends`) + UbiServices presence | also feeds the bridge's `GetFriendListRsp`, so games' own friend UIs work |
| Invite friend to this game | v2 | `InviteFriendToGameReq` path / `MultiplayerInviteReq` | needs the game to have set a multiplayer session |
| Accept invite / join | v2 | `FriendsGameInviteAcceptReq`, `MultiplayerInviteAnswerReq`, `MultiplayerJoiningRequestedPush` | invite may be for another game: cross-boot (not supported yet) |
| Friend picker for games that ask (`ShowFriendSelectionUIReq`, `OverlayShowFriendSelectionReq`) | v2 | bridge → Sight → reply with selected ids | parity item |
| Party | later | `proto_party` | |
| Text chat | later | `proto_chat_message_data`, conversations cache | gamepad text entry is poor; read + quick replies first |
| Voice chat | no | | Discord exists |
| Friend requests, block list | later | friends service | the app first |

### 3.5 Performance

The pinned HUD, the metrics model (producers, descriptors, rings, subscriptions) and
mod-provided metrics are designed in [SIGHT-HUD.md](SIGHT-HUD.md), with its steers decided:
Quiver's own HUD in the MVP, drawn in the game by the Sight layer from an image `quiver-sight`
renders with GPUI, a layer-shell fallback, MangoHud optional per game. The rows below are what
the panel and the rest of Sight get from it.

| Capability | Priority | Source | Notes |
|---|---|---|---|
| FPS, frame time, 1% low in the panel | MVP | SIGHT-HUD metrics (`PerfSubscribe`), from the Sight layer's present ring | quiverd computes every statistic (SIGHT-HUD §3.10) |
| Real vs generated (base vs presented) frame rate when a mod does FG | MVP (mean) / v2 (lows, per-frame) | mod metrics filling the `frame.base` role (SIGHT-HUD §3.4) | the layer can't tell them apart; aco-dlss knows exactly |
| Frame-time graph in the panel | MVP | `PerfStream` from the same ring | |
| CPU, GPU load, VRAM, temps, power, clocks | MVP (GPU via NVML, CPU via /proc + hwmon) | quiverd's system producer (SIGHT-HUD §3.2) | one collector, also usable by the app |
| Always-on perf HUD | MVP | Quiver's HUD (SIGHT-HUD), presets, toggled with Shift+F3 | MangoHud stays a per-game option that Sight can toggle |
| FPS limiter | v2 | set `DXVK_FRAME_RATE` at launch (restart) / MangoHud `fps_limit` live via its control socket | Sight layer doesn't sleep in present: Reflex and FG pacing own that |
| Reflex / latency info | MVP (mean) / v2 (breakdown) | mod metrics filling `latency.frame` (SIGHT-HUD §3.4); `VK_NV_low_latency2` presence seen by the layer | display only |
| VRR / refresh rate / tearing / scan-out info | MVP | quiverd's display producer (Hyprland IPC) | the Full preset's display-path line |
| Perf recording for a session | v2 | SIGHT-HUD recordings (`quiver perf record`); a summary on every session | MangoHud-compatible CSV export |

### 3.6 Mods and configuration

| Capability | Priority | Source | Notes |
|---|---|---|---|
| "What's running": profile, each mod and version, config patches applied, save store | MVP | launch plan for this session | read-only list |
| Live mod configuration form | MVP | MOD-CONFIG schema + live apply | the headline feature |
| Restart-required settings: edit, mark pending, "applies next launch" | MVP | MOD-CONFIG apply classes | |
| Try-then-keep (session-only change, then "Keep" to persist into the profile) | v2 | MOD-CONFIG scope | steer via MOD-CONFIG |
| Mod actions (momentary buttons, e.g. "reset history") | v2 | MOD-CONFIG actions | |
| Mod status readouts (DLSS mode, internal res, FG on) | v2 | the mod's `state` / `text` metrics (SIGHT-HUD §3.13, formerly MOD-CONFIG readouts) | |
| Quick toggles (pin a few settings to a quick menu) | v2 | Sight preference over MOD-CONFIG keys | aco-dlss: FG, DLSS↔TAA |
| Switch profile / mods | no (in-game) | needs a relaunch | offer "Relaunch with profile X…" as an action instead (later) |
| Game launch settings (env, args) | no (in-game) | restart-only | link to the app |
| Game's own config files (graphics ini) | later | config patches, restart-only | |

### 3.7 Capture

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Screenshot (hotkey and panel button) | v2 | `grim -o <output>` / `grim -g <window geometry>` (compositor, includes Sight unless hidden first) or the Sight layer copying the swapchain image (game pixels only, exact, HDR-aware) | steer #11. The layer's copy is offered with and without the HUD (SIGHT-HUD §2.12) |
| Achievement auto-screenshot | v2 | achievement event → capture | delay 0.5 s, hide toast first |
| Gallery per game | v2 | `~/Pictures/Quiver/<game>/` | browse in Sight and the app |
| Instant replay ("save last 30 s") | v2 | `gpu-screen-recorder -r 30` (NVENC) per session, `SIGUSR1` to save | starts only if enabled; costs VRAM and some GPU |
| Record clip start/stop | v2 | same tool | |
| Clip trimming / sharing | later | | app-side |
| Recording that excludes Sight | later | layer-side capture | compositor capture includes Sight surfaces. For the HUD, the "HUD in captures" setting (SIGHT-HUD §2.12) already covers OBS game capture and portal screen capture; Sight's own clips with and without the HUD need the layer-side frames |

### 3.8 Tools: guides, browser, notes

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Notes per game (text, gamepad-editable) | v2 | quiverd store | "where I was" |
| Pinned images (maps, guides) | later | files | |
| Open a URL the game asks for (`OverlayShowBrowserUrlReq`) | MVP (link out) | xdg-open + QR code in the panel | no embedded browser |
| Embedded web browser | no (for now) | would need a browser engine (CEF/WebKitGTK) in Sight | big, security-heavy; steer #8 |
| Pinned windows (a desktop window kept on top of the game) | later | Hyprland `pin` + float rules, driven by Sight | e.g. a wiki in Firefox, toggled with the panel |
| PCGamingWiki / ProtonDB page for this game | later | link out | |

### 3.9 Controller and input

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Show the controller's state (battery, connected pads) | v2 | hyprpad `status.json` / gilrs | |
| Per-game button remap | later | hyprpad per-game modes (`quiver-game:<id>`) | Sight can open/edit hyprpad's mode; not its own remapper |
| Steam-Input-like features (gyro, radial menus) | no | hyprpad and Steam Input | |
| Text entry with gamepad | v2 | hyprpad OSK typing into the Sight surface | |

### 3.10 Audio and media

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Game volume / mute (per-app) | v2 | PipeWire (the game's stream node by pid) | the common ask |
| Output device switch | later | PipeWire | |
| Music/media controls | v2 | MPRIS (D-Bus) | Spotify etc. |
| Voice | no | | |

### 3.11 System

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Brightness | later | brightnessctl / Omarchy | laptop |
| Resolution, refresh, VRR, HDR toggles | later | `hyprctl keyword monitor` | risky mid-game; confirm with timeout like a display dialog |
| Power profile (performance / balanced) | v2 | power-profiles-daemon D-Bus | laptop on battery |
| Keep awake while playing | MVP (no UI) | idle inhibitor while a session runs | probably belongs to quiverd, not Sight |

### 3.12 Store, DLC, rewards

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Owned DLC / add-ons for this game | v2 | library add-ons (exists) | read-only |
| Rewards (Club rewards: view, redeem) | later | `GameProgress` rewards; redeem unwired | |
| Store browsing / purchase | no | | link out to ubisoft.com; never handle payment; steer #8 |
| `OverlayShowShopUrlReq` / `Shop` section | MVP (link out) | xdg-open + QR | |

### 3.13 Wellbeing

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Session reminders ("you've played 2 h") | v2 | session timer | opt-in |
| Time limits / parental controls | later | quiverd policy + a stop action | hard to enforce on a personal machine; low priority |

### 3.14 Accessibility

| Capability | Priority | Source | Notes |
|---|---|---|---|
| UI scale, larger text | MVP | Sight scale | |
| High-contrast variant of the theme | v2 | theme tokens | |
| Reduced motion | MVP | no animations flag | |
| Colorblind filters for the game image | later | Sight layer shader (or vkBasalt) | in-game drawing needed |
| Text to speech for toasts | later | speech-dispatcher | |
| Subtitles/captions | no | | game-side |

### 3.15 Agents

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Agent posts a toast or a card in the panel (`quiver sight notify --title … --body …`) | MVP | new quiverd request, agent policy "Allow", rate-limited | attributed with the agent's actor colour |
| Agent approval prompts in Sight | v2 | existing approval model | |
| Ask an agent from Sight ("why is my fps low?") | later | a text box (OSK) that posts to the user's configured agent endpoint; replies arrive as cards | needs an agent transport outside Sight's scope |
| Agent reads Sight state (what's on screen) | v2 | `quiver sight status --json` | read-only; performance data through `quiver perf` (SIGHT-HUD §3.12) |

### 3.16 Diagnostics and control

| Capability | Priority | Source | Notes |
|---|---|---|---|
| Quit game (graceful: WM_CLOSE) | MVP | quiverd `Stop` (extend with graceful-first) | confirm with hold-A |
| Force quit | MVP | quiverd `Stop` (kills the session's processes) | |
| Bridge / mod log tail | v2 | `bridge.log`, mod logs | dev aid |
| "Something's wrong" bundle (logs, plan, settings) | later | diagnostics export | |
| Crash notice after the game dies | MVP (desktop) | launch job exit code | not in-game: the game is gone |

### 3.17 Environments

| Capability | Priority | Notes |
|---|---|---|
| Multi-monitor: appear on the game's monitor | MVP | game window's monitor from Hyprland (`clients` by pid) |
| Steam copies (`steam-run`) | MVP | same Sight; Steam overlay coexists (section 8) |
| XR (HypXRland monitor quads) | v2 | compositor surfaces are part of the output the quad shows; toasts can also go to hypxrhud's `toast` slot |
| gamescope nested on Hyprland | MVP (works like any window) | |
| gamescope as the session compositor (no Hyprland) | later | needs the in-game presenter or a gamescope external-overlay window |
| Other compositors (GNOME has no layer-shell) | later | in-game presenter |
| Native Linux games / OpenGL games | later | Sight panel works (compositor); Sight layer only on Vulkan |

---

## 4. Architecture options and recommendation

### 4.1 Options

**(a) Vulkan layer on the Linux side** (MangoHud/vkBasalt style). A `.so` in the game process,
enabled by env, intercepting `vkQueuePresentKHR`. Under Proton every D3D9–12 game goes through
DXVK or vkd3d-proton and Windows Vulkan through winevulkan, so this covers effectively every
Ubisoft title. Draws after DXVK, after vkd3d, after a mod's frame generation. Must be careful
with ABI (pass `pNext` through, handle multiple devices and swapchains, recreate on resize,
survive `VK_ERROR_OUT_OF_DATE`), and loads inside pressure-vessel for Steam copies (built against
an old glibc, like MangoHud). Has no input of its own: the game window has focus; input would
need X11 hooks (Steam's old way), a compositor surface, or in-process Windows hooks.

**(b) In-process Windows-side hooks** (Connect's `overlay64.dll`, ReShade, Steam's
`GameOverlayRenderer64.dll`). DXGI/D3D Present hooks plus window-proc, raw input, XInput,
DirectInput and cursor hooks. Has the game's input in hand and can block it precisely. But:
it shares the D3D11 swapchain and immediate context with mods like aco-dlss (proxy backbuffer,
present thread, context lock); it must implement D3D9/10/11/12/Vulkan backends; an injected,
unsigned DLL is exactly what anti-cheat looks for; a bug crashes the game. Loading could ride the
SDK's own overlay-module path (`injection_method=SDK`, spike S7), which is tidier than
`WINEDLLOVERRIDES` proxies but no less visible to anti-cheat.

**(c) Compositor-side surface.** `quiver-sight` opens wlr-layer-shell surfaces on the
`overlay` layer of the game's monitor: a click-through toast surface (no keyboard
interactivity, empty input region) and, when summoned, a full-output panel surface with
exclusive keyboard interactivity. No injection, no game-process risk, normal Wayland input,
gamepad via hyprpad, GPUI reuse is total. Costs: the game loses keyboard focus while the panel is
open (pauses some games; may minimize a D3D "exclusive fullscreen" window: spike S1); direct
scanout is off while any Sight surface is visible; game-only captures (window capture, a Vulkan
capture layer) don't include it; doesn't exist when gamescope or a non-layer-shell compositor is
the session compositor.

**(d) gamescope-based.** Run games in gamescope and put Sight in a gamescope external-overlay
window (as mangoapp and Steam do). Works everywhere gamescope does and gives gamescope's input
focus juggling. But it makes gamescope mandatory for Sight, adds gamescope's latency and
compatibility cost to every game, and doesn't help the user's normal path (Hyprland directly).

**(e) Hybrids.** The real choice is which presenter draws which part, and which input source is
used. Pieces: one UI process; presenters = compositor surface, Sight layer compositing a shared
texture, hypxrhud, desktop notification; input = compositor focus, hyprpad, in-layer key polling,
in-process hooks.

### 4.2 Evaluation

| Criterion | (a) Vulkan layer | (b) in-process hooks | (c) compositor | (d) gamescope | Recommended hybrid |
|---|---|---|---|---|---|
| DXVK / vkd3d games | yes | yes, 4 backends | yes (any window) | yes | yes |
| aco-dlss FG frames | draws on real + generated, after FG | conflicts with the mod's Present/GetBuffer hooks | n/a (outside the game) | n/a | n/a for the panel; layer only measures |
| OpenGL / native Linux games | no (Vulkan only) | n/a | yes | yes | panel yes; telemetry Vulkan only |
| Anti-cheat risk | low to medium (MangoHud-class layers are generally tolerated; nothing in the Windows address space) | high | none | none | none for the panel; layer can be disabled per game |
| Performance when hidden | one hook per present, ~µs | hooks on every input call | zero | gamescope's own cost | ~µs (timestamping) |
| Performance when shown | one textured quad per present | same | compositor composition, direct scanout lost | composition | compositor |
| Input capture / blocking | none of its own | precise, per API | keyboard+mouse by focus; pad via hyprpad | gamescope focus | focus + hyprpad |
| Gamepad | external | XInput/DInput/HID hooks | hyprpad mode | gamescope | hyprpad mode |
| HiDPI | from swapchain extent; needs output scale from compositor | same | native (Wayland scale) | native | native |
| Frame pacing / Reflex | must not sleep; tiny GPU work in the present path | Present hook in the path | outside the path | gamescope paces | outside the path |
| In screenshots / recordings | yes (in the game's image) | yes | only output/region captures | yes | output captures include it; game-only captures don't |
| Crash isolation | layer bug = game crash; UI logic out of process limits it | game crash | full | full | full for UI; layer is tiny |
| UI authoring | ImGui/egui in-layer, or a shared texture from GPUI | same | GPUI directly, same components as the app | GPUI window | GPUI directly |
| XR quads | in the game image | same | in the output the quad shows (spike S11) | n/a | yes, plus hypxrhud toasts |
| Works without Hyprland | yes | yes | needs wlr-layer-shell | yes | later via (a) |

### 4.3 Recommendation

**Compositor-first, one UI process, a telemetry-only Vulkan layer, in-game drawing later as a
second presenter. No in-process Windows hooks.** (The HUD decisions in SIGHT-HUD change item 3
and narrow item 4: the layer composites the pinned HUD from the MVP. See steer 2.)

1. **`quiver-sight` (GPUI, separate process) owns all UI and UI state.** It is a quiverd client
   like the app, sharing the app's theme, components, focus navigation and gamepad code (move
   them into a shared crate, e.g. `quiver-ui`). It never talks to Ubisoft and holds no tokens.
2. **Presenter 1 (MVP): wlr-layer-shell surfaces on the game's monitor**, `overlay` layer
   (drawn above fullscreen windows on Hyprland): a click-through toast surface that never takes
   focus, and the panel surface that takes exclusive keyboard focus while open. Requires a GPUI
   version with layer-shell. The HUD uses a layer-shell surface only as its fallback
   (SIGHT-HUD §2.6), because any overlay-layer surface on the game's monitor costs direct
   scan-out and tearing, and its updates disturb VRR pacing.
3. **The Sight layer (MVP): `VK_LAYER_QUIVER_sight`, telemetry and the HUD compositor.**
   Timestamps every `vkQueuePresentKHR`, records swapchain extent/format/present mode and
   whether `VK_NV_low_latency2` is in use, and writes a present ring into the session dir that
   quiverd reads (SIGHT-HUD §3.6). Per SIGHT-HUD's decisions it also composites the pinned HUD:
   a premultiplied RGBA image `quiver-sight` renders off-screen with GPUI, from shared memory,
   plus a frame-time graph it draws itself (SIGHT-HUD §2.8). It draws no text and parses
   nothing. Later it also takes one command, "capture the next presented image" (screenshots of
   game pixels, with and without the HUD, v2). No sleeping and no allocations in the present
   path. Enabled per launch only (`VK_ADD_LAYER_PATH` + `VK_INSTANCE_LAYERS`, or an implicit
   layer gated by `QUIVER_SIGHT=1`), never globally for other apps.
4. **Presenter 2 (phase 3): the Sight layer composites textures** that `quiver-sight` renders
   offscreen and shares as dma-bufs (toasts first, panel last). For gamescope sessions,
   compositors without layer-shell, game-only recordings and cases where focus stealing is
   unacceptable. Only built if those cases matter. The HUD doesn't wait for it: its simpler
   path (CPU readback into shared memory, re-uploaded only when it changes) suits a small image
   that changes twice a second; presenter 2 could start from it before needing dma-bufs.
5. **Input: the compositor and hyprpad.** Keyboard/mouse via the panel surface's focus;
   gamepad via a hyprpad mode that activates when the `quiver-sight` layer namespace is on
   screen (the game gets a neutral pad, Quiver reads the pad read-only). Summon via a Hyprland
   bind and a hyprpad chord, both calling `quiver sight toggle`.
6. **The bridge becomes Sight's window into the game's SDK**: it forwards achievement earns,
   rich presence, overlay calls and invites to quiverd, and (after spike S7) tells games the
   overlay exists and pushes `OverlayStateChangedPush` when the panel opens and closes, so games
   pause and free the cursor as they do for Connect.

Why not layer-first: on the user's actual setup (Hyprland, XWayland or Wine-Wayland, no
gamescope) the compositor path already works for every game including Steam copies and native
ones, reuses GPUI wholesale, has zero crash or anti-cheat exposure, and needs no cross-process GPU
sharing. The layer is still worth having from day one, but for what only it can do: exact frame
timing, game-pixel capture, and (SIGHT-HUD) a pinned HUD that doesn't cost VRR pacing or
direct scan-out. Why not in-process hooks: the motivating mod owns the D3D11
present path, and an injected DLL is the only option with real ban risk. If spike S1 shows that
focus stealing breaks games, the fallback for input is the bridge's SDK-level
`OverlayStateChangedPush` (games release input themselves) before considering hooks.

---

## 5. Process and IPC model

```
 host (your session)                                             game's Wine prefix / container
 ───────────────────                                             ────────────────────────────────
 quiver-app ─┐                                                   quiver-bridge.exe (Windows, sibling
 quiver CLI ─┼── quiver-api (JSON lines, $XDG_RUNTIME_DIR/quiver/   or parent of the game)
 agents ─────┤    quiverd.sock)                                   │  SDK pipes ⇄ game's uplay_r1/r2
 quiver-sight┘        │                                           │
   │  │               │                     broker (TCP 127.0.0.1:47810, JSON lines)
   │  │  HUD model ◀──┤ quiverd ◀─────────────────────────────────┘  + new: SdkEvent, OverlayCall
   │  │  (SIGHT-HUD)  │ (sessions, plans, MOD-CONFIG live apply, perf registry and sensors,
   │  │               │  toasts queue, capture orchestration)
   │  │               ├── MOD-CONFIG live channel ──▶ mod inside the game (aco-dlss)
   │  │               │
   │  │               └── reads (mmap, read-only) ◀── session dir /dev/shm/quiver-<uid>/<s>/
   │  │                                                 layer-<pid>.ring, mod rings
   │  └── writes hud.image (GPUI off-screen) ──────▶    hud.image
   │                                                        ▲ ring        │ HUD image
   │                                                        │             ▼
   │                                      VK_LAYER_QUIVER_sight inside the game process
   │                                      (libquiver_sight_layer.so, host-side Linux code)
   ▼
 Wayland: layer-shell surfaces on the game's monitor (Hyprland): toasts, panel, HUD fallback
 hyprpad: sees the `quiver-sight` namespace → game mode with no forwarding
 hypxrhud: D-Bus CreatePanel(slot=toast) when XR is active (v2)
```

**Components**

- **quiverd** stays the source of truth. New responsibilities: a session id per running game
  (link `ActiveSession` ⇄ launch plan ⇄ bridge ⇄ Sight layer), the toast queue and its policy
  (priorities, dedupe, rate limits, DND), the perf registry with its sensor collector (NVML,
  hwmon, `/proc`, UPower, statvfs; usable by the app too), aggregation and formatting, and the
  HUD model it sends `quiver-sight` (SIGHT-HUD §2.7), capture orchestration (screenshot paths,
  the replay recorder process), and the routing of MOD-CONFIG live values. It creates the
  session dir and reads the layer's and mods' rings there.
- **quiver-sight** is started by quiverd when a session starts (or socket-activated by
  `quiver sight toggle`), runs as a normal user process (a systemd user unit next to quiverd),
  and exits a little after the last session ends. One instance per desktop session, serving all
  running games (normally one). Besides the panel and toasts it **renders the pinned HUD**: an
  off-screen GPUI window per session with the HUD on, written into the session dir's
  `hud.image` for the layer to composite, or a layer-shell surface when the HUD falls back to
  the desktop (SIGHT-HUD §2.9). While the HUD is enabled it runs for the whole session, even if
  Sight's panel and toasts are off for that game. Its Vulkan device costs about 100 MiB of RSS on
  the iGPU, which is one reason the HUD renders here and not in quiverd.
- **The Sight layer** knows only: its session id (from `QUIVER_SESSION` in the environment; for
  Steam copies `steam-run` sets it) and the session dir (`QUIVER_PERF_DIR`). Without them it
  stays inert (pure passthrough). It has no socket: it writes its present ring and status block
  into the session dir and reads `hud.image` from it (SIGHT-HUD §3.6–3.8); capture requests
  (v2) arrive the same way, as a command word it checks once per present. It contains no UI
  code and no text rendering.
- **quiver-bridge** gets new broker messages:
  - `SdkEvent { session, kind }` (fire-and-forget): `AchievementEarned { id }`,
    `RichPresence { presence_id, tokens }`, `GameSession` / `MultiplayerSession` changes,
    `InviteReceived`, and every `OverlayReq` for logging.
  - `OverlayCall { session, call }` (request/response): `Show { section }`,
    `ShowBrowserUrl { url }`, `ShowShopUrl`, `FriendSelection { allowed }`, … quiverd answers
    with the `OperationResult` the bridge returns to the game (`Ok` when Sight opened, else
    `Unavailable`), and for the friend picker the selected ids.
  - A push path from quiverd to the bridge (a held connection like the lease, or the lease
    connection itself becoming bidirectional) for `OverlayStateChanged` and invite answers. The
    bridge's API connection loop then needs a writer task so it can send pushes between replies.
- **Mods** reach quiverd through MOD-CONFIG's live channel, never through Sight directly.
  Sight edits values via quiverd (`source = "sight"`), so the activity log and the profile's
  history record "You · Sight" like any other actor.

**Context.** `quiver-sight` asks quiverd `SightContext { session }` → game (id, name, art),
account (display name only), copy (Quiver/Steam), profile and revision, resolved mods with
versions, save store and cloud state, Proton build, launch settings revision, play time, MOD-CONFIG
schemas and values for the active mods, and the game's window/monitor (from Hyprland, matched by
the session's pids). Updates arrive as events (`SessionsChanged`, `ModsChanged`, new
`SightEvent`s).

**Security.**

- Nothing in the game process (layer, bridge) receives account tokens beyond what the bridge
  already has (the per-game ticket in the launch context). The layer has none.
- The layer talks only through the session dir (`/dev/shm/quiver-<uid>/<session>/`, 0700,
  files 0600). quiverd maps the layer's ring read-only and validates every record; the layer
  maps `hud.image` read-only and validates every descriptor (SIGHT-HUD §3.16). The layer only
  ever sends timing data and receives rendered pixels; a malicious game gains nothing from the
  channel it couldn't do already.
- Sight's actions go through quiverd's normal permission model with source `sight`, which is
  treated as "you" (the user is physically present). Agent-posted cards are labelled with the
  agent and cannot trigger actions without the user pressing a button.
- URLs from games (`OverlayShowBrowserUrlReq`) are shown before opening and only `https` is
  opened.

---

## 6. Input model

**Summon.**

- Keyboard: a Hyprland bind calling `quiver sight toggle`. Global binds always fire, even over a
  fullscreen game, and need no in-game hook. Since a global bind steals the chord from every app,
  quiverd adds the bind only while a Quiver game window is focused and removes it otherwise
  (`hyprctl keyword bind` / `unbind` on `activewindow` events; it already plans to watch Hyprland
  events for window tagging). Default chord: steer #4. Connect's Shift+F2 is the parity choice;
  aco-dlss uses F9/F10, MangoHud Shift+F12 and Shift+F1 (its `toggle_fps_limit`), Steam
  Shift+Tab. The HUD's keys, bound the same way, are Shift+F3 (toggle) and Ctrl+Shift+F3 (next
  preset) (SIGHT-HUD §1.3); as built, the Sight layer reads those in the game instead (X11
  keymap polling), with Hyprland binds only as a printed fallback.
- Controller: a hyprpad binding, `h.bind("guide+start", "Quiver Sight", h.exec "quiver sight
  toggle")`, live in `game` mode. Quiver ships the snippet; the user owns their hyprpad config.
  Never bare Guide (hyprpad replays a bare tap to Steam) and never Start alone (games use it).
- Without hyprpad: Quiver can watch pads read-only (gilrs) for a long-press chord, e.g.
  Select+Start held for 0.6 s, but the game sees those buttons too. Acceptable as a fallback,
  off by default.
- The panel also opens from games (`OverlayShowReq`), from toasts ("View", "Accept"), from the
  CLI and from agents (agents may open it only to show their own card).

**While open.**

- Keyboard and mouse: the panel surface has exclusive keyboard interactivity and a full-output
  input region, so the game gets no keys or clicks. XWayland pointer locks and constraints are
  released when the game loses pointer focus; the cursor is drawn normally over Sight.
- Gamepad: hyprpad's mode rule `ctx.layers:has("quiver-sight")` switches to a mode with no
  forwarding, so the game's virtual pad goes neutral (hyprpad already does a clean handoff on
  mode change: held buttons released). Quiver reads the real pad via gilrs (read-only, as the
  app does). Without hyprpad the game keeps seeing the pad unless it ignores input while
  unfocused; Sight warns once.
- Focus side effects (spike S1): the game receives a focus-out. Many games pause or mute; some
  D3D fullscreen paths minimize on deactivation. If a game behaves badly, per-game options:
  "pause via SDK only" (keep focus, rely on `OverlayStateChangedPush`; works only if the game
  honours it) or "toasts only".
- Text entry: hyprpad's OSK (`h.keyboard{…}` on a guide chord, or Sight asks hyprpad to raise it
  when a text field gets focus) types into the Sight surface through the virtual keyboard.
  Fields that matter are short (notes, search, chat quick replies); prefer pickers and steppers.
- Dismiss: the summon chord again, B at the top level, Escape. Closing restores the game's focus
  (Hyprland `focuswindow` to the game's address) and the hyprpad mode follows automatically.

**Toasts never take input.** The toast surface has an empty input region. Actionable toasts
("Invite from X: Accept") show the chord that opens the panel on that item; they don't capture
the pad.

**Cursor.** When the panel opens, Sight warps the pointer to the panel's focused element only if
the last input was the mouse; for gamepad use the cursor is hidden over the Sight surface.

---

## 7. Ubisoft overlay parity

What Connect's overlay does (Shift+F2, configurable; can be disabled in Connect's settings):
home with the game's Club challenges and rewards, achievements, friends list and chat, party,
game invites, store pages for the game and DLC, product activation, notifications, screenshots.
Games call into it through `OverlayReq` and the friends UI calls, and listen for
`OverlayStateChangedPush`.

| Game call | Today (bridge) | Quiver with Sight |
|---|---|---|
| `InitRsp.overlay {enabled:false, method:None}` | as shown | after S7: `enabled:true, method:None` when Sight is available for this game, so games enable their "Ubisoft overlay" entry points without trying to load a DLL. Keep `false` if S7 shows the SDK loads `overlay64.dll` for any `enabled:true` |
| `OverlayShowReq{Show / Home}` | `Unavailable` | open the panel on Session; `Ok` |
| `…{Achievements}` | `Unavailable` | open Achievements; `Ok` |
| `…{Actions, Challenges, Rewards}` | `Unavailable` | open Progress (Club, read-only); `Ok` once that section exists (v2), else `Unavailable` |
| `…{Friends, Chat, Party, PendingGameInvites}` | `Unavailable` | Friends section (v2); `Unavailable` until then |
| `…{Shop}` | `Unavailable` | panel card with the store link (xdg-open, QR); `Ok` |
| `…{ProductActivation}` | `Unavailable` | `Unavailable` (activation is the app's job); log it |
| `…{GameOptions}` | `Unavailable` | open Mods and settings for this game; `Ok` |
| `OverlayShowBrowserUrlReq{url}` | no reply (game may wait) | link card; `Ok`. **Fix now regardless of Sight:** reply `Unavailable` instead of nothing |
| `OverlaySetShopUrlReq` | no reply | remember the URL for the Shop card; `Ok` |
| `OverlayShowShopUrlReq` | no reply (fire-and-forget) | link card |
| `OverlayShowNotificationReq{id}` | no reply | log the ids; `Unavailable` until their meaning is known. **Fix now:** reply |
| `OverlayShowFriendInvitationReq`, `OverlayShowFriendSelectionReq`, `ShowFriendSelectionUIReq`, `ShowInviteFriendsToGameUIReq` | no reply / unhandled | v2 friend picker; until then reply `Unavailable` / `ShowFriendSelectionUIEmptyList`. **Fix now:** reply |
| `OverlayShowMicroAppReq{name}` | no reply | log names (catalog); `Unavailable`. **Fix now:** reply |
| `OverlayStateChangedPush` (to the game) | never sent | sent `Showing` / `Hidden` when the panel opens/closes (after S7) |
| Invite pushes (`FriendsGameInvitePush`, `MultiplayerInviteReceivedPush`, …) | never sent | v2, from the friends service, with actionable toasts |
| Achievements `GetAchievementsReq` | empty | MVP for Sight's toasts: keep a local earned set per account+game (from earns) and answer with it, which also stops Origins re-earning at every load; replaced by UbiServices state when wired |

"Fix now" items are small bridge fixes independent of Sight: every request that has a response
message in the protocol should get one, so no game blocks on Quiver.

Not parity: Connect's overlay is a browser (CEF). Sight is native UI with link-outs; no store,
no embedded web pages (steer #8).

---

## 8. Coexistence

| Other thing | Interaction | Plan |
|---|---|---|
| **MangoHud** | Its own implicit layer and ImGui HUD in the game image; Shift+F12 / Shift+F1 hotkeys; `control=` socket | Quiver's HUD is the default (SIGHT-HUD); MangoHud stays a per-game option. Sight's Performance section offers "Show MangoHud" (needs it in the launch: `MANGOHUD=1` at launch, toggled live via its control socket or `no_display`) and presets. When Quiver launches MangoHud it moves MangoHud's `toggle_logging` off Shift_L+F2. The Sight layer's timing is independent; layer order doesn't matter for telemetry |
| **Steam overlay** (Steam copies) | Steam's Vulkan layer and preload inside the game; Shift+Tab; opened by hyprpad's guide tap | Both work: different hotkeys, Sight is outside the game. Don't summon Sight on Shift+Tab. If both are open, Sight is on top (compositor) |
| **Steam Input** | Steam reads hyprpad's virtual pad for Steam copies | Unchanged; hyprpad's mode switch neutralizes the pad for the game while Sight is open |
| **aco-dlss and similar mods** | Own `d3d11.dll` proxy, present thread, F9/F10 | Sight never touches D3D. Sight is the UI for their settings via MOD-CONFIG. Their hotkeys are declared to Sight (assumption M7) so Sight's chord avoids them and shows them in the session page |
| **ReShade** (via `dxgi.dll`/`d3d11.dll` proxy under Proton) | In-process, Home key | No conflict with Sight (no hooks). It may conflict with mods that proxy the same DLL; that's MODS-V2's conflict detection |
| **vkBasalt**, lsfg-vk | Implicit Vulkan layers post-processing or generating frames at present | Telemetry: no conflict. The HUD compositor (MVP) and any later in-game drawing must run *after* them (closer to the driver) or the HUD gets post-processed or interpolated: explicit layer ordering in `VK_INSTANCE_LAYERS` (SIGHT-HUD §2.4) |
| **OBS / gpu-screen-recorder / Omarchy screen recording** | Output captures include Sight surfaces; window captures don't. The in-layer HUD is part of the game's image, so both include it; obs-vkcapture (a Vulkan layer) sees it depending on layer order | Hide toasts on the recorded output optionally (v2 setting "hide Sight in recordings" = suppress toasts while a known recorder runs). For the HUD, the "HUD in captures: Shown / Hidden" setting (SIGHT-HUD §2.12): layer order for obs-vkcapture, and a switch to the layer-shell fallback with `no_screen_share` while Hyprland reports a screen share |
| **Discord** | No in-game overlay on Linux; discover-overlay uses layer-shell | Coexists. Later: map the game's rich presence to Discord RPC (Connect ships `discord-rpc`) |
| **Desktop notifications (mako)** | Appear over the game | Sight replaces Quiver's own desktop notifications while a game is focused, and optionally puts mako in a DND mode during play (steer #12) |
| **hyprpad OSK / cheatsheet** | Layer-shell surfaces too | They stack above Sight (hyprpad's namespaces); hyprpad rules decide which mode wins; Sight's namespace is `quiver-sight` |
| **gamescope** | Nested: a normal window; session compositor: no layer-shell | Nested works now; the in-layer HUD works in both; the panel and toasts as session compositor need phase 3 |
| **Connect itself** | Must not run (pipes) | Unchanged; the bridge's guards stay |
| **Anti-cheat titles** | Ubisoft titles with kernel/user anti-cheat mostly don't run on Proton anyway | Sight layer off for games flagged with anti-cheat (as built: EasyAntiCheat, BattlEye and other markers in the install folder, SIGHT-HUD §1.8; `quiver hud enable GAME` overrides); the compositor panel is safe everywhere (`quiver sight toggle`), and the HUD is to use its layer-shell fallback there (SIGHT-HUD decision 16; not wired yet) |

---

## 9. UX sketches

### Toast lane (top-right of the game's monitor, below any bar margin)

```
                                                    ┌──────────────────────────────────┐
                                                    │ ▣  Achievement unlocked          │
                                                    │    Mysteries of Egypt     12/62  │
                                                    └──────────────────────────────────┘
                                                    ┌──────────────────────────────────┐
                                                    │ ⟳  Cloud saves up to date        │
                                                    └──────────────────────────────────┘
```

At most three stacked, 4 s each (achievements 6 s), newest on top, fade only (reduced motion:
none). Same urgency levels as hypxrhud (info, success, warning, action). In XR the same toasts go
to hypxrhud's `toast` slot.

### Clock strip (the HUD's `clock` layout, optional, top-left, tiny)

```
 21:47 · 1 h 12 m · ▮▮▮▯ 64%
```

Drawn by the HUD like any other layout (SIGHT-HUD §1.1); the performance presets are sketched
there.

### Panel (full output, game dimmed 55% behind, safe-area margins for TVs)

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│  ◆ Assassin's Creed Origins          DLSS profile · You · Quiver copy      21:47  64% │
├──────────────┬─────────────────────────────────────────────────────────────────────────┤
│ ▸ Session    │  This session          1 h 12 m   ·   Today 1 h 40 m   ·   Total 212 h │
│   Mods       │  Profile   DLSS (rev 7)                                                │
│   Achievem.  │  Mods      aco-dlss 0.9.3 (current) · Intro skip 1 (current)           │
│   Perform.   │  Saves     Default · cloud ✓ synced 20:35                               │
│   Friends ⋯  │  Proton    GE-Proton11-12        Play time reported ✓                  │
│   Capture ⋯  │                                                                         │
│   Notes      │  [ Quit game ]   [ Force quit ]   [ Open in Quiver ]                     │
│              │                                                                         │
├──────────────┴─────────────────────────────────────────────────────────────────────────┤
│  Ⓐ Select   Ⓑ Back   LB/RB Section   Ⓨ Pin to quick menu         Shift+F2 / ⊞+Start   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

(`⋯` = section exists but needs data that isn't wired: shown greyed with "not available yet",
per PROPOSAL's "mark what is not wired".)

### Mods section (live configuration)

```
│ ▸ aco-dlss 0.9.3                                     ● live       F9 FG · F10 DLSS/TAA │
│   Upscaler     ◀ DLSS ▶                                                    live        │
│   Preset       ◀ M (DLSS 4.5) ▶                                            live        │
│   Render scale ━━━━━━●━━━━━━━  0.50  (Performance)                         restart ⟲   │
│   Frame gen    [■ on ]          real 79 fps · generated 79 fps             live        │
│   NPC density  ━━━━━━━━━●━━━━  1.6                                         next load   │
│   ── Status ─────────────────────────────────────────────────────────────────────────  │
│   Internal 1280×800 → 2560×1600 · Reflex on · DLSS 1.6 ms                              │
│                                                                                        │
│   2 changes not saved to the profile        [ Keep ]  [ Revert ]                       │
```

Rows show where the change takes effect (live / next load / restart). Restart-required edits are
held as pending and shown in the session page ("applies next launch").

### Achievements

```
│  12 / 62 earned          ◀ All · Earned · Locked · Recent ▶                            │
│  ▣ Mysteries of Egypt          Earned 21:31 (this session)                             │
│  ▢ Seasoned Hunter             Kill 10 hippos                                          │
│  ▩ Hidden achievement          Ⓐ Reveal                                                │
```

### Quick menu (hold the summon chord, or Ⓨ-pinned items)

```
            ┌───────────────────────────────┐
            │  FG on    DLSS↔TAA   MangoHud │
            │  Shot     Replay     Volume   │
            └───────────────────────────────┘
```

A compact grid on the toast surface's corner, not full-screen, still taking focus while shown.

**Scaling and themes.** Sizes in logical pixels from the output's Wayland scale, times a user
factor (default 1.0, "TV distance" 1.5). Tokens come from the shared theme (Omarchy live, or the
built-in palette); the scrim is `ink-0` at 55%. Focus ring and hint glyphs as in the app.
Panel width caps at 1400 logical px, centred, on ultra-wide outputs.

---

## 10. Assumptions about MOD-CONFIG

Sight consumes MOD-CONFIG; it doesn't define schemas or storage. What Sight needs from it:

- **M1. Discovery per session.** quiverd can list, for a running session, the active mods (with
  resolved versions) that have a config schema, plus the current effective values, in one
  request.
- **M2. Renderable schema.** Each setting has a stable key, label, help text, type and
  constraints that map to gamepad controls: bool (toggle), enum (stepper/list), integer and float
  with min/max/step and unit (slider/stepper), short string (OSK), key binding (capture), colour
  (later). Grouping/ordering and simple visibility conditions ("only when mode = d3d12") are
  declared, not coded in Sight.
- **M3. Apply class per setting.** At least: `live` (pushed to the running mod), `next-load`
  (the mod picks it up at a game event, e.g. area load), `restart` (needs a relaunch), and
  ideally `save-affecting` as a warning flag.
- **M4. Write path with result.** A quiverd request to set values with a source label returns,
  per key, `applied` (mod acknowledged), `pending-restart`, `rejected {message}` or `timeout`.
  Sight shows the truth from the mod's acknowledgement, not optimism.
- **M5. Scope.** Values are persisted to the profile (and versioned in its history) or applied
  for this session only; Sight's "Keep / Revert" maps to that.
- **M6. Change events.** When a mod changes a value itself (F9 pressed in game) or another
  client edits it, quiverd emits an event so Sight updates.
- **M7. Mod hotkeys declared.** Mods can declare the keys they use (aco-dlss: F9, F10), for
  conflict checks with Sight's chord and for the session page.
- **M8. Status fields (read-only).** Mods can publish live readouts (FG state, internal
  resolution, real vs generated fps, Reflex on). Sight polls or subscribes at ≤ 4 Hz while
  visible and not at all while hidden. *Now provided by SIGHT-HUD's metrics model*: readouts are
  `state` / `text` metrics, read with `PerfCatalog` / `PerfSubscribe` (SIGHT-HUD §3.13).
- **M9. Actions.** Momentary commands ("reset DLSS history") with a result.
- **M10. No Sight-specific code in mods.** Everything above goes through the MOD-CONFIG SDK; the
  same schema renders in the app's Mods tab and in Sight.

If MOD-CONFIG can't provide M4's acknowledgement or M6's events, Sight degrades to "sent" states
and refresh-on-open.

**Reconciled with MOD-CONFIG (2026-11-16).** MOD-CONFIG calls the feature *Options* and its
"Contract with the in-game overlay" (C1–C10) covers these: M1 → C1 + C2 (`ActiveSession`
gains `session`, `profile`, `options_revision`, `pids`; `OptionsList` returns renderable
`SurfaceState`), M2 → C2 + C8 (`overlay = false` hides settings Sight can't edit), M3 → `apply`
/ `at` on `OptionDef` (live, at a game event, restart, next launch), M4 → C3 per-op outcomes,
M5 → C4 (in-game edits saved to the profile, coalesced per burst; `live: Only` and
`OptionsRevertSession` for try/revert), M6 → C5 events (`OptionsLive`), M8 → SIGHT-HUD's
metrics (`PerfSubscribe`, which replaced `OptionsReadout`), M7 → C7 `OptionsKeys` (Sight
registers its chord, and the HUD its keys, as runtime surfaces), M9 → C6
`OptionsInvoke`, M10 → C9. One gap: MOD-CONFIG has `danger` (shown read-only in game unless
enabled) but no separate *save-affecting* flag; save compatibility stays a property of the mod
version (MODS-V2), so Sight should warn from that rather than per option. Sight is a separate
process, not inside the game, so it finds its session from `Request::Sessions` (focused game's
pids/window) rather than from `QUIVER_SESSION` in its own environment; only the Sight layer
reads the env.

---

## 11. Risks and spikes

| # | Spike | Question | De-risks |
|---|---|---|---|
| S1 | **Focus steal** | A layer-shell surface with exclusive keyboard over AC Origins (DXVK; fullscreen and borderless), FC4, a Steam copy (Black Flag Resynced), on Hyprland 0.56 with XWayland and with `PROTON_ENABLE_WAYLAND=1`: does the game pause, mute, minimize, lose its mode? Does the pointer lock come back on close? Does frame pacing/VRR misbehave while the panel is up? | (c) as the input model; per-game fallbacks |
| S2 | **GPUI layer-shell** | Bump GPUI (crates.io release or git) to one with `WindowKind::LayerShell`; transparent, click-through surface on the `overlay` layer of a chosen output; switch input region and keyboard interactivity at runtime; per-output scale. What else breaks in the app from the bump? The HUD needs the same bump for off-screen rendering (SIGHT-HUD H8), so it's required either way | the UI process, the HUD |
| S3 | **Telemetry layer** | A minimal Rust Vulkan layer (ash-based dispatch, or a C++ vkroots shim) enabled only for Quiver launches via `VK_ADD_LAYER_PATH` + `VK_INSTANCE_LAYERS`; works under DXVK and vkd3d-proton, with aco-dlss FG on (two VkDevices, two presents per frame) and Reflex on (no DXVK frame-id warnings added, no fps change); loads inside pressure-vessel for a Steam copy (glibc floor, path translation of the manifest). The HUD compositor builds on it (SIGHT-HUD H2) | perf section, screenshots, the HUD |
| S4 | **In-game compositing** (only if phase 3 is wanted) | Layer imports a dma-buf from another process (external memory + DRM modifiers + sync_file or timeline semaphore), blends it on real and generated frames on NVIDIA; cost per present; HDR swapchains | presenter 2 |
| S5 | **GPUI offscreen** | Size of a patch that renders a GPUI window into an exportable image instead of a surface (blade renderer). *Answered for CPU readback by the HUD spike* (SIGHT-HUD §2.10: +681/−26 in gpui 0.2.2, ~400 lines to port onto upstream's wgpu headless renderer, 0.2–0.7 ms per warm frame); exporting as a dma-buf for presenter 2 remains open | presenter 2 with GPUI |
| S6 | **Pad routing** | hyprpad rule on `ctx.layers:has("quiver-sight")` neutralizes the game's pad without input leaks at the switch; Quiver reads the pad via gilrs at the same time | gamepad model |
| S7 | **SDK overlay semantics** | In the recon prefix only, offline: answer `InitRsp.overlay` with `enabled:true` and methods `None`/`SDK`; trace (`WINEDEBUG=+loaddll`) whether `uplay_r164/r264` try to load `overlay64.dll` and through which pipe; whether games then call `OverlayShowReq`; whether pushing `OverlayStateChangedPush{Showing}` pauses Origins | parity, pause-without-focus |
| S8 | **Overlay call catalog** | Run more games (BF Resynced, newer R2 titles) with the bridge and grep their logs for `overlay_req`, friends UI calls and micro-app names | priorities in section 7 |
| S9 | **Sensors** | NVML on the RTX 5070 laptop (temp, power, VRAM, clocks) without waking a sleeping dGPU when no game runs; CPU/battery via hwmon/UPower | perf section |
| S10 | **Replay** | gpu-screen-recorder replay buffer on Hyprland + NVIDIA (portal vs KMS capture), GPU and VRAM cost while playing, interaction with FG and VRR | capture |
| S11 | **XR** | Sight surfaces visible in HypXRland's monitor quads; the case where a game window is redirected to its own quad; hypxrhud toasts | XR support |
| S12 | **Session identity** | Implement `QUIVER_SESSION` + pids + window address on `ActiveSession` (PROPOSAL's markers); verify the env survives Proton and pressure-vessel into the game's Vulkan process | everything that maps a game to its session |

Other risks:

- **GPUI churn.** A GPUI bump touches the whole app; layer-shell support was reverted once
  upstream. The HUD adds an off-screen patch on top (SIGHT-HUD §4.3). Mitigation: S2 early;
  fork pin if needed.
- **Layer ABI and stability.** A layer bug crashes games. Mitigation: telemetry plus a HUD
  compositor that draws no text and parses nothing (SIGHT-HUD §2.8), no allocation in present,
  fuzz with validation layers, per-game kill switch, inert without a session id.
- **Toast spam.** Origins re-earns achievements at every load (section 2). Mitigation: the
  local earned set in quiverd; toasts only for ids not earned before.
- **Wine-Wayland vs XWayland differences** in focus and pointer lock (S1 covers both).
- **Anti-cheat.** Only the layer is in-process; disabled for flagged games.

---

## 12. Phasing

**Phase 0: spikes and small fixes (no Sight UI).**

- Bridge: reply to every overlay/friends-UI request that has a response type (section 7 "fix
  now"). Forward `EarnAchievement`, rich presence and overlay calls to quiverd as `SdkEvent`
  (logged only).
- quiverd: session ids and markers (S12), local earned-achievement set per account and game
  (also answers `GetAchievementsReq`).
- Spikes S1, S2, S3, S6, S7 (in that order), with SIGHT-HUD's phase 0 (H1–H5, H8). Decide
  steers.

**Phase 1: MVP.**

- `quiver-sight` process with presenter 1 (layer-shell), started by quiverd per session; it
  also renders the pinned HUD (SIGHT-HUD phase 1, which has its own scope).
- Summon: Hyprland bind installed while a Quiver game is focused; hyprpad snippet; CLI
  `quiver sight toggle|open <section>|notify|status`.
- Toast lane: achievement unlocked (deduped), cloud-save pull/conflict, download/update done,
  battery low, disk low, signed in elsewhere, play time not reported, mod apply results, agent
  notes.
- Panel sections: **Session** (game, profile, mod versions, saves, play time, clock, battery,
  quit/force quit), **Mods** (live MOD-CONFIG forms, pending restart edits), **Achievements**
  (definitions + earned set, spoiler masking), **Performance** (SIGHT-HUD metrics: fps,
  frame-time graph, base vs presented, CPU/GPU/VRAM/temps/power; HUD preset; MangoHud on/off).
- Sight layer (S3) for Quiver launches and Steam copies: telemetry and the HUD compositor.
- Bridge: `OverlayShowReq` for `Home/Show/Achievements/GameOptions` opens Sight (reply `Ok`).
  `ShowBrowserUrl`/`Shop` produce link cards.
- Steam copies supported; anti-cheat-flagged games get toasts and panel but no layer.

**Phase 2.**

- Friends and presence, invites with actionable toasts, friend picker for games (needs quiverd's
  friends service first). Club challenges/rewards (read-only), rich presence text.
- `OverlayStateChangedPush` and `InitRsp.overlay.enabled` (after S7).
- Capture: screenshots (compositor and layer), achievement auto-capture, gallery, instant replay
  via gpu-screen-recorder.
- Quick menu, the HUD's `clock` layout, try-then-keep for mod settings, mod status metrics and
  actions.
- Audio (game volume via PipeWire), MPRIS media controls, power profile.
- XR: hypxrhud toasts.
- Agent approvals in Sight; notes per game; session reminders; DND/mako integration.

**Phase 3 (only for the cases that need it).**

- Presenter 2: in-game compositing via the Sight layer (S4, S5) for gamescope sessions,
  non-layer-shell compositors, game-only recordings.
- FPS limiter integration, colorblind filters (layer shaders), pinned windows, chat, party,
  plugin panels beyond MOD-CONFIG, Discord rich presence, time limits. (The own perf HUD moved
  to the MVP, SIGHT-HUD.)

---

## 13. Steers wanted

**Decided 2026-11-16:** the user accepted every recommendation below as written (with the
SIGHT-HUD notes applied). Each steer's *Recommend* is the decision; the other options are kept
for the record.

1. **Name.** (a) Sight; (b) Nock; (c) Fletch; (d) plain "Quiver HUD".
   *Recommend (a): short, archery-themed, and unlike "overlay" or "HUD" it isn't already taken
   by overlayfs mounts or MangoHud.*
2. **Architecture baseline.** (a) compositor-first panel + telemetry-only Vulkan layer, in-game
   drawing later; (b) Vulkan-layer-first, everything drawn in-game from day one; (c) in-process
   Windows hooks like Connect's `overlay64.dll`; (d) require gamescope.
   *Recommend (a): works on your Hyprland setup for every game with no injection risk, and keeps
   (b) open as a second presenter.*
   *Affected by SIGHT-HUD:* the layer composites the pinned HUD from the MVP whichever option is
   chosen, so the layer is no longer "telemetry-only" in (a); (a) now means the panel and toasts
   stay on the compositor. (b) would extend the HUD's compositing path to toasts and the panel.
3. **UI toolkit.** (a) GPUI in a separate process, sharing the app's components (needs a GPUI
   bump for layer-shell); (b) egui drawn inside the layer; (c) both (egui for toasts in-game,
   GPUI for the panel).
   *Recommend (a): one design system, crash isolation, and the panel is where the work is.*
   *Affected by SIGHT-HUD:* the HUD is already rendered by GPUI in `quiver-sight` (off-screen),
   and the GPUI bump is now required by the HUD for off-screen rendering whatever is chosen here.
   (b) and (c) would put egui next to the HUD's GPUI rendering: two toolkits in game.
4. **Summon chord.** (a) Shift+F2 (Connect parity), bound only while a Quiver game is focused;
   (b) a Super chord (e.g. Super+Grave), always bound; (c) no default, user sets one. Controller:
   hyprpad `guide+start` in all cases.
   *Recommend (a): muscle memory and in-game hints already say Shift+F2, and binding it only
   while a game is focused keeps it out of other apps.*
   *Note:* the HUD's keys are decided as Shift+F3 / Ctrl+Shift+F3, bound the same way, chosen to
   sit next to Shift+F2; with (b) or (c) they'd no longer be next to the panel's chord.
   *As built (2026-11-17):* the HUD's keys are read in the game by the Sight layer (X11 keymap
   polling, §4.1's "in-layer key polling"), not bound in Hyprland (SIGHT-HUD §1.3). The panel's
   Shift+F2 is read the same way, X11 and Wayland windows alike (§14); a Hyprland bind is only
   a printed fallback (`quiver sight keybinds --conf`).
5. **Gamepad while Sight is open.** (a) rely on hyprpad to neutralize the game's pad;
   (b) Quiver grabs the pad (EVIOCGRAB) while open, reversing PROPOSAL's "never grab";
   (c) accept that the game also sees the pad.
   *Recommend (a): hyprpad already has the mode machine and layer-namespace rules; no
   new device ownership.*
6. **Focus behaviour.** (a) the panel takes keyboard focus (the game sees focus-out, usually
   pauses); (b) never take focus; rely on SDK `OverlayStateChangedPush` and in-process input
   hooks later; (c) per game, default (a), fallback "toasts only".
   *Recommend (c): S1 will find games that misbehave; the default stays simple.*
7. **Superseded (2026-11-16) by [SIGHT-HUD.md](SIGHT-HUD.md):** the user chose Quiver's own
   pinned HUD with pluggable, mod-provided metrics: drawn in the game by the Sight layer, which
   composites an image `quiver-sight` renders off-screen with GPUI, with an automatic
   layer-shell fallback; MangoHud stays a per-game option. The original steer, for the record:
   **Performance HUD.** (a) integrate MangoHud (Sight toggles it and its presets; Sight's panel
   shows its own numbers); (b) replace MangoHud with a Sight HUD; (c) Sight panel numbers only,
   MangoHud left as is.
   *Recommend (a): MangoHud is mature and already in your launches; replacing it needs in-game
   drawing (phase 3).*
8. **Store, DLC, rewards, web pages.** (a) link out (xdg-open + QR), read-only owned DLC and
   rewards in the panel; (b) embed a browser engine for Ubisoft's store/web pages; (c) omit
   entirely and answer `Unavailable`.
   *Recommend (a): parity where it costs nothing, no browser engine or payment flows.*
9. **Tell games an overlay exists.** (a) after spike S7, set `overlay.enabled` and push
   `OverlayStateChangedPush`; (b) keep reporting no overlay; (c) per game opt-in.
   *Recommend (a) if S7 shows no DLL loading, else (c).*
10. **Social scope.** (a) friends list + presence + invites (v2), chat later; (b) full Connect
    parity including chat and party in v2; (c) no social in Sight.
    *Recommend (a): invites matter for co-op titles; chat with a gamepad is low value.*
11. **Capture.** (a) orchestrate gpu-screen-recorder for clips/replay, screenshots via grim
    (with Sight) or the layer (game only); (b) own layer capture + encoder; (c) leave capture to
    Omarchy's tools, Sight only shows the gallery.
    *Recommend (a): NVENC replay exists today; Sight adds the per-game folders and the
    achievement auto-shot.*
    *Affected by SIGHT-HUD:* "HUD in captures: Shown / Hidden" is decided (SIGHT-HUD §2.12), and
    Sight's own screenshots and clips always offer a version with and without the HUD. For
    screenshots that's the layer's copy in (a). For clips, gpu-screen-recorder records the
    output, where the in-layer HUD is already baked in, so "clip without HUD" needs frames from
    the layer: part of (b), or a limit on (a) for clips.
12. **Notifications while playing.** (a) Sight toasts replace Quiver's desktop notifications
    while a game is focused, and mako goes to a DND mode during play (opt-in); (b) both Sight
    toasts and desktop notifications; (c) desktop notifications only (no toast lane).
    *Recommend (a) with mako DND opt-in.*
13. **Anti-cheat games.** (a) no Sight layer for games flagged with anti-cheat, panel and toasts
    still on; (b) no Sight at all for them; (c) always on.
    *Recommend (a): the panel never touches the game process.*
    *Note:* SIGHT-HUD decided the matching HUD behaviour: layer off for flagged games, HUD on the
    layer-shell fallback. Under (b) those games would get no HUD either.
    *As built 2026-11-18:* (a), with the flag from install-folder markers (SIGHT-HUD §1.8).
14. **Agents in Sight.** (a) agents may post toasts/cards (Allow, rate-limited) and request
    approvals; asking an agent from Sight comes later; (b) also let agents open the panel on
    any section; (c) no agent surface in Sight.
    *Recommend (a): matches the existing policy (read and notify are Allow) without letting an
    agent pull you out of a game.*
15. **Steam copies.** (a) Sight on for Steam copies like Quiver installs; (b) off when Steam's
    overlay is present; (c) toasts only.
    *Recommend (a): different chords, and Sight lives outside the game process.*
16. **Where Sight's UI code lives.** (a) a new crate/binary `quiver-sight` with a
    shared `quiver-ui` crate extracted from `quiver-app`; (b) a mode of the app binary
    (`quiver-app --sight`); (c) inside quiverd.
    *Recommend (a): separate lifecycle (per session, systemd user unit) without a second copy of
    the components.*
    *Affected by SIGHT-HUD:* the HUD is rendered with `quiver-ui` components in `quiver-sight` and
    needs that process for the whole session, so the `quiver-ui` extraction is needed in the HUD's
    MVP. (c) is ruled out for the HUD: decision 18 keeps the Vulkan device (100–280 MiB RSS) out
    of quiverd.

---

## 14. As built: the panel MVP (2026-11-17)

*2026-11-18:* every game Quiver starts (Ubisoft, Epic, Steam through `quiver steam-run`) gets
the Sight layer by default with the HUD hidden, so Shift+F2 and Shift+F3 work in any game with
no per-game step; anti-cheat games are the exception (SIGHT-HUD §1.8).

Phase 1's panel, built on the HUD's `quiver-sight` process (SIGHT-HUD §2.9) with the steers of
§13 as decided: a compositor panel on a wlr-layer-shell surface, GPUI in `quiver-sight`,
Shift+F2, hyprpad holding the game's pad neutral (5a), and the panel taking keyboard focus
(6a; the per-game "toasts only" fallback of 6c isn't built).

**Summon and dismiss**

- **Shift+F2 in the game.** The Sight layer's key reader (`quiver-sight-layer/src/keys.rs`,
  X11 keymap polling and the Wayland `wl_keyboard` of SIGHT-HUD §1.3) has a third binding,
  `Shift+F2 → Action::Panel`, matched like the HUD's (exact modifiers, edge only). Presses
  are counted in a new `qv_layer_status.key_panels` (the struct grew from 256 to 264 bytes;
  a layer from before writes 256 and reads as zero). quiverd's ring thread diffs it like
  `key_toggles` and calls `PerfHub::panel_keys`, which issues a panel toggle for that session
  (`by: "keys"`). No Hyprland bind is installed; `quiver sight keybinds --conf` prints one
  (Lua and hyprlang forms) for games without the layer.
- **Closing** is the panel's own job: while it is open it has the keyboard, so the game's
  layer sees nothing. Shift+F2, Escape, B at the top level, or the chord again close it.
- **CLI**: `quiver sight toggle [--section S] [--session ID] | open [SECTION] | close |
  status [--json] | hyprpad [--snippet] | keybinds [--conf] | pad-chord [on|off]`.
- **Control plane** (`quiver-api/src/sight.rs`, `quiverd/src/perf/panel.rs`):
  `Request::Sight(SightRequest::{Toggle, Open, Close, Status, Report})`. quiverd picks the
  session (named, else the focused game, else the only one: `PerfHub::pick`, as `hud toggle`
  does), starts `quiver-sight` if needed, and broadcasts a `PanelCommand` (seq, action,
  session, section, the game's monitor, who) as `PerfEvent::Panel` (`Event::SightPanel` for
  new clients; old clients skip it). It keeps the last command, so a `quiver-sight` started
  for it acts on it if it's under 8 s old (`PanelCommand::fresh`). quiverd keeps no open/closed
  state of its own: a toggle goes through as a toggle and `quiver-sight` decides.
  `quiver-sight` sends `Report(PanelReport)` on every change (open, section, monitor, backdrop
  and why, pad, focus target, last error), which `quiver sight status` prints.

**The surface** (`quiver-sight/src/panel/mod.rs`)

- Namespace `quiver-sight`, overlay layer, anchored to all four edges at the monitor's
  logical size, `exclusive_zone = -1` (over bars), keyboard interactivity **exclusive**. The
  toast lane moved to `quiver-sight-toast` (it shared `quiver-sight` before), so hyprpad's
  mode rule and the blur rule match only the panel; the desktop HUD stays
  `quiver-sight-hud`.
- Monitor: the command's (quiverd's game monitor), else the focused window's, else the
  focused one. Before opening it records Hyprland's active window (focus goes back there on
  close, `focuswindow`) and finds the game's window (a client whose process, or a parent a
  few levels up, has `QUIVER_SESSION=<session>` in its environment), which Quit closes.
- Checked on the live desktop with a 2.5 s test window (`--demo panel --namespace
  quiver-sight-test`): mapped on eDP-2 at the full 2048×1280 logical size, took the keyboard,
  gave focus back on close.

**Backdrop: compositor blur, else dim** (`quiver-sight/src/hypr.rs`)

- On Hyprland with `decoration:blur:enabled` on, opening adds a runtime layer rule that blurs
  the panel's surface with the user's own blur settings (size, passes: nothing overridden),
  `ignore_alpha 0.02`, and the panel draws a light tint over it (`ink-0` at 35%, a card at
  78%, theme tokens so light themes get a light tint). Otherwise, or if the rule fails, the
  plain dim of §9 (`ink-0` at 55%, card at 94%). `quiver sight status` says which and why
  ("blur is off in Hyprland (decoration:blur:enabled): plain dim" on this machine today).
- Hyprland's syntax is detected, not assumed: `eval hl.version()` answering `ok` means the Lua
  config (0.55+; this machine, 0.56.2): `eval if quiver_sight_panel_blur == nil then
  quiver_sight_panel_blur = hl.layer_rule({ name = 'quiver-sight-panel-blur', blur = true,
  ignore_alpha = 0.02, match = { namespace = '^(quiver-sight)$' } }) end
  quiver_sight_panel_blur:set_enabled(true)`, disabled again on close
  (`set_enabled(false)`); a config reload drops the rule and its global, and the next open
  adds it again. A hyprlang config ≥ 0.53 gets `keyword layerrule blur on, ignore_alpha
  0.02, match:namespace ^(quiver-sight)$`, older ones `layerrule = blur,…` and
  `ignorealpha 0.02,…`; keyword rules can't be removed without a reload, so they are added
  once per `quiver-sight` run and left (they only match the panel). Focus and Quit use
  `dispatch hl.dsp.focus({ window = 'address:…' })` / `hl.dsp.window.close(…)` under Lua,
  `focuswindow` / `closewindow` otherwise. Nothing is written to the user's config. Side
  effect noted in Hyprland's source: every `eval` clears Hyprland's stored config-error list.
- The blur keeps the game composited while the panel is open, but nothing is lost by it:
  direct scan-out is already off while any Sight surface is on the game's monitor (§4.1 (c)).
  For compositors without layer blur, the Sight layer could blur the game's own image in the
  game (a shader pass when the panel is up): a possible phase 3 alternative next to
  presenter 2, not built.

**Sections** (`panel/state.rs` builds rows from `PanelData`; `panel/view.rs` draws them)

- **Session**: game and copy (Quiver install, Steam copy), profile, mod versions
  (`ModsState` profile's `resolved[].describe()`, the current resolution, not necessarily what
  launched), play time (this session from `ActiveSession.started`; total from the store's
  `GameDetail.playtime`), whether Ubisoft took the session; clock and battery in the header.
  **Quit** asks Hyprland to close the game's window (the game's own close, which may prompt);
  **Force quit** is quiverd's `Stop` (`wineserver -k` for Quiver installs). Both need a
  confirmation (Cancel focused by default).
- **Mods**: `OptionsList` for the running profile (not the platform surface), rows grouped by
  page and group, only options for the overlay (`overlay != false`, visible, up to the
  Advanced level). Bool/enum/int/float/key options step with Left/Right and cycle with A
  (`quiver_ui::options`, the app's logic moved there); dangerous and free-text options are
  read-only with "change it in the app". Each row shows the apply badge as the app does (LIVE,
  LIVE · ~10 s, AT SWAPCHAIN CREATION, RESTART, NEXT LAUNCH), the pending text, and after a
  change the outcome quiverd reported (`outcome_text`: "Applied live · the mod confirmed",
  "Saved · needs a restart", "Rejected: …"). Steps on a row are debounced 350 ms into one
  `OptionsSet` (source `sight`, saved to the profile, live where possible); then the section
  reloads. Changes made elsewhere (`OptionsLive` not by Sight) reload it too.
- **Tweaks**: the platform surface at the running profile's scope (else the game's), so a
  change always reaches this game; same rows and honesty (most are NEXT LAUNCH).
- **Performance**: tiles for FPS, 1% low, base FPS (when a mod provides it), frame time, GPU
  load/temp/power, VRAM, CPU, RAM, from a `PerfSubscribe` at 2 Hz held only while the panel
  is open. The HUD: shown (`HudToggle`), preset, position and size (`HudSet` at the game
  scope; `settings_changed` makes them live). When the HUD isn't set up for this launch the
  rows say NEXT LAUNCH and "Use Quiver's HUD" turns it on for the next one.
- **Achievements**: from `GameDetail.achievements` (definitions with unlock state where the
  store's data has it): n / m earned, unlocked first (newest first), hidden ones masked
  until A.

**Input** (`panel/state.rs` `Nav`, `quiver-ui/src/input.rs`, `quiver-ui/src/gamepad.rs`)

- One `Input` vocabulary for keyboard and pad, shared with the app (moved to `quiver-ui`):
  Up/Down move (side list or rows), Right/A enter the rows, Left/Right adjust a stepper or
  toggle (Left on a plain row goes back to the side list), A activates (cycles a value,
  opens a confirmation, reveals a hidden achievement), B goes back then closes, LB/RB and
  LT/RT switch sections (wrapping). Keys: arrows (and hjkl), Enter/Space, Escape/Backspace,
  Tab/Shift+Tab, `[` `]`, Page Up/Down. Mouse: click sections, rows, ◀ ▶, dialog buttons.
- **The controller, two paths.** Spike S6's question turned out to matter: in a hyprpad mode
  that doesn't forward, no evdev device carries the real pad (hyprpad reads the Steam
  Controller over a root-only hidraw node, grabs evdev pads like the Elite, and its virtual
  pad, which the game and gilrs see, goes neutral). So the panel takes the pad as **keys
  typed by hyprpad** in its `quiver-sight` mode (`tools/hyprpad/quiver-sight.lua`: D-pad and
  stick flicks → arrows, A → Enter, B → Escape, X → Space, LB/RB/LT/RT → Page Up/Down; held
  buttons hold keys, so key repeat works). gilrs still reads pads read-only (the app's
  `gamepad.rs`, now shared) for setups without hyprpad or pads hyprpad doesn't own; there the
  game sees the pad too, and the hint bar says so. The two never both deliver a press: in
  the `quiver-sight` mode gilrs sees only the neutral virtual pad.
- **Without hyprpad**, optional and off by default: Select+Start held 0.6 s toggles the panel
  (`quiver_ui::gamepad::HoldChord`; `quiver sight pad-chord on` writes `pad_chord = true` to
  `$XDG_DATA_HOME/quiver/sight.toml`, read when `quiver-sight` starts).
- **hyprpad snippet** (`tools/hyprpad/quiver-sight.lua`, `quiver sight hyprpad --snippet`):
  the mode (`ctx.layers:has("quiver-sight")`, an exact match), placed before the game modes
  since the first matching mode wins; the button → key binds, `:only_in("quiver-sight")`;
  and the chord. hyprpad keeps one binding per guide chord and `guide+start` is
  `guide+menu`, which the user's config uses for the Omarchy menu, so the snippet offers a
  mode-branching script (Quiver in game modes and over the panel, the menu elsewhere) or a
  direct bind where the chord is free. Quiver edits no hyprpad config.

**Lifecycle**: unchanged from the HUD: quiverd starts `quiver-sight` for every Quiver launch
and Steam copy, and now also on a panel command; it exits 5 s after the last session. A
session ending closes its panel. The panel needs nothing in the game besides the layer's key.

**Verification**: unit tests for navigation, rows, value stepping, confirmations, key
mapping (`panel::state`), Hyprland command forms, flavor detection, game-window matching
(`hypr`), the chord (`quiver-ui` gamepad), Shift+F2 over X11 and Wayland (layer `keys`), the
status layout, and quiverd's key → command path, commands and reports (`perf::tests::
panel_key_and_commands`). `quiver-sight --demo panel` shows the fixture on a desktop;
`cargo run -p quiver-sight --example panel_png -- OUT [--backdrop dim]` renders every section
off-screen. Not verified: a real game (Shift+F2 → panel → focus back), hyprpad's keys
reaching the surface, the blur (it's off on this machine), spike S1.

**Deferred**: per-game "toasts only" / "pause via SDK" fallbacks (6c) and
`OverlayStateChangedPush` (S7); the quick menu; try-then-keep (`live: Only` + Revert); mod
actions and status metrics in the Mods section; the frame-time graph in Performance; Club
challenges; cursor hiding for pad use; on-screen keyboard for text options; Shift+F2 as a
configurable chord; the panel on the game's own monitor when quiverd hasn't seen it focused
(falls back to the focused monitor); session id ⇄ window matching beyond `QUIVER_SESSION` in
the environment.
