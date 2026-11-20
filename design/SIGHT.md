# Sight: Quiver's in-game overlay (design notes)

Status: designed 2026-11-16; the HUD (SIGHT-HUD.md) and the panel MVP are built (stereo frames: [§15](#15-stereo-the-hud-and-the-panel-in-each-eye-2026-11-19)), see
[§14 As built](#14-as-built-the-panel-mvp-2026-11-17). *Decided 2026-11-18:* the panel is
drawn **in the game** by the Sight layer and takes the game's keyboard and mouse **inside the
game's process** (an LD_PRELOAD library, as Steam's overlay does), so the game keeps focus; the
layer-shell panel is the fallback ([§14, "In the game"](#in-the-game-2026-11-18)). Companion to `design/MOD-CONFIG.md`
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
| **input preload** | `libquiver_sight_input.so`, LD_PRELOADed into the game's processes with the layer: hands the game's keyboard and mouse to the panel while it's open in the game |
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
  it works under Wine-Wayland: as shipped in GE-Proton11-12, `VK_LAYER_GE_wayland_steam_overlay`,
  "Steam overlay input compatibility bridge for Wine Wayland", enabled by the proton script
  (`WINE_WAYLAND_STEAM_OVERLAY_LAYER=1`) when `gameoverlayrenderer.so` is in `LD_PRELOAD`; and
  Proton's winex11 itself drops key and mouse events in `process_events` while the named event
  `__wine_steamclient_GameOverlayActivated`, which lsteamclient sets, is signalled — the same
  idea, at the same place, as Sight's input preload, §14); gamescope (overlay windows marked with `GAMESCOPE_EXTERNAL_OVERLAY`,
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

*Decided 2026-11-18, after spike S1 failed in the field (§6, "Focus side effects"):* the panel's
presenter is **(a)**, the Sight layer compositing an image `quiver-sight` renders (the HUD's
path, at the panel's size), and its input is **in-process, below Wine's drivers**: not (b)'s
Windows-side hooks (no DLL in the game's address space, no D3D, no window-procedure hooks) but
an LD_PRELOAD library interposing the Linux calls Wine's X11 and Wayland drivers read input
with, as Steam's `gameoverlayrenderer.so` does. No surface takes the keyboard, so the game's
window never deactivates. (c) stays as the fallback for games the layer can't draw in. As built
in §14, "In the game".

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
5. *(Superseded 2026-11-18 for games the layer draws in: the panel takes input in the
   game's process instead, §14 "In the game"; what follows is the desktop fallback.)*
   **Input: the compositor and hyprpad.** Keyboard/mouse via the panel surface's focus;
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

- Keyboard and mouse: the panel surface has the keyboard and a full-output input region, so
  the game gets no keys or clicks (as built it is on-demand keyboard interactivity, so the
  compositor can still move focus away, which hides the panel: §14, "Tied to the game
  window"). XWayland pointer locks and constraints are
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
  Seen (2026-11-18): Alan Wake Remastered (D3D12 through vkd3d-proton, exclusive fullscreen) on
  Hyprland. Within about 1.5 s of the panel taking the keyboard, the game minimised itself. Its
  toplevel left Hyprland's list, and vkd3d-proton dropped its Vulkan swapchain (a minimised
  window has a zero max extent: "occlusion"), so the layer reported no swapchain. The game's
  next launch came up with a swapchain at most 375 px tall, at ~1000 fps: a tiny image
  stretched over the screen, "a single rectangle flashing colours". The game apparently
  kept the minimised window's size as its resolution (`resolution.xml`; inferred, not seen). The Sight layer passes vkd3d-proton's swapchain
  creation and presents through unchanged (tests/wine_vkd3d.rs). The panel's keyboard grab
  is what deactivated the game.
  *Fixed 2026-11-18* by not taking focus at all: the panel is drawn in the game and gets the
  game's input inside the game's process (§14, "In the game"); the layer-shell panel only opens
  for games the layer can't draw in.
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
  logical size, `exclusive_zone = -1` (over bars), keyboard interactivity **on demand**
  (*2026-11-18*; it was exclusive: see "Tied to the game window" below for why, and the
  exclusive fallback). The toast lane moved to `quiver-sight-toast` (it shared
  `quiver-sight` before), so hyprpad's mode rule and the blur rule match only the panel; the
  desktop HUD stays `quiver-sight-hud`.
- Monitor: the command's (quiverd's game monitor), else the focused window's, else the
  focused one. Before opening it records Hyprland's active window (focus goes back there on
  close, `focuswindow`) and finds the game's window (a client whose process, or a parent a
  few levels up, has `QUIVER_SESSION=<session>` in its environment), which Quit closes.
- Checked on the live desktop with a 2.5 s test window (`--demo panel --namespace
  quiver-sight-test`): mapped on eDP-2 at the full 2048×1280 logical size, took the keyboard,
  gave focus back on close.

**Backdrop: the game blurred by the Sight layer, else compositor blur, else dim**

In that order, picked when the panel opens (`quiver-sight/src/panel/mod.rs`); `quiver sight
status` prints the one in use (`PanelReport.backdrop`: `game_blur`, `blur`, `dim`) and why.
Both blurs get the same light tint from the panel; the dim is darker.

*The game blurred by the Sight layer* (added 2026-11-18; Quiver owns the blur, so it works
under any compositor, and with Hyprland's blur off, as on this machine). The layer already sits
on the game's swapchain at present time, so while the panel is open it blurs the game's own
frame there, under the HUD, and the compositor draws the panel over an already-blurred game.

- **The request** goes from `quiver-sight` straight into the session's `hud.image` header, the
  per-session channel the layer already maps (`qv_hud_header.backdrop`: mode, dim and radius in
  one 64-bit word, and a deadline `backdrop_until`; SIGHT-HUD §3.8). Only that session's game
  reads it, so only that game blurs. `quiver-sight` renews the deadline (now + 2 s) every half
  second while the panel is open and clears it on close; if `quiver-sight` dies, the request
  lapses with the panel's surface. quiverd doesn't relay it: it keeps no panel state (above),
  and the writer that owns open/close owning the request too means no stale blur after a crash.
  quiverd's part is telling `quiver-sight` what the layer can do: every `HudModel` carries
  `backdrop` (`LayerBackdrop`: the `hud.image` path whatever the HUD's presenter, `ready`,
  `blurring`, and why not), from the layer's status block.
- **The layer** (`quiver-sight-layer/src/blur.rs`) adds `SAMPLED` (with `COLOR_ATTACHMENT`)
  usage at `vkCreateSwapchainKHR` when the surface allows it and the format can be sampled with
  a linear filter and blended into, and reports `LAYER_BLUR_READY` (or `backdrop_error`:
  `format`, `usage`, `queue`, `internal`, `off`). On a live request it records, before the HUD's
  render pass in the same submit: the swapchain image to `SHADER_READ_ONLY`, a 4×4 box filter to
  a quarter-size `R16G16B16A16_SFLOAT` image, a dual-filter (Kawase) blur of 1–5 levels down and
  back up, and a full-size pass that writes the result over the swapchain image. The radius is
  2.5% of the swapchain height by default (levels and filter offset from the height, so 1080p
  and 4K look the same). It fades in over 160 ms and out over 200 ms of the game's own presents,
  and the HUD is drawn sharp on top. Images and pipelines are made the first time a panel asks
  (about 5.5 MiB at 4K) and kept with the swapchain; with no request the present path is one
  extra 64-bit load. `QUIVER_SIGHT_BLUR=0` turns it off (no usage added).
- **Formats**: any colour space, because the blur reads the image and writes it back in the
  same encoding: SDR UNORM (blurred as encoded, like a compositor), `_SRGB` (in linear light),
  HDR10 (`A2B10G10R10`/`A2R10G10B10` + ST 2084, in PQ), scRGB (`R16G16B16A16_SFLOAT`, linear).
  The HUD itself still can't draw on HDR swapchains; the blur can, since its compositor exists
  for either. DXVK and vkd3d-proton games are Vulkan games here. The layer sits below vkBasalt
  and frame generators (SIGHT-HUD §2.4), so it blurs the post-processed frame and every
  generated one.
- **Fallback**: the panel uses the layer when the session's model says `ready`; it expects
  `blurring` within 2.5 s (the layer sets it for half a second after each blurred present) and
  otherwise drops the request and switches to the compositor blur or the dim, as when the layer
  isn't there (OpenGL games, the layer not loaded or disabled, no `hud.image`), can't blur this
  swapchain, or the game stops presenting when it loses focus.
- **Cost**, GPU time per present from timestamp queries (`quiver hud status` shows the layer's
  running mean, `backdrop_gpu_ns` in its status block): on the AMD 890M iGPU (shared LPDDR5x,
  RADV) 0.21 ms at 1080p, 0.37 ms at 1440p, 0.42 ms at 2560×1600, 0.66 ms at 4K; it is
  bandwidth-bound (one full-size read, one full-size write, small passes), so a discrete GPU
  should be several times faster (estimated ≤ 0.15 ms at 4K at 500 GB/s; NVIDIA's driver has
  no headless surface, so the harness couldn't measure the RTX 5070). Paid only while the panel
  is open. `tests/vulkan_blur.rs` (opt-in, a GPU with `VK_EXT_headless_surface`) checks the blur,
  the sharp HUD over it, dim, fade-out, an expired request, sRGB in linear light, HDR formats
  when the surface offers them (RADV's headless surface doesn't), and prints the cost;
  `examples/sight_harness -- --blur` measures it on any device.
- **Known limits**: the blur needs the game to keep presenting (it's done on the game's
  frames): a game that stops rendering when it loses focus keeps its last frame, partly faded
  in, until the fallback; an app that doesn't redraw its whole swapchain image each frame
  would blur what it left (games redraw); presenting from a queue without graphics, or
  switching present queues every frame, skips it like the HUD.

*Compositor blur* (`quiver-sight/src/hypr.rs`), used only when the layer can't:

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
  (The first version noted the in-game blur above as a possible phase 3 alternative; it is now
  the first choice.)

**Tied to the game window** (*2026-11-18*; `quiver-sight/src/front.rs`, `toplevels.rs`,
`panel/mod.rs`; the layer's `focus.rs`)

A layer surface belongs to an output, not a workspace, so the first panel stayed up over the
user's other workspaces and windows, and held the keyboard there. Now the panel is tied to the
game's window: when the game isn't what the user is looking at (another workspace on that
monitor, another window active, the game minimised or closed) the panel **hides without
closing**, and when the game is in front again it comes back in the same state (section,
selection, drafts, scroll: the view outlives its surfaces). Hidden means no surface at all, so
no keyboard grab, and hyprpad's `quiver-sight` mode (`ctx.layers:has("quiver-sight")`) ends,
giving the pad back; gilrs input is ignored while hidden. Closing a hidden panel doesn't pull
focus back to the game. `quiver sight status` says `open, hidden` and why, and which toplevel
the panel is tied to and how it was found (`PanelReport.hidden`, `front`, `game_window`).

*Measured first* (a test client in a nested Hyprland 0.56.2, the same build as the desktop's,
switching workspaces and focus there; the user's session untouched):

| With a layer surface on the game's monitor | exclusive keyboard | on-demand keyboard |
|---|---|---|
| mapping it | the layer gets the keyboard; the game's foreign toplevel stays `activated` | the same (Hyprland focuses on-demand overlay surfaces on map) |
| workspace switch | **nothing**: no `activewindow` event, the game stays `activated`, the layer keeps the keyboard on the new workspace; only `workspace` events (socket2, ext-workspace) | the layer gets `leave`, the game loses `activated`, the new workspace's window gets it |
| switch to an empty workspace | — | the game loses `activated`, nothing gets it; Hyprland hands the keyboard straight back to the layer (`leave` + `enter`) |
| `focuswindow` / alt-tab to another window | refused ("exclusive ls") | the other window is `activated` |
| moving the game to a special workspace (Omarchy's minimise) | — | the game loses `activated` (transient states inside one batch) |
| switching exclusive → on demand after mapping | — | Hyprland drops the layer's keyboard at once |
| XWayland game (`glxgears`), fullscreen or not | — | same as native; `_NET_ACTIVE_WINDOW` stays the game while the layer has the keyboard, `0` when a Wayland window is focused |

So an exclusive panel can't be tied to anything but Hyprland's workspace events, and it is
what kept the keyboard on other workspaces. On demand, every case shows up as the game's
toplevel losing `activated`.

*Signals chosen*, in this order (`front::Front::verdict`):

1. **wlr-foreign-toplevel-management** (`zwlr_foreign_toplevel_manager_v1`, on a Wayland
   connection of `quiver-sight`'s own, its own thread; Hyprland, Sway, labwc, river, niri,
   Wayfire have it): the game is in front while its toplevel is `activated` and not
   `minimized`; a `closed` toplevel unbinds. Snapshots go out after each batch of events, so a
   compositor's transient states inside one change collapse. Compositor-agnostic and
   event-driven. ext-foreign-toplevel-list was ruled out (no state), ext-workspace too (no
   window ↔ workspace mapping).
2. **The panel's own keyboard**: a compositor may deactivate the game when a layer surface
   takes the keyboard, so "the panel has the keyboard and no other toplevel is activated" also
   counts as in front. Whether this compositor keeps the game `activated` under the panel is
   observed once the panel has had the keyboard 250 ms (`observe_panel`); Hyprland does, and
   then the same situation means an empty workspace (Hyprland gives the keyboard back to the
   panel there), so it counts as away. A just-mapped panel counts as having the keyboard for
   700 ms or until its first `enter` (two connections, no ordering between them), and hiding
   waits 120 ms of "away" so a change seen in two steps doesn't flicker. If an on-demand panel
   gets no keyboard within 700 ms (a compositor that doesn't focus on-demand surfaces on map),
   it is mapped again exclusive.
3. **Which toplevel is the game's** (the protocol has no pids): Hyprland's window for the
   session when Hyprland is there (the client whose process has `QUIVER_SESSION`, matched by
   class and title, the activated one of twins; optional, at most once a second); else the
   toplevel activated when the game's own Shift+F2 summoned the panel (`by: "keys"`; the
   key came from the focused game, and if the list isn't there yet the first snapshot within
   2 s decides); else learned: the toplevel that stays the only activated one for 400 ms
   while the game's Sight layer says the game is in front (one session claiming it, never
   overriding Hyprland's match). Learning runs whatever is shown, so a panel summoned later
   from a terminal (`quiver sight open`) knows the game: it opens hidden and shows when the
   game is in front.
4. **The Sight layer's word** (`LAYER_FOCUS_KNOWN` / `LAYER_FOCUSED` in its status block,
   flags bits 10–11, no size change; `HudModel::game_focus`, sent at once on a change): for
   Wayland windows its `wl_keyboard`'s `enter`/`leave` (keyboard focus: the panel taking the
   keyboard reads as "not in front"); for X11 windows `_NET_ACTIVE_WINDOW` against the game's
   Vulkan surface windows and their ancestors (Wine renders into a child of its toplevel),
   else the active window's `_NET_WM_PID`. The X11 side is a plain XCB connection, because an
   Xlib request on a just-destroyed window raises an X error that goes to Wine's process-wide
   handler (which asserts); XCB returns errors as replies. Both are read every 50 ms tick,
   also while the game doesn't present (a hidden game may stop), and the Wayland queue is
   now serviced every tick once attached (attaching still waits for presents). Used for
   learning (3), and as the signal when there's no toplevel list (KDE has layer shell but
   not the wlr protocol): then the game is in front while the layer says so or the panel has
   the keyboard.
5. **Nothing to go on** (no toplevel list and no layer word, as for an OpenGL game on such a
   compositor): unknown, and things stay shown as before. Hiding only happens when a signal
   can say when to show again.

Hyprland IPC stays optional: only the window lookup in (3) and what the panel did before
(monitor, return focus, Quit, the blur rule). No socket2 subscription was needed: on demand,
foreign-toplevel already reports everything socket2 would.

*Per case* (Hyprland, measured in the nested compositor with the real panel, `--demo panel`):

| Case | Panel | Desktop HUD |
|---|---|---|
| Workspace switch on the game's monitor (to a window or an empty workspace) | hides within ~130 ms; back in ~250–300 ms (a new surface) when the game's workspace is shown | hides, shows again |
| Alt-tab / focus another window on the same workspace | hides; shows when the game is focused again | hides; shows |
| Minimise (Omarchy: moved to `special:minimized`) | hides; shows if the special workspace is toggled up with the game focused, or when it's restored | same |
| Game closed (its window) | the binding drops; the session's end closes the panel | the session's end removes it |
| Fullscreen vs windowed | the same (overlay layers draw over fullscreen windows; `activated` is independent of fullscreen) | same |
| XWayland vs native Wayland game | the same through the toplevel list; the layer's word differs (X11: the active window, which stays the game under the panel; Wayland: keyboard focus, which the panel takes) | same |
| Focus on another monitor (multi-monitor) | hides (the game isn't the active window); comes back when the game is focused | hides too, though the game may still be visible there (known: activation, not visibility) |

*Toasts stay global*: they take no input, are click-through, last a few seconds and are
about moments (an achievement unlocked, a mod setting's outcome); hiding them on another
workspace would drop the news or show it late and stale, and a stray toast covers nothing
the user is working on. They stay on the game's monitor. *The desktop HUD* (the presenter for
games whose layer can't draw) follows the panel's rule, without the keyboard part: a frozen
or irrelevant FPS counter over other windows is the same bug; it's hidden before its first
model shows if the game isn't in front. *The in-game HUD* needs nothing: it's in the game's
frames.

*Verification*: `front::tests` (the Hyprland sequences above as fixtures, compositors that
deactivate the game, minimised/closed, twins, learning only from settled signals and
correcting a key binding, the list arriving late or going away, the no-protocol fallback),
`toplevels::tests` (state arrays), the layer's `focus::tests`, `tests/focus_x11.rs`
(Xvfb: Wine's client-window layout, the pid fallback, destroyed windows are answers, a dead
server is unknown), `keys_wayland` (enter/leave serviced without presents, unknown after the
surface goes), the opt-in Vulkan tests through the real layer (`vulkan_keys_wayland`:
`FOCUS_KNOWN|FOCUSED` then `FOCUS_KNOWN` after `leave`; `vulkan_keys`: an XCB window made
active and inactive on Xvfb), quiverd's `game_focus_from_the_layer`. In the nested Hyprland:
the real panel through workspace switches (to windows and empty ones), focus changes, the
special workspace, an XWayland fullscreen "game"; the desktop HUD through focus changes,
bound through Hyprland's window for the session (a `vkgears` with `QUIVER_SESSION`); and
`vkgears` with the real layer, native Wayland and XWayland, its status block following
focus. Not verified: a real game (Wine/DXVK on winewayland and XWayland), a compositor
other than Hyprland, the exclusive fallback on a compositor that needs it, multi-monitor.

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
off-screen. The layer's backdrop blur: `blur::tests` (levels and offset per resolution, the
fade), `formats::tests`, the request word (`quiver-perf` `hud::tests::backdrop_request`, the C
header's asserts), quiverd's `layer_backdrop_state`, and the GPU test above. Not verified: a
real game (Shift+F2 → panel → focus back, the layer's blur under the panel, the fallback when
a game stops presenting unfocused), hyprpad's keys reaching the surface, Hyprland's blur rule
(it's off on this machine), HDR swapchains on a real surface, spike S1.

**Deferred**: per-game "toasts only" / "pause via SDK" fallbacks (6c) and
`OverlayStateChangedPush` (S7); the quick menu; try-then-keep (`live: Only` + Revert); mod
actions and status metrics in the Mods section; the frame-time graph in Performance; Club
challenges; cursor hiding for pad use; on-screen keyboard for text options; Shift+F2 as a
configurable chord; the panel on the game's own monitor when quiverd hasn't seen it focused
(falls back to the focused monitor); session id ⇄ window matching beyond `QUIVER_SESSION` in
the environment.

### In the game (2026-11-18)

**Why.** Alan Wake Remastered (exclusive fullscreen, vkd3d-proton) minimised itself when the
layer-shell panel took the keyboard, vkd3d-proton dropped its swapchain, and the game saved a
tiny resolution (§6, "Focus side effects"). Any surface with the keyboard deactivates the game's
window. So the panel no longer takes focus at all: the **Sight layer draws it in the game's
frames**, and the **game's keyboard and mouse are taken inside the game's process**, below
Wine, the way Steam's overlay does it (§2, prior art: `gameoverlayrenderer.so` in `LD_PRELOAD`,
and Proton's own `process_events` filter while the Steam overlay is up). The layer-shell panel
above stays as the fallback.

```
 game process (Wine)                                   quiver-sight
 ───────────────────                                   ────────────
 winex11 / winewayland ──XCheckIfEvent / listeners──▶ libquiver_sight_input.so
                                                       │  keeps input from Wine while
                                                       │  the panel captures; forwards it
 VK_LAYER_QUIVER_sight ◀── panel.image ───────────┐   ▼
   composites the panel + cursor,                  │  panel.input (session dir): state,
   marks it drawn ──────────────────────────────▶ │  deadline, cursor, the layer's and the
                                                   │  preload's words, an event ring ──▶ PanelView in an
                                                   └──────────────────────────────────── off-screen GPUI window
```

**Which presenter** (`quiver-sight/src/panel/mod.rs`, `panel/ingame.rs`). `quiver sight toggle`
(and Shift+F2, the pad chord, `OverlayShowReq` later) opens the panel **in the game** when the
session's `panel.input` says the layer can draw on the game's swapchain (its word:
`PANEL_LAYER_CAN_DRAW`, the HUD's formats, drawing on, fresh within 3 s) and the preload has
hooked the game's input (its word: `INPUT_X11` or `INPUT_WAYLAND`, fresh); otherwise on the
desktop (OpenGL games, the layer turned off or anti-cheat, an HDR swapchain, no preload, a
launch from before). If the layer hasn't composited it within 2 s of opening (the game isn't
presenting), it moves to the desktop. `quiver sight status` says which and why (`drawn:`,
`PanelReport.presenter`, `presenter_detail`). `QUIVER_SIGHT_PANEL=desktop` in quiver-sight's
environment forces the desktop. In the game there is nothing to hide or show when the game
isn't in front (the panel is in its frames), focus never moves, and nothing is restored on
close.

**The files** (quiver-perf `panel.rs`; the session dir, next to `hud.image`):

- `panel.input` (4 KiB header + 1024 × 32-byte event slots), made by quiver-sight when it
  first sees the session. quiver-sight writes `state` (open flag, cursor flag, a new epoch per
  open), `until` (a deadline at most 2 s ahead, renewed to now + 1 s every 250 ms tick),
  `opened_ns`, its pid, and the cursor (swapchain px). The layer writes its word (pid,
  `PANEL_LAYER_CAN_DRAW`, the swapchain extent, a 250 ms heartbeat) and `drawn_ns` /
  `drawn_epoch` from every present that composited the panel. The preload writes its word
  (pid, the hooks it has, capturing, counters) and the events.
- `panel.image`: the `hud.image` format (double buffer, per-buffer seqlock, publish counter,
  heartbeat) with full-screen limits (4096 px a side, 64 MiB per buffer), made by
  quiver-sight at open, sized for the swapchain, replaced for a new extent. Its descriptor's
  new `dirty_y0`, `dirty_y1`, `dirty_base` (in what was padding: zero in every `hud.image`)
  say which rows changed since an earlier publish; a reader whose texture holds that publish
  copies only those rows.

**The layer** (`quiver-sight-layer/src/panel.rs`, `compositor.rs`). The heartbeat thread maps
`panel.input`, writes the layer's word, and maps `panel.image` while the panel is open (dropped
when it closes: 32-bit games have little address space). Per present, one atomic load while
closed; while open it takes a new image into a second plane (its own staging buffer, texture,
descriptor sets; only the dirty rows when its texture holds the base), draws it after the HUD
in a render pass of its own, and draws the cursor on top (an arrow, two triangles through the
graph pipeline, at the descriptor's scale). The panel is drawn only while its image is fresh
(heartbeat within 3 s) and laid out for this extent. The backdrop blur behind it is the
existing request in `hud.image`, so the order is game, blur, HUD, panel, cursor. Cost: an
upload only when the panel changes (a few rows for a selection move; the whole image, 16 MiB
at 2560×1600, for a section change), one full-screen textured quad per present while open.

**quiver-sight** (`panel/ingame.rs`). The same `PanelView` in an off-screen GPUI window (the
HUD's path, SIGHT-HUD §2.9) at `extent / factor` logical px, where `factor` maps the game's
monitor's logical height onto the swapchain's (2560×1600 under a 1.25 scale: 1.25, as the
desktop panel), rendered when it changes (≤ 60 Hz) and published with its dirty rows. A thread
drains the event ring every 4 ms and the main thread turns events into GPUI input on the view:
keys into `KeyDown`/`KeyUp` with modifiers (evdev codes, US names; Shift+F2 is skipped, the
layer's own key reader toggles the panel through quiverd as before), X11 pointer positions as
they are (the game's window is the swapchain's size; with the game's own cursor shown the two
coincide), Wayland surface positions and relative motion as deltas, buttons and wheels at the
cursor. The cursor is shown once the mouse is used and hidden by a key (pad navigation).

**The preload** (`crates/quiver-sight-input`, `libquiver_sight_input.so`). quiverd appends
`<lib>/quiver/sight/$PLATFORM/libquiver_sight_input.so` to the launch's `LD_PRELOAD` whenever
the launch has the Sight layer (`quiver hud enable/disable GAME` stays the one per-game switch;
no separate switch was needed; `QUIVER_SIGHT_INPUT=0` in a launch's env turns it off), and
`quiver steam-run` keeps Steam's own `LD_PRELOAD` first (STORE-STEAM G6). ld.so's `$PLATFORM`
is `x86_64` or `i686`, so 32-bit processes load the 32-bit build. It has to be a preload: Wine's
drivers bind their Xlib and libwayland symbols when Wine loads them, before any Vulkan layer,
and only an object earlier in the lookup order can interpose a symbol.

What Wine actually calls (GE-Proton11-12's `winex11.so` / `winewayland.so` imports, and the
Wine 10 source in the GE-Proton10-34 tree):

- **winex11**: every event it handles comes off the queue in `process_events` through
  **`XCheckIfEvent`** with its `filter_event` predicate. It never calls `XNextEvent`,
  `XIfEvent`, `XPending` or `XPeekEvent`; `XWindowEvent` (GraphicsExpose) and
  `XCheckTypedWindowEvent` (clipboard) never return input. XInput2 events are `GenericEvent`
  cookies through the same call; Wine reads their data with `XGetEventData`, which it resolves
  with `dlsym` on its own libX11 handle (not interposable, and not needed). Raw input
  (`RegisterRawInputDevices`) puts it in its XInput2 raw mode, where `WM_INPUT` comes from
  `XI_RawMotion`/`XI_RawButtonPress`/`Release`. `SetCursorPos` is `XWarpPointer`.
- **winewayland**: listeners on `wl_keyboard`, `wl_pointer` and `zwp_relative_pointer_v1`,
  registered with the inline `*_add_listener`, i.e. **`wl_proxy_add_listener`** from
  libwayland-client (loaded locally as winewayland's dependency, so the real one is found with
  `dlopen(RTLD_NOLOAD)` when `RTLD_NEXT` doesn't see it). Wine repeats held keys itself.

So it interposes `XCheckIfEvent` (and `XIfEvent`, `XNextEvent` for anything else in the process
that reads X input that way), `XWarpPointer`, and `wl_proxy_add_listener`, which for those three
interfaces installs a dispatcher (`wl_proxy_add_dispatcher`, Wine's listener as the
dispatcher's data and its user data as the proxy's) that calls Wine's handlers or doesn't. The
Sight layer's own `wl_keyboard` resolves libwayland through its own handle and is never
wrapped. What the game gets while the panel captures (`filter.rs`, the same rules for both
drivers):

| Event | While capturing | After the panel closes |
|---|---|---|
| key or button press | the panel's (forwarded) | the game's |
| release of what the game saw pressed (held when the panel opened, Shift+F2 itself) | **the game's**, so nothing sticks | the game's |
| release of what was pressed in the panel | the panel's | **kept from the game** (no key-up without its key-down: the Escape that closed the panel) |
| XInput2 key/button press and release (raw input) | paired by count per class (their codes are only in the cookie data, which a predicate can't read) | same |
| motion, wheel, enter/leave, XInput2 motion and touch, `relative_motion` | the panel's | the game's |
| focus, configure, keymap, `enter`/`leave`/`modifiers`/`keymap`/`frame` on Wayland | the game's | the game's |
| `XWarpPointer` (the game re-centring the pointer) | dropped | passed |

It never blocks Wine's threads: the interposed calls load atomics, read the vDSO clock and
write lock-free ring slots; a thread of its own, started by the first interposed call and only
with `QUIVER_PERF_DIR` set, maps `panel.input` and does every syscall. Nothing runs at load (it
is in every process of the prefix: wineserver, explorer, the game). A panic in it passes the
event through. Same glibc baseline as the layer (newest symbol version 2.34 on both
architectures; `tests/glibc.rs` keeps it so).

**Failsafe: the game is never left with dead input.** Input is captured only while all of
these hold (quiver-perf `panel::capture`, checked by every interposed call): the panel is open;
its deadline is ahead and at most 2 s away (quiver-sight renews it every 250 ms, so a
quiver-sight that hangs releases within a second, one that exits releases at once); the layer
marked this open panel drawn within the last second, or it opened less than 1.5 s ago (a game
that stops presenting, a layer that can't draw, a stale image: released); and quiver-sight's
pid still exists (checked by the preload's thread every 50 ms while open: a killed
quiver-sight releases within ~60 ms; a pid namespace that hides it never counts as dead).

**Controller.** hyprpad's `quiver-sight` mode (pad held neutral, buttons typed as keys) used to
trigger on the panel's layer-shell surface, which the in-game panel doesn't have. quiver-sight
now writes `$XDG_RUNTIME_DIR/quiver/sight-panel` (`open <pid> <game|desktop> <session>`) while
the panel is open (removed on close and at startup), and `tools/hyprpad/quiver-sight.lua`'s rule
is `ctx.layers:has("quiver-sight") or` that file naming a live pid. The keys hyprpad types reach
the focused window, the game, where the preload hands them to the panel. hyprpad re-runs rules
on compositor events and on its periodic rescan (500 ms): opening the in-game panel changes no
window, so the mode follows within that interval.

**Verification** (opt-in tests are `--ignored`; the Wine ones need `QUIVER_TEST_PROTON`):

- Unit: the filter's state machine (held keys, keys pressed in the panel, counted classes,
  reset, pure decisions), Wayland event classes, `panel.input` (capture rules, cursor, ring
  order, laps, four concurrent writers), `panel.image` (limits, partial reads), key and pointer
  translation, factors, the launch's `LD_PRELOAD` (after the launch's own, never without the
  layer, `QUIVER_SIGHT_INPUT=0`), the preload's install paths, the cursor's shape.
- `quiver-sight-input/tests/wine_input.rs` (GE-Proton11-12 winex11 on Xvfb, XTEST input, a
  Windows probe printing every key/mouse/raw-input message): closed, all reaches the game;
  open, none does (no `WM_KEY*`, button, wheel, move or `WM_INPUT`), the panel's ring has the
  keys, buttons, wheel and positions; Shift and F2 held at open and W held are released to the
  game; C pressed in the panel and released after close never reaches it; nothing held
  afterwards (`GetAsyncKeyState`); the failsafes: the owner killed (released in ~10 ms), the
  deadline not renewed (~1.0 s), the layer not drawing (~1.5 s); with the cursor clipped and
  hidden (XInput2 raw mode) and re-centred every 50 ms, no `WM_INPUT` while open and the
  pointer follows the motion instead of the game's warps.
- `quiver-sight-input/tests/wayland_input.rs` (GE-Proton11-12's winewayland on a fake
  compositor, `tests/wayland/fakecomp.c`: libwayland-server, private runtime dir and socket):
  the same checks for keys, buttons, motion, wheel and relative motion, the held key released
  to the game, the panel-pressed key kept from it after close, the dead-owner failsafe.
- `quiver-sight-layer/tests/vulkan_panel.rs` (headless Vulkan, read back): the panel's pixels,
  a partial update (rows outside the dirty range untouched), the cursor, the layer's word and
  drawn marks, nothing after close.
- `quiver-sight-layer/tests/wine_panel.rs` (end to end: a D3D12 program under Proton with
  vkd3d-proton, the real layer and the real preload, the test standing in for quiver-sight):
  the panel and its cursor on screen over the game's frames, capture held past the grace by
  the layer's drawn marks, released 3.9 s after the image stops being refreshed (3 s staleness
  plus 1 s), the game's frames after close.
- `quiver-sight/tests/ingame_panel.rs` (quiver-sight's real `PanelCtl`, headless GPUI, the
  layer's and preload's words faked): the in-game presenter chosen, `panel.input` opened, the
  image at the extent, a key moving the section with a partial image after it, Shift+F2
  ignored, the cursor following the pointer, the state file written and removed; with no
  preload it refuses the in-game presenter and says why.
- The hyprpad rule, evaluated with Lua against state files (none, a live pid, a dead pid).

Verified live (2026-11-18): Alan Wake Remastered (vkd3d-proton, exclusive fullscreen, XWayland
on Hyprland). The panel opened in the game for about 15 s and closed again; the user reported it "works
well". Which inputs were used wasn't recorded.

Not verified: AC Origins (DXVK with Pharos frame generation), a real compositor under
winewayland (only the fake one), Steam copies inside pressure-vessel (`$PLATFORM` in `LD_PRELOAD` there, and
`/dev/shm` shared), 32-bit games, mods that read input their own way (DirectInput, XInput pads
are unaffected: the pad is hyprpad's), IME text input, multi-monitor, HDR swapchains (the
panel isn't drawn on them; the desktop panel opens).

**In a lab instance** (design/LAB.md §12, 2026-11-19): `quiver lab create … --sight` gives an
agent the same panel and HUD in a headless game. A lab `quiver-sight` runs inside the game's
container, sharing its pid namespace and `/dev/shm`, so the failsafe's pid check works. It has
no quiverd: its HUD model comes from the layer's ring, and panel actions that would change
settings are refused and logged. `quiver lab sight N status` and `quiver lab logs N` show the
presenter, capture and failsafe releases. The preload's release lines now name the panel's
epoch and the reason (`input back to the game (panel N): failsafe: …`), from the same snapshot
as the decision (quiver-perf `panel::capture_why`).

**HDR, in a lab instance** (design/LAB.md §13). On an HDR10 or scRGB swapchain the layer still
can't draw the HUD or the panel (`DRAW_ERR_FORMAT`). On the desktop the panel moves to its
layer-shell window. In a headless instance there's no layer-shell: Shift+F2 leaves the panel
closed, with the reason in its report, and the game keeps its input (no capture without the
panel in the frames). Shift+F3 turns the lab HUD on, but nothing is drawn. Measured with the
D3D12 test program under gamescope's forced-HDR headless output.

Two changes to the layer:
- **`vkSetHdrMetadataEXT`:** the layer now intercepts it and passes it on unchanged, putting
  the last metadata in its status block (`hdr_*`).
- **Captures:** with `QUIVER_SIGHT_CAPTURE=1` (lab launches only), it captures the presented
  image in its own format on request (`capture.req` → `capture-<n>.raw/.json`).

Under gamescope the layer has to sit above gamescope's WSI layer to see the HDR swapchain at
all (`sightenv::layer_order` names it below Sight when `ENABLE_GAMESCOPE_WSI=1`).

**Known limits.** Several held raw-input buttons across an open are paired by count, not code.
A game that keeps its own cursor visible and confines the pointer shows it under the panel's
(they coincide on X11; on Wayland the panel follows deltas). Wine's client-side key repeat on
Wayland repeats a key the game held when the panel opened until it's released. hyprpad's mode
follows within its rescan interval.

### The shell, docking and the Mods section (2026-11-19)

Toward the SightMods / SightSession / docked mock-ups (`panel/view.rs`, `panel/hints.rs`,
`panel/sections/`).

- **Shell.** No side list: a tab row on top ("Sight", LB, Session · Mods · Tweaks · Performance ·
  Achievements, RB; on the right the game, profile, clock and battery with "charging" /
  "plugged in"), the section's body in cards, and the hint bar along the bottom. The rows have
  focus from the start; B closes (in Mods it goes back to the surface list first).
- **Hint API** (`panel/hints.rs`): a section adds its hints (`Hints::add([Glyph::X], "Reset to
  default")`, a right-aligned `detail`) from one function that `PanelView::section_hints`
  calls for it; the shell adds B, LB / RB and R3 after them unless the section has its own.
- **Docked** (the docked proposal): Mods and Tweaks open as a 500 px column on the left with no
  blur and no dim, so the game shows unobstructed beside it, the HUD included (unless it's
  anchored on the left); Session, Performance and Achievements stay centred over the blurred
  game. R3 (hyprpad types Tab; Tab on a keyboard) swaps them, remembered per section in
  `sight.toml` (`[docked]`, only what differs from the default). The view tells the controller
  (`Msg::PanelLayout`), which drops the layer's blur request while docked and asks again when
  centred; `quiver sight status` says `layout: docked`. In the game the column is part of the
  same `panel.image`, transparent around it; on a stereo frame each eye gets it, and the lifts
  follow (the column is the card, the focused row inside it). Input capture is unchanged:
  the panel has the keyboard, mouse and pad while it's open, docked or not.
- **Change chip.** Every outcome quiverd reports for a change (Mods or Tweaks) shows a chip
  at the top of the game area beside the column: "Crowd density 1.0× → 2.0× · live, 0.6 s
  ago", "· next launch", "· at next area load", "· not applied: …", fading after 3.5 s. It
  floats at the panel's rung, and gives way to a dialog (four lifts at most).
- **Y: tap pins, hold compares.** Y released within 350 ms pins the focused row to the quick
  menu (built separately; until then it says so). Held on a live option of a surface that
  reaches the game, it flips the option to its value before this session's last change (or
  its default) with `live: only` until Y is let go, then flips it back the same way; nothing
  is saved. A release followed within 60 ms by a press is key repeat, not a new press. gilrs
  pads (no hyprpad) only tap. Tweaks have no session-only set, so they don't compare.
- **Mods** (SightMods): the profile's surfaces on the left (each mod's, the game's own
  settings, runtimes such as DXVK) with where they take effect ("live · SDK connected", "ACO.ini
  · next launch"), and the mods with none ("Skip intro videos has no options"). The picked
  surface: its pages as tabs (LT / RT; hyprpad types Home / End), a preset picker, and its
  option rows: label, badge (LIVE, NEXT AREA LOAD for an `at` moment, NEXT LAUNCH) and help on
  the left, the control on the right (a slider with − / + for numbers with a range, ‹ › for
  choices, a switch for toggles) with the result line under it ("Applied live · DLSS
  confirmed it 0.6 s ago"). On the right: "DLSS right now" (the surface's `state` / `text` /
  gauge metrics from the session's catalog, subscribed with the panel's metrics), "This
  session" (the changes made in game; **Revert this session** is `OptionsRevertSession`;
  **Session only** takes them out of the profile with `live: no` and sets them again with
  `live: only`, and later changes go out `live: only` until it's turned off), and "Locked in
  game" (options the mod marks dangerous: the app changes those). Saved changes that wait for
  the next launch get a Restart bar with Undo (each back to where the session started). X
  resets the focused option (`OptionOp::Reset`), and the hint bar shows its default and range.
  Docked, the same stacks in the column.
- **Tweaks**: "Saving to the DLSS profile", its pages and rows as in Mods, and the Restart bar.
  (The scope picker of the docked mock isn't built: it saves at the running profile's scope.)
- **Keys** (`state::panel_key`, `tools/hyprpad/quiver-sight.lua`): arrows, Enter, Escape as
  before; LB / RB Page Up / Down (and Q / E, Shift+Tab); LT / RT Home / End (and `[` `]`); X
  `x` (and Delete); Y `y` (timed on its key-up); R3 Tab. The snippet's old `h.bind("lstick_…")`
  lines are gone: hyprpad's `h.bind` takes guide chords only, and they stopped the whole
  config from loading.


---

## 15. Stereo: the HUD and the panel in each eye (2026-11-19)

**Why.** Stereo mods present a *packed* frame. Torchbearer (Alan Wake Remastered) does this with
`[stereo] mode = sbs` and `output = full` (a 3840x1080 window, each half a 1:1 eye) or
`output = hsbs` (a window at the desktop size, each half an eye squeezed 2:1). Ishimura (Dead
Space) does it too. The viewer is an XReal Air 2 Ultra under HypXRland, which crops a stereo
window half per eye. Under Wine the Vulkan swapchain is the window's client size and
vkd3d-proton/DXVK blit the game's already-packed buffer into it, so the layer sees the packed
image. Drawn once over the whole swapchain, Sight's HUD landed in one eye, and the panel
straddled the seam with its pointer mapping wrong.

**Layouts** (`quiver_perf::stereo`). The spellings are HypXRland's `stereo:<layout>` (its
docs/openxr/05-configuration.md §8.6), so one string names a layout everywhere: the SDK call,
the setting, and the compositor's window rule. The left eye is the left (or top) half.

| Layout | Frame | Each eye | Overlay laid out at (logical) |
|---|---|---|---|
| `mono` (also `off`) | one view | the frame | the frame |
| `sbs` | 2× wide | a half, 1:1 | half the width × the height |
| `hsbs` | window width | a half, squeezed 2:1 horizontally | the width × the height |
| `tab` | 2× tall | a half, 1:1 | the width × half the height |
| `htab` | window height | a half, squeezed 2:1 vertically | the width × the height |

**Drawing: once per eye, at zero disparity.**
- quiver-sight lays the HUD and the in-game panel out for one eye's *logical* size:
  - the HUD's anchor and offset are resolved in that space;
  - its text size is 1.6% of the eye's height (quiverd's `text_px`);
  - the panel's off-screen window is that size, at the monitor factor of that size.
- The image's descriptor says so: `qv_hud_buffer.stereo` (was padding, so 0 = mono from
  older writers). `x`, `y` and the size are then in eye-logical pixels, and quiver-perf
  validates them against that space. `extent_w/h` stay the swapchain's.
- The layer draws each image once per eye (`compositor::Eyes`):
  - the viewport is the eye's half of the swapchain, and the push constants' extent is the
    eye's logical size, so the same quad, graph vertices and cursor land in both eyes at the
    same place (zero disparity, like the game's own HUD);
  - into a squeezed eye, the quad averages the two texels each fragment covers (a 2:1 box
    filter: `FLAG_SQUEEZE_X/Y`). It used to fetch by `gl_FragCoord`; it now uses an
    interpolated texel coordinate, so it works under any viewport;
  - the scissor is the image's pixels within that eye, so nothing crosses the seam.
- The panel's cursor is in the image's (logical) pixels and is drawn in both eyes, squeezed
  with the rest.
- **Disparity**: built since (design/STEREO-UI.md §12). Each image and each lifted part of it
  (the panel's card, focused row, dialog) gets a per-eye shift from its depth rung, at the
  `sight.depth` strength; `off` is the zero disparity described here.

**The backdrop blur stays in each eye.** The request in `hud.image`'s header carries the
layout (`qv_hud_header.stereo`, written with the request). Every blur pass clamps its taps
to the half of the source that the fragment is in, half a texel inside. Level sizes are
rounded up to even, so the seam (uv 0.5) is a texel edge at every level, and no tap mixes
the two eyes.

**Pointer: in the eye it's over** (`panel/ingame.rs` `Translate`). The input preload is
unchanged. It forwards X11 positions in the game's window, which is the swapchain's size.
quiver-sight maps each position into the eye it's over (`Stereo::to_logical`):
- `sbs`: `x mod` the eye's width;
- `hsbs`: that, ×2;
- `tab` / `htab`: the same, vertically.

A click lands where the panel is in that eye. Relative motion and Wayland deltas stay 1:1 in
logical pixels.

**Captures** stay whole packed frames: the capture is taken before the overlay and isn't
split. A one-eye capture isn't built.

**How the layout is decided** (`stereo::resolve`):
1. **A live mod's word.** `quiver_set_stereo_layout(m, "sbs")` in `quiver_mod.h` (protocol 3)
   sends `{"t":"stereo","layout":"sbs"}`. quiverd keeps it per session, newest connection
   first (`Session::sdk_stereo`), and drops it when that connection goes (bye or pipe
   closed) or the mod withdraws it (`NULL` or `"auto"`). It reaches quiver-sight in the next
   `HudModel` (`HudModel::stereo`: layout, source, detail), so Torchbearer can switch at
   runtime. A change while the panel is open in that game reopens the panel, laid out anew.
2. **Else the `sight.stereo` Tweak** (`auto` | `mono` | `sbs` | `hsbs` | `tab` | `htab`, on
   the Display page; global, game or profile scope like every Tweak; applies at the next
   launch). Its mechanism is the game's environment, `QUIVER_SIGHT_STEREO=<layout>`. The
   layer reads that and reports it in its status block (`qv_layer_status.stereo_setting`), so
   quiverd and a lab's quiver-sight read it the same way. In a lab, set it with
   `quiver lab edit N env.QUIVER_SIGHT_STEREO=sbs`.
3. **Else mono.**

**Precedence: the mod wins while it's live.** This is the opposite of HypXRland's rule for
the window tag, where an explicit rule beats the client's tag. Content metadata there is
untrusted, and the last human instruction must win. Here the declaration comes from code the
person installed. It knows its packing frame by frame: Torchbearer switches it at runtime,
and with stereo off but packed output on it still says `sbs`/`hsbs`. The setting is the
fallback for mods without the SDK (Ishimura today). A setting that should override a
misbehaving mod would be a new value (e.g. `force:<layout>`); it isn't built.

**Status.**
- `quiver hud status`: `stereo: sbs (mod: torchbearer (SDK); setting auto)`.
- `quiver sight status`: a `stereo:` line per session.
- `quiver lab sight N status`: the layout, the size the overlay is laid out at, and how to
  set it.
- The layer's status block also has `stereo_drawn`, the layout of the last image it drew.
  `quiver hud status` says when that differs from the resolved layout (a quiver-sight still
  on the old layout).
- The panel's `drawn:` line says "in each eye (sbs, laid out at 1920x1080)".

**Wire and layouts** (all compatible: zero means mono or unset):
- `qv_hud_buffer.stereo` at offset 172 (was `_pad0`);
- `qv_hud_header.stereo` at 144;
- `qv_layer_status.stereo_setting` and `stereo_drawn`, appended (the struct is 408 bytes);
- `QV_STEREO_*` in `quiver_perf.h`;
- the SDK's `quiver_mod.h 3.0` with `QUIVER_MOD_PROTOCOL 3`, and `quiver-proto`'s `PROTOCOL = 3`;
- `ModSdkConnection::stereo`, `HudModel::stereo`, `HudStatus::stereo` and
  `PanelKeys::stereo`, appended and optional.

An older quiverd ignores the `stereo` message. An older layer draws a stereo image once,
across the frame. That is transient skew until the game restarts.

**Verification.**
- Unit tests:
  - the layouts' geometry, pointer mapping both ways, scissors, settings and precedence
    (`quiver_perf::stereo`);
  - descriptor validation in eye space and the request's layout (`hud::tests`);
  - the status block's zero-extension;
  - the layer's `Eyes` per layout;
  - the blur's even levels and splits;
  - the panel's pointer per layout (`ingame::tests::stereo_pointer`);
  - the lab model's text size;
  - the SDK message parse and the registry (`stereo_layout_from_the_sdk`: set, the same
    layout again, a refusal, a withdrawal);
  - quiverd's resolution (`stereo_from_the_setting_and_a_live_mod`: the setting, the mod
    winning live, another connection's withdrawal, a disconnect back to the setting);
  - the Tweak's plan (`QUIVER_SIGHT_STEREO`, nothing for auto).
- GPU, opt-in: `tests/vulkan_stereo.rs` (headless RADV) reads back a panel drawn through the
  real layer:
  - sbs: the block and cursor in both eyes at the same place;
  - hsbs: squeezed into both 320-px eyes, one-pixel stripes averaged to grey, the cursor in
    both;
  - the status block's setting and `stereo_drawn`.
- The SDK compiles with MinGW as C99 (32 and 64-bit) and C++. `modsdk_wine.rs` checks the
  call through the real bridge.
- **Live, in the lab** (Alan Wake Remastered with Torchbearer 0.4.1, built from 66e0c66's tree:
  `[stereo] mode = sbs`, `[game] display_mode = borderless`, `[dev] start_level = 15`,
  `skip_videos = 1`, `env.QUIVER_SIGHT_STEREO` set in the instance). In both runs below:
  - Shift+F3 put the HUD in both eyes at the same place;
  - Shift+F2 put the panel in both eyes, over a blur that stops at the seam;
  - `Down` moved the selection in both eyes;
  - a click on "Performance" in the left eye and on "Achievements" in the right eye each
    selected that section, the cursor showing in both eyes;
  - Escape gave the input back.

  The two runs:
  - `output = full` on a 3840x1080 instance: eyes 1920x1080, the overlay laid out at
    1920x1080.
  - `output = hsbs` on a 1920x1080 instance: eyes 960x1080 squeezed, the overlay laid out at
    1920x1080 and squeezed with the game.

  The lab had no mod channel then, so these runs used the setting, not the SDK call. It has
  one now (design/LAB.md §14): a lab mod's word reaches the lab's quiver-sight through the
  instance's own read-only endpoint.

**Not verified.**
- The SDK path with a real mod, live: Torchbearer doesn't call `quiver_set_stereo_layout`
  yet, and lab sessions refuse mod channels. The C side ran through the real bridge under
  Wine (`modsdk_wine`), and the quiverd side ran in unit tests.
- On the glasses under HypXRland: whether a stereo window's per-eye crop shows the overlay
  fused.
- `tab` / `htab` with a real producer (GPU and unit tests only).
- Winewayland (pointer deltas only, as before).
- The panel re-laying itself out when the layout changes while it's open.
- 32-bit games.


---

## 16. The quick menu, toasts, onboarding (2026-11-19)

Built to the mock-ups (`SightQuick.dc.html`, `Main.dc.html`). Code: `quiver-sight/src/quick.rs`,
`toasts.rs`, `overlay.rs`, `screenshot.rs`, `glue.rs` (how they sit in `Sight`); quiverd's
`notices.rs`; `quiver-api` `sight.rs` (notices, settings, pins).

### Drawing over the game without taking its input

The panel's in-game path (§14) captured input whenever it drew. Toasts, the status chip and the
first-launch hint must not. `panel.input` gained a state bit, `PANEL_SHOW` (4): the layer draws
`panel.image` but nothing is captured. The preload's capture rule still asks for `PANEL_OPEN`
only, so a preload from before reads it as closed. The layer says it draws such images with
`PANEL_LAYER_CAN_SHOW` in its word; with an older layer, toasts stay on their layer-shell surface.
The same `panel.image` serves the overlay, the quick menu and the panel, one at a time: the panel
or the quick menu takes it over (the overlay closes first; each open bumps the epoch, and a view
whose epoch was superseded lets go without touching the state). Toasts that come while the panel
is open in the game go to the desktop surface.

`panel::ingame::InGame` is generic over an `InGameView` (the panel, `QuickView`, `OverlayView`):
each gives its depth parts and its focus handle. `InGame::show` is the no-capture variant.

Stereo: everything is laid out for one eye and drawn in each, like the panel. Depth rungs
(design/STEREO-UI.md §12): the toast stack and the hint at `PANEL`, the chip at `HUD`, the
quick menu's strip at `PANEL` and its focused tile at `FOCUS`.

### Toast kinds

| Kind | From | Notes |
|---|---|---|
| Achievement unlocked | `Event::AchievementUnlocked` | as before, now with its session |
| Mod setting | `OptionsOutcome` (not Sight's own) | as before |
| Saved to the cloud | quiverd notice (`NoticeKind::CloudSaved`) after a session's upload: Ubisoft (`LaunchEvent::SavesUploaded`), Epic (`epicgame::end`) | the game has exited, so it shows on the desktop; Steam syncs its own saves (no event) |
| A mod's hotkey | quiverd notice (`ModHotkey`) from an SDK `changed` with `persist: false` | "Frame Generation off · You pressed [F9] · this session only": the option's label and value from the schema, the key from an action sharing the option's first id segment (`fg.toggle` ↔ `fg.enabled`) with a `hotkey_option` |
| Agent or mod notice | `SightRequest::Notify`, `quiver sight notify`, SDK `{"t":"notice"}` | caption "Agent · name" or the mod's surface; up to three buttons |
| Download finished / failed | `Event::Job` (install jobs: installs, updates, repairs) | each job once; a failure is an error |
| Errors | any of the above at error level | shown in do-not-disturb |

quiverd's options sink is now wired (`notices::ModSink`): a non-persisted `changed` goes through
`options_from_mod` (recorded as what the game runs, `OptionsLive` with `saved: false`) and becomes
the hotkey notice. Persisted changes (a mod's own menu) are still only logged.

**Do not disturb** (`toasts.dnd`): only urgent toasts (errors, rejections, failed downloads)
show. **`achievements.on_unlock`** = `toast` (default) | `toast_screenshot`: the screenshot is
taken first (so the toast isn't in it), then the toast shows.

### The notice API

```
Request::Sight(SightRequest::Notify { notice: NoticeSpec { kind, level, caption?, title, body?,
    key?, session?, buttons: [{id, label, primary}], secs?, tag? } })      → SightNotice { id: "n-7", from, at, … }
Request::Sight(SightRequest::NoticeAct { id, button })                    → Event::SightNoticeAction { id, button, from, tag, by }
SDK: mod → {"t":"notice","id":"dlss4","title":…,"body":…,"level":…,"buttons":[{"id":"keep_v3","label":"Keep v3 for now"},…]}
     ← {"t":"notice_action","id":"dlss4","button":"keep_v3"}
```

quiverd checks and trims (title ≤ 80, body ≤ 240, ≤ 3 buttons with distinct `[A-Za-z0-9._-]`
ids), captions agents' and mods', resolves the session (named, else the focused or only game;
none running: refused), keeps the last 64 for their answers, starts quiver-sight if needed, and
limits each poster to six a minute (the registry also allows a mod one a second). `{key}` in a
body is where the key shows as a keycap. No SDK protocol bump: unknown `t` is ignored both ways.
The C header has no `quiver_notice()` yet.

Buttons are answered from the **quick menu**: while a notice with buttons is up, its buttons are
the first tiles. `quiver sight answer ID BUTTON` does the same.

### The quick menu

Hold Shift+F2 in the game. The layer counts the press as before (quiverd sends the panel toggle,
`by: "keys"`) and now also writes the chord's state into `panel.input`'s layer word (offsets 232
and 240: held, the press count, when it changed; `PANEL_LAYER_CHORD` says it does), from both
its X11 keymap polling and its Wayland keyboard (`keys::Chords::down`). quiver-sight, on a keys
toggle with the panel closed and the game able to draw it, waits (`quick::Quick`, polled every
25 ms): released within 280 ms, a tap, the panel opens as before; still held, the quick menu
shows. Left / Right choose, releasing applies the focused tile and closes, Escape / B closes.
`quiver sight quick` opens it latched (A / Enter applies). A layer from before (no chord word),
or a game it can't be drawn in: the panel, as a tap would. There's no desktop fallback.

Tiles: notices' buttons; frame generation (a mod's live, adjustable `fg.enabled`, with its key);
the HUD preset (cycles the presets, shows the HUD if hidden); the frame-rate cap (`fps.cap`
through No cap, 30, 60, 90, 120, 144, refresh − 3); pins; screenshot (`grim` of the game's
monitor into `$OMARCHY_SCREENSHOT_DIR`, else `$XDG_PICTURES_DIR`, else `~/Pictures`, 250 ms
after the menu leaves the frames); save replay (only while a `gpu-screen-recorder -r` replay
buffer runs: SIGUSR1); quit (A held 1 s; released on it, the menu latches: Hyprland closes the
game's window, else quiverd stops it). Applying says what it did in a short toast. The menu writes
the `sight-panel` state file while up, so hyprpad's `quiver-sight` mode holds the pad.

**Pins** ("pin to quick menu": Y tapped on a Mods or Tweaks row, `PanelView::pin` →
`quick::pin_row`): per game and profile in Sight's settings (`pins."ubisoft:3539/dlss"` and
`pins."ubisoft:3539"`; the quick menu shows the profile's, then the game's; eight per scope). Y
on a pinned row unpins it. `SightRequest::Pin { scope, pin, on }`, `quiver sight pin GAME ID
--surface S | --tweak [--profile P] [--off]`.

**hyprpad** (`tools/hyprpad/quiver-sight.lua`, option (a)): a key on a guide chord is held for
as long as the chord is, so `h.bind("guide+menu", …, h.key "shift+f2"):only_in("game",
"quiver-sight")` makes Guide+Start the keyboard's gesture, tap and hold; `guide+dpad_left/right`
type the arrows under the guide. One binding per guide chord means this replaces an Omarchy-menu
bind on Guide+Start; option (b), the mode-branching script, stays tap-only. Checked with `hyprpad
bindings` against a scratch config.

### The status chip and the first-launch hint

The chip ("Sight · 118 fps · 21:14 · 1 h 12 m", top-left, 4 s) shows when the panel or the quick
menu closes if `status_chip.enabled` (off by default), and on `quiver sight chip`. FPS is the
last the panel's metrics subscription saw (absent if the panel hasn't been open this session);
the play time is the session's. In the game only.

The hint ("Open Sight: Shift+F2 or Guide+Start · shown on the first launch only", bottom centre,
9 s) shows once per install, when the first game's layer can draw (or after 20 s as a desktop
toast where it can't), then quiver-sight sets `hint.shown`. `quiver sight hint --reset` shows it
again.

### Sight settings

`$XDG_DATA_HOME/quiver/sight.toml` (`quiver_api::sight::SightSettings`), owned by quiverd:
`SightRequest::Settings`, `SettingsSet { key, value }`, `Pin`; each change goes out as
`Event::SightSettingsChanged`, which quiver-sight follows. Keys: `toasts.dnd`,
`achievements.on_unlock` (`toast` | `toast_screenshot`), `status_chip.enabled`, `hint.shown`,
`pad_chord`, and `pins`. Every writer keeps keys it doesn't know (the CLI's `pad-chord` writes
the file directly, as before). CLI: `quiver sight settings [KEY [VALUE]]`.

### Verification

- Unit: the quick menu's state machine (tap, hold, release applies, Back, Quit held, latched,
  stale chord word), its tiles and what they do, caps, replay detection, pinning rows
  (`quick::tests`); every toast kind and do-not-disturb (`toasts::tests`); the chip
  (`overlay::tests`); screenshot names; `panel.input`'s show bit and chord word
  (`quiver-perf panel::tests`); the layer's held chord (`keys::tests`); the SDK notice messages
  (`quiver-proto`) and their round trip through the registry (`modsdk::tests::
  notices_and_their_answers`); quiverd's rate, checks, captions, answers, the hotkey notice and
  the settings file (`notices::tests`).
- PNGs: `cargo run -p quiver-sight --example sight_png -- OUT` (the quick menu held, with a
  notice, latched on Quit; the overlay with toasts, chip and hint; every toast kind).

Not verified: anything live in a game (the layer's chord word, the in-game overlay, the quick
menu's capture, stereo), hyprpad's held key reaching the game, the cloud-save and hotkey notices
with a real upload or mod, grim from quiver-sight's environment, gpu-screen-recorder's replay.
