# Sight HUD: a pinned performance HUD and pluggable metrics (design notes)

Status: design only (2026-11-16). Nothing here is built. Extends `design/SIGHT.md` (the in-game
overlay) and `design/MOD-CONFIG.md` (mod options and the `quiver_mod.h` SDK). It **supersedes
SIGHT steer 7**: Quiver gets its own pinned performance HUD; MangoHud stays available as an
option per game.

The user's direction, which this document works out:

> If we're going to build our own performance overlay, then: (1) build a compact one that can be
> pinned on top of the game (as an alternative to MangoHud); (2) make it pluggable for mods, e.g.
> our DLSS mod should be able to provide "real frame" timings to have a separate base FPS from
> the presented FPS, as well as latency numbers for Reflex, etc. Just make it possible for mods
> to get perf info to the viewer so that it can be rendered.

**Names** (new ones; SIGHT.md's names still apply):

| Name | What it is |
|---|---|
| **HUD** | the pinned, always-on performance readout over the game (replaces SIGHT's "corner HUD" for perf; the clock/battery corner HUD becomes a HUD preset) |
| **metric** | one named, typed, described stream of values (`fps.base`, `gpu.temp`, `aco-dlss.dlss.ms`) |
| **producer** | something that publishes metrics: the Sight layer, quiverd's sensors, a mod through the SDK, the bridge, the compositor |
| **consumer** | something that reads them: the HUD, Sight's Performance panel, the app, the CLI, agents, recordings |
| **descriptor** | a metric's static description: kind, unit, labels, format, thresholds, display hints, roles |
| **role** | a well-known slot a producer's metric can fill (`frame.base`, `latency.frame`), so layouts work for any mod that provides it |
| **session dir** | `/dev/shm/quiver-<uid>/<session>/`: the rendezvous where rings and the HUD frame live |
| **ring** | an append-only shared-memory log of per-frame or per-event records, one writer process |
| **HUD frame** | what quiverd hands a presenter: formatted cells, styles and live bindings, not pixels |
| **presenter** | as in SIGHT: who puts pixels on screen. Here: the **game presenter** (the Sight layer draws into the swapchain) or the **desktop presenter** (a layer-shell surface from `quiver-sight`) |

---

## 0. Summary

- **Where it draws.** The pinned HUD is drawn **inside the game by the Sight Vulkan layer** by
  default (a narrow, HUD-only slice of SIGHT's phase 3, without the dma-buf compositing), with a
  **compositor (layer-shell) fallback chosen automatically** when the layer isn't active
  (OpenGL/native games, anti-cheat-flagged games, layer failure). Reason, verified in
  Hyprland's source: any mapped surface on the `overlay` layer, or any visible `top` layer
  surface, on the game's monitor blocks the "solitary client", which turns off both direct
  scanout and tearing; a HUD on the desktop side therefore costs exactly what a performance HUD
  is used to measure. VRR (modes 2 and 3) survives, but every HUD commit adds an off-cadence
  compositor flip.
- **Metrics model.** Producers declare descriptors (namespaced id, kind, unit, labels, format,
  thresholds, display hints, roles) once; values flow either as **JSON over the existing
  channels** (≤ 10 Hz: gauges, states, counters, text) or through **shared-memory rings** in a
  per-session `/dev/shm` directory (per-frame and per-event records). quiverd is the hub:
  registry, alignment of clocks and frames, aggregation (windows, percentiles, 1% lows),
  derived metrics, subscriptions, the HUD composer and recordings. Consumers render from
  descriptors with no per-metric code.
- **Wine shared memory works with a real file.** A PE DLL calling `CreateFileMappingW` on a
  file it opened under `Z:\dev\shm\…` gets a `MAP_SHARED` mapping of the same Unix file that
  Linux processes `mmap` (verified in Wine 10.0 / GE-Proton10 source); pagefile-backed sections
  are anonymous memfds and can't be shared by name. QPC under Wine is `CLOCK_MONOTONIC_RAW` in
  100 ns ticks (verified), so mod and layer timestamps share a clock.
- **Mods** get a handful of `quiver_mod.h` calls: declare metrics (or ship them in
  `quiver-options.toml`), set gauges/states, submit per-frame records and markers. Lock-free,
  allocation-free and syscall-free on the hot path; no-ops when Quiver isn't there. MOD-CONFIG's
  "readouts" become metrics of kind `state`/`text`.
- **MVP:** in-layer HUD with four presets, present timing from the layer, system sensors from
  quiverd, mod metrics over the SDK pipe (JSON), a per-session performance summary, `quiver perf
  watch`. Mod rings, frame correlation, Reflex breakdowns, recordings and comparisons follow.

---

## 1. The pinned HUD

### 1.1 What it shows: presets

Four built-in presets plus Custom. Every preset is just a layout (section 1.4) over metric ids and
roles, so mod metrics appear in presets without special cases: a preset row that references a
role nobody provides is dropped, not shown empty.

**FPS only.** One number, top-left, small. With a frame-generation provider active it shows base
and presented:

```
158                                       (no FG provider, or FG off)
79 → 158                                  (FG on: base → presented)
```

**Compact** (one line, the MangoHud "horizontal" equivalent):

```
FPS 79→158  12.7ms  1%↓ 64  │ GPU 97% 71° 92W  │ CPU 38% 64°  │ DLSS K 1280×800 · FG 2× · LAT 21ms
```

**Standard** (a small block with a frame-time graph):

```
┌──────────────────────────────────────────────┐
│ FPS   79 → 158        1% low  64 → 128       │
│ FT    12.7 ms base    6.3 ms presented       │
│ ▁▁▂▁▁▁▃▁▁▁▁▁▂▁▁▁▁▇▁▁▁▁▁▁▂▁▁▁▁▁▁▁▁▂▁▁▁▁▁▁▁▁   │  presented frame times, 3 s; generated frames muted
│ GPU   97%  71°C  92 W  6.1/8 GB              │
│ CPU   38%  64°C        RAM 11.2 GB           │
│ DLSS  K · 1280×800 → 2560×1600 · 1.6 ms      │
│ LAT   21 ms (Reflex on)                      │
└──────────────────────────────────────────────┘
```

**Full** adds clocks, per-process RAM and VRAM, battery and AC state, disk and network I/O, the
latency breakdown as a stacked bar (simulation, render submit, queue, GPU, present), session time,
profile and mod versions, Proton/DXVK/driver versions, present mode, and the display path
(`scanout` / `composited`, VRR on/off, tearing) from Hyprland's monitor state:

```
LAT ▕███▌▓▓▓▓▒▒▒▒▒▒░░▏ 21 ms   sim 4.1 · submit 2.0 · queue 5.6 · GPU 7.2 · present 2.1
DISP 2560×1600@165 VRR · FIFO · composited (blocked: overlays)
```

The "composited (blocked: …)" line is useful on its own: it tells the user when something on
their desktop (a notification, a PiP, Quiver's own toast) is costing them direct scanout.

**Custom**: any of the above edited in Sight's Performance section, or written by hand.

The clock / session timer / battery strip that SIGHT called the corner HUD is a fifth built-in
layout (`clock`) using the same machinery; it has no performance metrics.

### 1.2 Placement, opacity, scale

| Setting | Values | Default |
|---|---|---|
| `anchor` | `top-left`, `top`, `top-right`, `left`, `right`, `bottom-left`, `bottom`, `bottom-right` | `top-left` (toasts own the top-right, SIGHT §9) |
| `offset` | logical px from the anchor (x, y) | 12, 12 |
| `scale` | 0.75–2.5 × the base size | 1.0 |
| `base size` | text height = 1.6% of the swapchain height (≈ 17 px at 1080p, 26 px at 1600p) | |
| `background` | opacity 0–100%, rounded panel or none | 45% `ink-0` |
| `text` | opacity 50–100%; colour by threshold level on/off | 100%, on |
| `orientation` | `row` (compact) / `block` | per preset |
| `graph` | span 1–10 s, height in rows | 3 s, 2 rows |
| `safe area` | extra margin for TVs (percent of the frame) | 0% (1.5% in TV mode) |

Colours are theme **levels**, never raw colours: `normal`, `dim`, `good`, `warn`, `bad`,
`accent`, mapped to the Omarchy theme tokens the app uses (shared theme crate, SIGHT §4.3). A
mod can say "this value is bad above 50 ms"; it can't pick hot pink.

Sizes are relative to the swapchain, not the output, in the game presenter: the HUD is part of
the game's image. If the game renders at a lower resolution than the output and the compositor
scales it up (XWayland's emulated modes), the HUD scales with it and is softer; the desktop
presenter doesn't have that problem. Noted, not solved (it's the same for MangoHud).

### 1.3 Keys

The HUD needs one toggle and one preset cycle. Constraints:

| Owner | Binding | Note |
|---|---|---|
| MangoHud | Shift_R+F12 toggle HUD, Shift_R+F11 position, Shift_R+F10 preset, Shift_R+F9 reset metrics, Shift_L+F1 FPS limit, Shift_L+F2 logging, Shift_L+F4 reload | README defaults; Hyprland binds can't tell Shift_L from Shift_R, so all of Shift+F1, F2, F4, F9–F12 collide |
| Sight panel | Shift+F2 (SIGHT steer 4, Connect parity) | already collides with MangoHud's logging |
| aco-dlss | F9 (FG), F10 (DLSS ↔ TAA) | bare keys |
| Steam | Shift+Tab overlay, F12 screenshot | Steam copies |
| ReShade | Home | |
| Omarchy | Ctrl+F1, Ctrl+F2, Shift+Ctrl+F2, Super+Ctrl+F12 | desktop |

Proposal (steer 4):

- **Shift+F3: toggle the HUD** (off ↔ the game's last preset). **Ctrl+Shift+F3: next preset.**
  Free of every default above, next to Sight's Shift+F2.
- Bound the way SIGHT §6 binds the panel: a Hyprland bind added while a Quiver game window is
  focused and removed otherwise, calling `quiver hud toggle` / `quiver hud next`.
- Controller: a hyprpad snippet (`guide+select` → `quiver hud toggle`), never bare Guide.
- **When Quiver launches MangoHud** for a game (it owns `MANGOHUD_CONFIG` for that launch, via
  the MangoHud runtime surface in MOD-CONFIG), it moves MangoHud's `toggle_logging` off
  Shift_L+F2 (to Shift_L+F7) so Sight's panel chord and MangoHud logging don't fight. Spike H7
  checks whether a Hyprland bind already hides the key from MangoHud's in-process key polling
  under XWayland; the remap is needed either way for Wine-Wayland games.
- The HUD registers its keys as a runtime surface (MOD-CONFIG C7), so `options keys` shows them
  and flags conflicts with mods' keys.

### 1.4 Layouts and scopes

A layout is data. Built-in presets are layouts shipped with Quiver. The user's layouts resolve
**profile > game > global**, so "DLSS profile shows base vs presented and latency; vanilla shows
compact" is two lines of configuration:

```toml
# ~/.local/share/quiver/hud/layouts/dlss-tuning.toml  (a user layout)
format = 1
name = "DLSS tuning"
anchor = "top-left"
background = 0.45

[[row]]
cells = [
  { metric = "fps.base", stat = "mean", short = "FPS" },
  { text = "→" },
  { metric = "fps", stat = "mean" },
  { metric = "fps.base", stat = "p1low", label = "1% low" },
]

[[row]]
cells = [{ graph = "frametime", series = ["frametime", "frametime.base"], span = "3s" }]

[[row]]
cells = [
  { metric = "aco-dlss.upscaler" },            # a mod's own metric, by id
  { metric = "render.resolution" },            # a role: whoever provides it
  { metric = "aco-dlss.dlss.ms", stat = "mean" },
  { metric = "latency.render", stat = "mean", short = "LAT" },
  { metric = "fg.state", display = "badge" },
]
```

Selection (stored with revisions like game settings, in quiverd's config):

```
hud.global   = { layout = "compact", on = false }
hud.game.<product>             = { layout = "standard" }
hud.profile.<product>/<profile> = { layout = "dlss-tuning", on = true }
```

`on` is the default state at launch; the toggle chord changes it for the session only (and is
remembered as "last state" per game if steer 12 says so).

### 1.5 Update rates

| What | Rate | Where computed |
|---|---|---|
| Presented FPS number, frame time number | 2 Hz (MangoHud's `fps_sampling_period` default is 500 ms); 1, 4, 10 Hz selectable | the layer, from its own presents (zero lag, no IPC) |
| Frame-time graph | every present | the layer, from its own presents |
| Base FPS, 1% lows, latency, mod gauges | 2 Hz (window 1 s) | quiverd, sent in the HUD frame |
| System sensors (GPU/CPU load, temps, power, memory) | 1 Hz | quiverd |
| Battery, disk, network | 0.5–1 Hz | quiverd |
| Display path (scanout, VRR, tearing) | on Hyprland events | quiverd |
| Mod gauges and states | as published, coalesced to ≤ 10 Hz | SDK → quiverd |

The game presenter redraws every present regardless (the image is new every frame), but
rebuilds its vertex data only when the HUD frame or a live value changes.

### 1.6 Picking metrics

Sight's Performance section has a **metric picker** over the session's **catalog** (section
3.11): canonical metrics first (FPS, base FPS, frame time, 1% low, latency, GPU, CPU, memory),
then each producer's own metrics under the producer's name ("DLSS mod", "System", "Display").
Each entry shows its label, unit, a live sample, the producer and a one-line help. Picking adds
a cell to the current row; the cell editor offers the stat (`last`, `mean`, `min`, `max`, `p1low`,
`p99`), the display (`number`, `badge`, `bar`, `sparkline`, `graph`) and a short label. Gamepad
works the way MOD-CONFIG's forms do (A adds, D-pad reorders, Y removes).

Mod metrics are listed **even when the game isn't running**, because the descriptors ship in the
mod version's `quiver-options.toml` (section 3.3). The app's game page has the same layout
editor, so a HUD can be set up before launching.

CLI: `quiver hud layout show|set|edit`, `quiver hud catalog origins` (the same catalog as text).

### 1.7 MangoHud

MangoHud stays a supported option per game (`wrappers=mangohud` or `MANGOHUD=1`, MOD-CONFIG's
runtime surface for its config). Quiver's HUD is the default; if both are enabled Sight shows a
one-time hint, and the HUD toggle chord doesn't touch MangoHud. Reasons to keep it: OpenGL games
(MangoHud also hooks GL), gamescope sessions via `mangoapp`, its logging/upload ecosystem, users'
existing presets. `quiver perf export --mangohud` writes MangoHud-compatible CSV so tools that
read MangoHud logs (FlightlessMango's log viewer, spreadsheets) keep working.

---

## 2. Where it draws

### 2.1 What the compositor does with a pinned surface (verified in source)

Source: the user's Hyprland fork (`~/code/hypxrland`, HypXRland on Hyprland 0.56/0.57), files
`src/output/Monitor.cpp` and `src/render/Renderer.cpp`; consistent with the earlier hyprsc research
(`~/code/hyprsc/docs/research/gamescope-hyprland-integration.md` §4, against v0.56.2).

- **Direct scanout and tearing both require a "solitary client"** on the monitor
  (`isDSBlocked` → `DS_BLOCK_CANDIDATE`; `isTearingBlocked` → `TC_CANDIDATE`).
- `isSolitaryBlocked` refuses a solitary client when, among others:
  - `!m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY].empty()`: **any surface on the
    overlay layer of that monitor, regardless of size, opacity or content** (`SC_OVERLAYS`);
  - any `top` layer surface whose fade alpha isn't 0 (`SC_OVERLAYS`);
  - a floating window allowed over the fullscreen one (`SC_FLOAT`), so a pinned float is no
    better;
  - a Hyprland notification (`SC_NOTIFICATION`), the fullscreen window not being opaque or not
    covering the monitor, the window being maximized rather than fullscreen (`SC_WINDOWED`).
- **VRR is independent of the solitary gate**: `ensureVRR` for `misc:vrr` 2 and 3 checks only
  that the workspace has a real fullscreen window (and content type game/video for 3). A HUD
  surface doesn't turn VRR off.
- But a layer surface that commits (a value changed) damages the output and schedules a
  compositor frame of its own. With VRR, that flip lands wherever the HUD update happened, not on
  the game's cadence; the game's next frame may then wait for the panel's minimum refresh
  interval. At 2 Hz number updates that is two off-cadence flips per second; a desktop-side
  frame-time graph updating at 30–60 Hz would be 30–60 (inferred from the scheduling code;
  spike H1 measures it).
- `hyprctl monitors -j` reports `solitaryBlockedBy`, `directScanoutBlockedBy` and
  `tearingBlockedBy`, which makes all of this measurable (and gives the Full preset its
  display-path line).

The user's current configuration, read-only: `misc:vrr = 3`, `cursor:no_break_fs_vrr = 2`,
Omarchy's default `general:allow_tearing = false`, and `render:direct_scanout` unset (default 0,
off). **So today, on this machine, a desktop-side HUD costs no scanout and no tearing (both are
already off), only the extra flips.** That changes as soon as the user enables
`direct_scanout = 2` (auto for game content) or an `immediate` window rule for a competitive game,
and it's the wrong default to design around: a performance HUD shouldn't be the thing that
blocks the fastest display path. (On hybrid laptops where the panel hangs off the iGPU and the
game renders on the dGPU, direct scanout of the game's buffer is often impossible anyway; the
tearing argument still holds.)

Other consequences of the desktop presenter: game-only captures (portal window capture, OBS
vkcapture) don't include it, which is the opposite of what people expect from a MangoHud
alternative (benchmark videos show the HUD); windowed games need the surface moved with the
window (layer-shell anchors to outputs); it needs GPUI with layer-shell (SIGHT spike S2).

### 2.2 What MangoHud does

MangoHud is an implicit Vulkan layer (plus an `LD_PRELOAD` GL hook). At `vkQueuePresentKHR` it
records a Dear ImGui draw into the swapchain image with its own render pass (load existing
contents), waits on the application's present semaphores, signals its own, and passes the
present down with its semaphore in place of the app's. It adds `COLOR_ATTACHMENT` usage at
swapchain creation when missing, keeps a font atlas texture built from an embedded font, reads its
config from `MANGOHUD_CONFIG` / config files (reloaded on a key or file change), polls hotkeys
in-process (X11 key state), exposes a `control=` Unix socket (`mangohudctl`), and does FPS
limiting by sleeping in the present path. Under gamescope it can't draw in-layer and uses
`mangoapp`, a separate client that gamescope composites (`--mangoapp`).

Under Proton every D3D9–12 game ends in DXVK or vkd3d-proton, so a present-time layer sees every
frame the player sees, including frames a mod generates (aco-dlss presents generated and real
frames through the game's DXGI swapchain, both of which become `vkQueuePresentKHR` calls).

### 2.3 Frame generation, Reflex, anti-cheat

- **FG.** A HUD drawn into a frame that is then fed to an interpolator gets interpolated:
  in generated frames its text is warped by the game's motion vectors and shimmers. That's what
  happens to anything drawn in-process before the mod's DLSS-G pass (a DXGI `Present` hook, for
  instance). The layer sits at `vkQueuePresentKHR`, after DXVK and after the mod's FG, and draws
  on every present, real and generated, with the same pixels: crisp. Order matters with other
  layers: **Sight's layer must be below (closer to the driver than) any frame-generation or
  post-processing layer** (lsfg-vk, vkBasalt, a driver present layer), or it would be
  interpolated or post-processed. The launcher controls the order via `VK_INSTANCE_LAYERS`;
  spike H2 checks the loader's actual chain (`VK_LOADER_DEBUG=layer`).
- **Reflex.** Drawing adds GPU work on the present queue right before present. With
  `VK_NV_low_latency2` in use, every `vkQueueSubmit` should carry
  `VkLatencySubmissionPresentIdNV` so the driver attributes the work to a frame: the HUD's submit
  copies the present id from the `VkPresentIdKHR` in the app's present (DXVK passes its frame id
  there) and chains it. No sleeps, no waits, no fences on the hot path; `pNext` passed through
  untouched.
- **Anti-cheat.** Drawing doesn't change the layer's exposure category: the same `.so` is
  already in the game process for telemetry, and it's Linux code outside the Windows address
  space (MangoHud-class). The per-game kill switch (SIGHT steer 13) still applies, and those
  games get the desktop presenter.

### 2.4 Options

| | Game presenter (in-layer) | Desktop presenter (layer-shell) | Both, automatic |
|---|---|---|---|
| Direct scanout, tearing | kept | **lost while shown** (any overlay-layer surface) | kept whenever the layer is active |
| VRR cadence | untouched (part of the game's frame) | extra off-cadence flips per HUD update | untouched when in-layer |
| FG frames | drawn on real and generated, after FG | n/a (outside the game) | |
| GPU cost | one small render pass per present (tens of µs, inferred from MangoHud-class layers; spike H2) | compositor composition every frame while visible | |
| In game captures/recordings | yes (like MangoHud) | only in output captures | |
| OpenGL / native games | no (Vulkan only) | yes | yes (fallback) |
| Anti-cheat-flagged games | layer off: no | yes | yes (fallback) |
| gamescope as session compositor | yes (it's in the game image) | no | yes |
| Windowed games | follows the swapchain | must track the window | |
| Crash exposure | renderer bug = game crash | none | |
| Code | a small renderer in the layer | GPUI layer-shell (shared with Sight) | both, one HUD frame format |

### 2.5 Recommendation

**Both presenters behind one HUD frame format, with an automatic choice that prefers the game
presenter.** Pull forward from SIGHT's phase 3 only what the HUD needs: a small fixed-function
renderer in the Sight layer (no dma-buf import, no GPUI offscreen, no input). Toasts and the
panel stay on the desktop presenter as SIGHT designed (they're transient or interactive; losing
scanout for the six seconds a toast is up is acceptable).

Choice per session (`hud.presenter = auto | game | desktop`, default `auto`):

1. `auto` uses the **game presenter** when the session's layer has registered a swapchain in the
   session dir and has drawn successfully (it reports a heartbeat and a "drawn" counter).
2. Otherwise the **desktop presenter** (`quiver-sight` maps a click-through layer-shell surface
   with an empty input region, namespace `quiver-sight-hud`, on the game's monitor), and the Full
   preset's display line says "HUD on desktop: scanout off".
3. If the layer stops drawing (error, device lost, swapchain format it can't handle), quiverd
   switches to the desktop presenter within a second and logs why.

### 2.6 Rendering inside the layer

The layer stays dumb about metrics, formatting, fonts and configuration. It renders cells.

**What it receives.** A **HUD frame** that quiverd writes into `<session dir>/hud.frame`
(double-buffered under a sequence counter, section 3.8): anchor, offset, scale, opacity, theme
level colours (resolved RGBA), and rows of cells. A cell is pre-formatted text plus a level, or a
**live cell** the layer fills itself:

| Cell | Content | Filled by |
|---|---|---|
| `text` | UTF-8 string, level | quiverd |
| `badge` | text in a pill | quiverd |
| `bar` | value 0–1, level; or a stack of segments with levels | quiverd |
| `spark` | up to 120 points, normalized | quiverd |
| `live.fps`, `live.frametime` | number, format | the layer, from its own presents |
| `live.graph` | frame-time graph from the layer's presents, optional second series and per-frame tint from a mod's frame ring (generated frames muted, v2) | the layer |

**Font.** quiverd (or `quiver-sight`) rasterizes a glyph atlas with the user's monospace font
(the Omarchy font, via the app's text stack) at the pixel size the layer asks for (the layer
reports its swapchain extents in its ring header), as an R8 bitmap plus a glyph table, into
`<session dir>/hud.atlas`. The layer uploads it once per change. No font parsing in the game
process. The HUD glyph set is fixed (printable ASCII, Latin-1, arrows, box and block elements,
°, µ, ×, ↓, →); anything else in a string renders as a replacement box.

**Renderer.** One graphics pipeline (premultiplied alpha, two pre-compiled SPIR-V shaders:
vertex with position/uv/colour, fragment sampling the atlas's alpha), a solid-white texel in the
atlas for rectangles and graph lines (drawn as quads), one vertex buffer per swapchain image
(host-visible, rewritten only when content changes), a render pass with `LOAD_OP_LOAD` on the
swapchain image limited by a scissor to the HUD's rectangle. Per present: wait on the app's wait
semaphores, submit the HUD command buffer (chaining the latency present id, 2.3), signal the
layer's semaphore, present with that semaphore. With the HUD off the hook is one atomic load and
a pass-through: zero GPU work.

**Formats and colour.** UNORM swapchains get sRGB-encoded vertex colours (shader does the
transfer), sRGB formats get linear; `HDR10_ST2084` swapchains encode to PQ at a configurable
paper white (default 203 nits); `EXTENDED_SRGB_LINEAR` (scRGB FP16) uses linear with 1.0 =
80 nits scaled to paper white. Anything else (exotic formats, no `COLOR_ATTACHMENT` usage that
can be added at swapchain creation, a present queue family without graphics): the layer reports
"can't draw" and quiverd falls back to the desktop presenter.

**Devices and swapchains.** Per-device state (aco-dlss runs two `VkDevice`s; only the one that
owns the DXGI swapchain presents), per-swapchain resources rebuilt on recreate and on
`VK_ERROR_OUT_OF_DATE_KHR`, all resources destroyed with the swapchain/device. Multiple
swapchains: draw on the largest (the game window), configurable.

**Safety.** Every entry point catches panics (Rust: `catch_unwind`, `panic = "abort"` is not
acceptable inside a game) and permanently disables drawing for the session on the first one,
leaving telemetry and pass-through intact. HUD frames are bounds-checked (cell count ≤ 128,
string bytes ≤ 4 KiB, glyph indices inside the atlas, vertex count ≤ 16k); a bad frame isn't
drawn. The layer never blocks: if a per-image fence isn't signalled it reuses the previous
image's vertex data rather than wait.

**Why not Dear ImGui or egui in the layer.** Both work (MangoHud proves ImGui) but bring layout,
config parsing, font loading and text formatting into the game process and tie the look to the
toolkit. A cell renderer is a few hundred lines, renders the same HUD frame the desktop
presenter renders (so switching presenters doesn't change the HUD), and keeps the Omarchy
theme's fonts and colours (steer 2).

### 2.7 Desktop presenter

`quiver-sight` subscribes to the same HUD frame (over the daemon socket instead of the session
dir) and renders it with GPUI: the same cells, a GPUI `canvas` for graphs. Live cells are filled
from the layer's ring through quiverd at 10 Hz (or not at all for OpenGL games, where only system
and mod metrics exist: "FPS" shows `—` without a frame source). Commits are coalesced to the
HUD's update rate; the graph updates at most 10 Hz to limit extra flips.

---

## 3. The metrics object model

### 3.1 Overview

```
 game process (Wine)                                     host
 ───────────────────                                     ────
 mod DLL (aco-dlss) ──quiver_mod.h──┐
   │ JSON (≤10 Hz): declare, gauges,│ \\.\pipe\quiver-mods-<s>   ubi-bridge.exe ── broker TCP ──┐
   │   states, counters, marks      └──────────────────────────▶ (relays to quiverd)            │
   │ ring: frame / series / event records                                                     ▼
   └──▶ /dev/shm/quiver-<uid>/<s>/mod-aco-dlss.ring ◀── mmap (read-only) ───────────── quiverd
                                                                                  registry, clocks,
 Sight layer (.so, same process)                                                  correlation,
   ├──▶ /dev/shm/…/<s>/layer-<pid>.ring  (present records) ◀── mmap (read-only) ──  aggregation,
   └──◀ /dev/shm/…/<s>/hud.frame, hud.atlas  ◀── written by ─────────────────────  HUD composer,
                                                                                  recordings
 sensors: NVML, /proc, hwmon, power_supply, Hyprland IPC ─────────────────────────▶   │
                                                                                      │ ubi-api
                                        quiver-sight (panel, desktop HUD), app, CLI, agents ◀┘
```

Two planes:

- **Control plane** (JSON lines, existing transports): producer hello, descriptor declarations,
  low-rate samples (≤ 10 Hz coalesced), markers, ring announcements; consumer catalog queries
  and subscriptions; HUD layout changes.
- **Data plane** (shared memory): per-frame and per-event records in rings; the HUD frame and
  atlas going the other way.

Rule of thumb for producers: **anything that happens per frame or per event goes in a ring;
anything you'd sample at 10 Hz or less goes as JSON.**

### 3.2 Producers

| Producer | Namespace | Transport | Publishes |
|---|---|---|---|
| **Sight layer** (`VK_LAYER_QUIVER_sight`) | `present.*` | ring (+ status block in the ring header) | one record per `vkQueuePresentKHR`: CPU time at entry and return, present id (if the app chained `VkPresentIdKHR`), swapchain, image index, present mode, result; swapchain extent/format/colour space; `VK_NV_low_latency2` in use. Later: display time via `vkWaitForPresentKHR` on a layer thread when the app enabled present-wait; Reflex timings via `vkGetLatencyTimingsNV` for games whose Reflex is native or comes from dxvk-nvapi. **It can't tell rendered from generated frames.** |
| **quiverd system** | `gpu.*`, `cpu.*`, `mem.*`, `bat.*`, `disk.*`, `net.*`, `proc.*` | in-process | GPU via NVML (load, temp, power, clocks, VRAM total/used, per-process VRAM, throttle reasons) or sysfs/hwmon for AMD/Intel; CPU load total and per core from `/proc/stat`, package temp from hwmon; RAM, swap; game process RSS, CPU, I/O from `/proc/<pid>`; battery and AC from `power_supply`/UPower; disk and network throughput. Polled only while a session runs (SIGHT S9: never wake a sleeping dGPU) |
| **quiverd display** | `display.*` | Hyprland IPC | mode, refresh, VRR active, tearing active, direct scanout target, blocked-by reasons, the game window's fullscreen state |
| **Mods** (SDK) | the mod's surface id (`aco-dlss.*`) | JSON over the mod pipe + one ring per mod | anything the mod knows. aco-dlss: rendered-frame records (game Present arrival, its own present call/return, real/generated/no-FG flag), FG state, generated/dropped counts, upscaler and preset, internal and output resolution, DLSS evaluate time, Reflex mode, Reflex sleep time, per-frame latency reports from `NvAPI_D3D_GetLatency` (input, simulation, render submit, present, driver, OS queue, GPU start/end), markers (FG reset, swapchain resize, teleport, bench begin/end) |
| **Bridge** (Ubisoft SDK) | `game.*` | broker | events only: achievement earned, rich presence change, overlay calls, game session changes. Useful as markers on the timeline ("Territory: Siwa" at 21:12); the SDK knows nothing about frames |
| **Wrappers** | `gamescope.*` | later | gamescope already exports frame timing to `mangoapp`; a reader could map it. Not MVP |

The producer set per session is dynamic: the layer appears when the game creates its first
swapchain, a mod when its SDK connects, and both disappear with the process.

### 3.3 Descriptors

A descriptor is declared once per metric, either **statically** in the mod version's
`quiver-options.toml` (`[metric.<name>]`, so the catalog works without a running game and
`options lint` checks it) or **at runtime** over the SDK (a JSON object with the same fields).
Runtime declarations override static ones field by field for that session. Built-in producers'
descriptors are compiled into quiverd.

| Field | Meaning |
|---|---|
| `id` | `<namespace>.<name>`, `[a-z0-9_-]` segments, 2–5 segments, ≤ 64 chars. The namespace is the producer's (a mod's surface id), enforced by the channel |
| `kind` | `gauge`, `counter`, `series`, `frame`, `state`, `text`, `event`, `histogram` (3.5) |
| `unit` | `ms`, `us`, `ns`, `s`, `fps`, `Hz`, `%`, `C`, `W`, `MHz`, `B`, `B/s`, `x`, `px`, `count`, or `""` |
| `label`, `short`, `help` | ≤ 24 chars, ≤ 6 chars (HUD prefix like `LAT`), ≤ 200 chars |
| `format` | `precision` (decimals), `si` (B → GB), `sign`, `percent` |
| `range` | `{ min, max }` for bars and graphs, or `auto` |
| `thresholds` | `[{ above | below, level }]`, levels `good`, `warn`, `bad`, `dim` |
| `better` | `higher` / `lower` / `none`: colouring direction and benchmark comparisons |
| `display` | default hint: `number`, `badge`, `bar`, `sparkline`, `graph`, `stack` |
| `group`, `order` | catalog grouping ("Frame generation") |
| `sample` | expected rate (`per-frame`, `per-event`, or Hz), for UI defaults and rate limits |
| `values` | for `state`: `[{ value, label, level }]`, ≤ 16 |
| `fields` | for `frame`: ordered timestamp/value field names, ≤ 12 |
| `clock` | for ring kinds: `qpc`, `mono_raw_ns`, `reflex_us` (3.9) |
| `key` | for `frame`: what `key` means (`present_seq`, `frame_id`) for correlation |
| `roles` | canonical roles this metric fills (3.4), optionally with a flag filter |
| `derive` | derived metrics (3.10): `interval`, `span`, `rate`, `ratio`, `stack` |
| `renamed_from` | old ids (as for options) |
| `level` | `basic`, `advanced`, `debug` (catalog visibility, as MOD-CONFIG) |

Identity and versioning: ids are the contract, like option ids. The descriptor format has its own
`format = 1` integer (inside `quiver-options.toml` it follows that file's `format`). A producer's
descriptor set has a revision that increments on any runtime change; consumers get
`PerfCatalogChanged` events.

Lifecycle: a metric is **declared** (in the catalog, no data), **live** (data within 2× its
expected period), **stale** (the HUD shows `—`, then hides the cell after 5 s), **gone** (its
producer disconnected; kept in the session's catalog for recordings). A producer has
`connected`, `ring: ok | none | invalid(reason)`, `clock: ok | suspect`.

### 3.4 Canonical roles and metrics

Layouts mostly reference **canonical metrics** (no namespace) that quiverd derives from whichever
producer fills the matching **role**, so "base FPS" works for aco-dlss today and for the next FG
mod, an lsfg-vk reader, or a Streamline producer later, without new layouts:

| Role | Filled by | Canonical metrics derived |
|---|---|---|
| `frame.presented` | the layer (always) | `fps`, `frametime`, `fps.p1low`, `fps.p01low`, `frametime.p99` |
| `frame.base` | a mod's frame metric, filtered to real/no-FG records; else = `frame.presented` | `fps.base`, `frametime.base`, `fps.base.p1low` |
| `frame.generated` | a mod's frame metric, generated records | `fps.generated`, `fg.ratio` (= presented ÷ base) |
| `fg.state` | a mod state metric | `fg.state` |
| `latency.frame` | a mod's (or the layer's) Reflex reports | `latency.render` (simulation start → GPU end), `latency.total` (input → present end, when input is marked), `latency.breakdown` (stack) |
| `render.resolution`, `output.resolution` | a mod text metric; the layer (swapchain extent) | same |
| `upscaler` | a mod text metric | same |

Names like the user's example `dlss.fps.base` therefore become: the mod publishes
`aco-dlss.frame` with role `frame.base` (its real frames), and layouts show `fps.base`. A mod's
own namespaced metrics remain addressable (`aco-dlss.dlss.ms`). If two producers claim the same
role, the first wins, the conflict is shown in the catalog, and the user can pick.

`fps.p1low` is defined as **1000 ÷ the 99th-percentile frame time in the window** ("1% low",
the CapFrameX P1 definition), stated in the help text; `fps.p1low.avg` (mean of the slowest 1%)
exists for people who want MangoHud-style numbers (steer 11).

### 3.5 Kinds and values

| Kind | Transport | Value | Aggregations |
|---|---|---|---|
| `gauge` | JSON | f64 in `unit`, latest wins | last, mean, min, max over windows |
| `counter` | JSON | u64 monotonic; consumers see a rate | rate per second, total |
| `series` | ring | (t, f64) per event | mean, min, max, percentiles, histogram |
| `frame` | ring | (key, flags, t, fields[≤12]) per frame | intervals between records → frame times; spans within a record; per flag |
| `state` | JSON | one of `values` | current, time share per value |
| `text` | JSON | ≤ 48 bytes UTF-8 | current |
| `event` | ring or JSON | (t, duration?, label ≤ 48 bytes) | count, list (timeline markers) |
| `histogram` | JSON | fixed bucket edges + counts, for producers that only have a distribution | percentiles |

### 3.6 Control plane

**Mods → bridge → quiverd.** MOD-CONFIG's pipe protocol bumps to `proto = 2` (fields and
messages added; protocol-1 peers ignore them):

| Direction | `t` | Fields | Meaning |
|---|---|---|---|
| mod → | `hello` | + `caps: ["metrics"]` | |
| ← | `welcome` | + `perf: { session_dir_win, clock: "qpc" }` | where rings live, how to address them |
| mod → | `metrics` | `defs: [descriptor…]` | declare or update (runtime); static ones come from the version's `quiver-options.toml` |
| ← | `metrics_ok` | `ids: { name → index }`, `rejected: { name → reason }` | indices used in ring records |
| mod → | `m` | `t` (QPC), `v: { index → value }` | gauges, states, text, counters; the SDK coalesces to ≤ 10/s |
| mod → | `mark` | `index`, `t`, `dur`?, `label`? | low-rate events (FG reset, teleport) |
| mod → | `ring_req` | `bytes` (≤ 1 MiB) | ask for a ring |
| ← | `ring` | `path_win`, `bytes`, `nonce` | the bridge created and sized the file; the SDK maps it |
| ← | `perf_ctl` | `{ rings: on/off, detail: basic/full }` | quiverd tells mods whether anyone consumes per-frame data (no HUD, no recording: mods can skip work) |

`readout` (protocol 1) remains accepted and maps to a `text` metric (3.13).

The bridge relays these over its broker connection to quiverd (MOD-CONFIG's
`BrokerRequest::ModChannel` stream). **The bridge creates ring files** on request because it is
always in the same mount namespace as the game (a Quiver launch, a Steam copy in pressure-vessel,
a container) and can enforce a budget (≤ 1 ring per mod connection, ≤ 1 MiB each).

**Layer.** The layer has no socket: it creates its own ring file in the session dir (named in
the environment, below) and polls `hud.frame`'s generation counter. quiverd discovers rings with
inotify on the session dir. This replaces SIGHT §5's "memfd over an abstract socket to
quiver-sight": the hub is quiverd, so recording, the CLI and agents work without the Sight UI
running.

**Consumers ↔ quiverd.** New `ubi_api::perf` requests (section 3.11).

### 3.7 Data plane: the session dir and Wine shared memory

**The session dir.** At launch quiverd (or `quiver steam-run`, which runs on the host before
Steam's container wrapper) creates `/dev/shm/quiver-<uid>/<session>/`, mode 0700 under a 0700
per-user parent, and passes it to the game:

```
QUIVER_SESSION=s-4kq2
QUIVER_PERF_DIR=/dev/shm/quiver-1000/s-4kq2              # for the layer (Linux code)
```

Wine copies the Unix environment into the Windows one, so the SDK sees both; it builds the
Windows path itself (`Z:\dev\shm\quiver-1000\s-4kq2\…`), falling back to Wine's
`\\?\unix\dev\shm\…` name form if `Z:` is missing (both supported: Proton creates
`dosdevices/z: → /`, and Wine's ntdll resolves `\??\unix\` names; verified in source). In
practice the SDK gets the path in `welcome` / `ring` messages and doesn't construct it.

**How the PE side shares memory with Linux processes** (verified in the Wine 10.0 tree inside
`~/code/proton-ge-custom`, GE-Proton10-34):

- `CreateFileW(path, GENERIC_READ|GENERIC_WRITE, FILE_SHARE_READ|FILE_SHARE_WRITE, …,
  OPEN_EXISTING)` → `CreateFileMappingW(h, NULL, PAGE_READWRITE, 0, 0, NULL)` →
  `MapViewOfFile(m, FILE_MAP_WRITE, …)`. The section is backed by the file's Unix fd, and
  `map_file_into_view` (`dlls/ntdll/unix/virtual.c`) maps it with
  `MAP_FIXED | (VPROT_WRITECOPY ? MAP_PRIVATE : MAP_SHARED)`: a writable, non-copy-on-write view
  is a **`MAP_SHARED` mmap of the same Unix file**, so stores are visible to any Linux process
  that maps the file, with ordinary cache coherence (x86-64 and the C11/Rust atomics on both
  sides agree on the memory model).
- **Not** pagefile-backed sections (`CreateFileMapping(INVALID_HANDLE_VALUE, …)`): wineserver
  backs those with `memfd_create("wine-mapping")` (`server/mapping.c`), which nothing outside
  Wine can open by name.
- The layer `.so` lives in the same process as the PE DLL, but PE code can't call into an ELF
  library without a Wine unixlib, and neither side knows the other's addresses. They meet
  through files in the session dir like everyone else. (A unixlib would be an extra DLL pair
  to ship and version per Proton; not worth it for this.)
- The file is created and sized (`ftruncate`) by the bridge, so the SDK never extends it, and
  mapping size = file size.

**Containers.**

- Quiver launches (`proton run` on the host): no container; trivial.
- Steam copies through pressure-vessel: `/dev` is bind-mounted from the host and Steam's own
  client↔game IPC uses files in `/dev/shm` across the container boundary, so the session dir is
  shared (believed; spike H3 confirms).
- Containers with their own `/dev/shm` (aco-dlss's dev loop): the bridge can't make the files
  visible to quiverd. Detection: every ring header has a 128-bit nonce the bridge reports over
  the broker; quiverd maps `<session dir>/<file>` and compares. On mismatch or absence,
  **the bridge becomes the reader**: it maps the rings itself (it's a Wine process in the same
  container) and relays records over its broker connection in batches (≤ 20 batches/s, the same
  record bytes, base64 in JSON or a binary frame type on the stream). Slower, but the
  benchmarking agent gets its numbers in containers too. The HUD frame then flows the other way
  through the bridge, which writes `hud.frame` into the container's session dir.

**Cleanup.** quiverd removes the session dir when the session ends and sweeps stale ones at
start. Files are 0600; readers open them `O_RDONLY` and map `PROT_READ`.

### 3.8 Ring and HUD frame layout

One ring per writer process per producer (the layer: one per process; a mod: one per SDK
connection). Fixed-size slots, many writer threads, many readers, readers never write.

```c
/* Shared by the SDK (C), the layer (Rust) and quiverd (Rust). Little-endian, x86-64.
 * Offsets are fixed; the header is one 4 KiB page. */
#define QV_RING_MAGIC   0x474E495256515551ull   /* "QUQVRING" */
#define QV_RING_VERSION 1

typedef struct qv_ring_header {
    uint64_t magic;             /* written last at creation (release) */
    uint16_t version, header_bytes;
    uint16_t slot_bytes;        /* 128 */
    uint16_t clock;             /* 1 = QPC ticks, 2 = CLOCK_MONOTONIC_RAW ns */
    uint32_t slot_count;        /* power of two */
    uint32_t producer_kind;     /* 1 layer, 2 mod */
    uint64_t clock_freq;        /* ticks per second (QPC: 10,000,000 under Wine 10) */
    uint8_t  nonce[16];         /* written by the creator, reported out of band */
    uint32_t writer_pid;        /* Linux pid for the layer; Windows pid for mods */
    uint32_t schema_rev;        /* bumps when the producer's metric indices change */
    _Alignas(64) uint64_t reserve;   /* next slot to reserve (writers: fetch_add) */
    _Alignas(64) uint64_t heartbeat; /* writer's clock, updated ≥ 1/s even when idle */
    uint64_t dropped;           /* records the writer discarded (ring disabled, overflow) */
    _Alignas(64) uint8_t status[2048];  /* producer status block (seqlocked): the layer puts
                                           swapchain extent/format/colour space, present mode,
                                           low_latency2 in use, "drawing ok" counter here */
} qv_ring_header;

typedef struct qv_record {      /* 128 bytes */
    uint64_t commit;            /* = slot index + 1, stored last with release ordering */
    uint32_t metric;            /* index from metrics_ok (or a layer built-in) */
    uint32_t flags;             /* frame: REAL 1, GENERATED 2, NOFG 4; event: has_label 8 */
    uint64_t key;               /* correlation key (present seq, frame id) */
    uint32_t swapchain;         /* writer-chosen id; changes on recreate */
    uint32_t nfields;
    int64_t  t;                 /* the record's own time in the ring's clock */
    int64_t  f[12];             /* frame fields / series value (f64 bits) / event duration */
} qv_record;
```

Writer: `i = atomic_fetch_add(&h->reserve, 1)`; slot `i & (n-1)`; copy the record with
`commit = 0`, then `atomic_store_release(&slot->commit, i + 1)`. No locks, no syscalls, no
allocation. If the slot still holds an uncommitted record from a full lap ago (a writer thread
died mid-write), the writer overwrites it; readers detect it by `commit`.

Reader (quiverd, the bridge in relay mode, later the layer reading a mod ring for graph tints):
keeps its own cursor; reads `reserve` (acquire); for each index from its cursor, checks
`slot.commit == index + 1` (else: not yet committed → stop and retry next tick; or overwritten
because `reserve - index > n` → count as lost, skip ahead); copies the slot; re-checks `commit`
after the copy (a writer lapping it mid-copy → discard). That is a per-slot sequence check, the
seqlock idea applied to a log. 4096 slots × 128 bytes = 512 KiB holds ~13 s of three records per
frame at 100 fps; quiverd drains every 50 ms.

The **HUD frame** (`hud.frame`) and the status block use a classic seqlock with two buffers:
writer increments `seq` to odd, writes buffer `seq/2 % 2`, increments to even; the reader copies
and retries if `seq` changed or is odd. Readers there are the layer (in the game) and nothing
else; the layer treats every byte as untrusted anyway.

### 3.9 Clocks

Everything is converted to **`CLOCK_MONOTONIC_RAW` nanoseconds** in quiverd, which also
timestamps its own sensor samples with that clock.

- **Mods (PE code)** use `QueryPerformanceCounter`. In this Wine
  (`dlls/ntdll/unix/sync.c`), `NtQueryPerformanceCounter` returns `monotonic_counter()`:
  `clock_gettime(CLOCK_MONOTONIC_RAW)` in 100 ns ticks, frequency `TICKSPERSEC` = 10 MHz, no TSC
  bypass. winevulkan agrees: it maps `VK_TIME_DOMAIN_QUERY_PERFORMANCE_COUNTER_EXT` to the host's
  `CLOCK_MONOTONIC_RAW` and divides by 100 (`dlls/winevulkan/vulkan.c`). aco-dlss's own
  `latency.h` relies on the same fact. So `ns = qpc × 100` when `clock_freq == 10_000_000`.
- **Guard against a future Wine/Proton** that switches QPC to the TSC (Windows does, via
  `QpcBypassEnabled`): the ring header carries `clock_freq`; if it isn't 10 MHz, or if the
  correlation check below finds a systematic offset, quiverd fits `ns = a·qpc + b` by linear
  regression over correlated present pairs and marks the producer `clock: suspect` until the fit
  is stable.
- **The layer** (Linux code) calls `clock_gettime(CLOCK_MONOTONIC_RAW)` directly.
- **Reflex reports** (`NvAPI_D3D_GetLatency` through dxvk-nvapi, or `vkGetLatencyTimingsNV`)
  are microseconds in a domain the driver/DXVK chooses (DXVK maps its own frame times into the
  report's base, `dxvk_latency_reflex.cpp`). Declared as `clock = "reflex_us"`; quiverd aligns
  them by matching each report's `presentStartTime` for frame id *k* to the mod's own
  `present_call` QPC timestamp for the same Reflex frame id, and takes the median offset. Spike
  H5 pins the domain down.
- **Wayland presentation feedback** (if the desktop presenter ever measures anything) is
  `CLOCK_MONOTONIC`; quiverd samples both clocks back to back to convert.

### 3.10 Frame correlation and derived metrics

**Correlating mod frame records with layer present records** matters for per-frame views
(graph tints, latency through to present, recordings), not for the headline numbers.

- DXVK chains `VkPresentIdKHR` with its per-swapchain frame id on every present when the device
  supports present ids (`dxvk_presenter.cpp`, `presentImage(frameId…)`); the D3D11 swapchain's
  `m_frameId` grows by one per successful `Present` and `GetLastPresentCount()` returns
  `m_frameId - DXGI_MAX_SWAP_CHAIN_BUFFERS` (`d3d11_swapchain.cpp`). vkd3d-proton does the same
  for D3D12.
- The mod keys its frame records by its own per-swapchain present sequence (1, 2, 3… for every
  `Present` it makes, generated or real) and changes its `swapchain` id on `ResizeBuffers` or a
  new swapchain. The layer keys by the present id when chained (else its own count) and changes
  its id on `vkCreateSwapchainKHR`.
- quiverd aligns the two sequences per swapchain pair by finding the constant offset *k* that
  satisfies `mod.present_call[i] ≤ layer.present_entry[i+k] ≤ mod.present_call[i] + 50 ms` for
  nearly all *i* (DXVK presents on its own submission thread, a few ms after the DXGI call), and
  re-runs the fit after any gap or recreate. Exact ids are a bonus, not a requirement, so a
  DXVK detail changing doesn't break it. Unmatched records stay usable on their own.
- With correlation: per-present type (real/generated) on the presented timeline, so the graph
  can mute generated frames; "real frame shown N ms after the game finished it"; the mod's
  Reflex reports joined to actual present times.

**Derived metrics** are a closed set of functions, declared in descriptors and computed only in
quiverd (one definition of every statistic for every consumer):

| Function | Example |
|---|---|
| `interval(frame, flags)` | `frametime.base` = time between consecutive `aco-dlss.frame` records flagged REAL, on `game_present` |
| `span(frame, a, b)` | `latency.render` = `gpu_end − sim_start` within a `latency` record |
| `rate(counter)` | `aco-dlss.fg.dropped` per second |
| `ratio(x, y)` | `fg.ratio` = `fps ÷ fps.base` |
| `stack(frame, [a, b, c, …])` | `latency.breakdown`: consecutive spans as segments |
| window stats | `mean`, `min`, `max`, `p50`, `p95`, `p99`, `p1low`, `p01low`, `stddev`, over 1 s (HUD), 10 s, session |

Producers may still publish finished numbers as gauges when only they can compute them (DLSS
evaluate time as measured by the mod), but "FPS", "1% low" and latency spans are always
quiverd's.

### 3.11 Registry, subscriptions, API

quiverd keeps, per running session: producers (state, descriptor set and revision, ring status,
clock status), the catalog (built-in + static + runtime descriptors + role bindings + derived
metrics), an in-memory store (last 10 s of raw records per metric, 1 s buckets for the whole
session), the correlator, the HUD composer, and any active recording.

New requests in `ubi_api::perf` (appended to `Request` as `Perf(PerfRequest)`, like
`ModsRequest`):

| Request | Reply | Notes |
|---|---|---|
| `PerfSessions` | sessions with perf state: producers, presenter in use, recording | |
| `PerfCatalog { session \| product_id }` | `Vec<MetricInfo>` (descriptor, producer, role bindings, state, last value) | with `product_id` and no session: static catalog from installed mod versions + built-ins |
| `PerfSubscribe { session, select: [ids, roles or globs], every_ms, window_ms, stats }` | `sub_id` | the connection then receives `Event::PerfUpdate { sub, at, values: { id → { stat → value, level } } }`; ≥ 100 ms; ≤ 8 subscriptions per connection |
| `PerfUnsubscribe { sub }` | ok | also on disconnect |
| `PerfStream { session, select, raw: true }` | `sub_id` | raw records (relative ns), for `perf record --raw` to stdout and tools; ≤ 2 per session |
| `PerfRecordStart { session, label, duration?, after_marker?, until_marker? }` / `PerfRecordStop` | recording id | |
| `PerfRecordings { product_id }`, `PerfRecording { id }` | metadata, summary | |
| `PerfCompare { a: Vec<id>, b: Vec<id> }` | per metric: mean/p1low/p99 deltas, run-to-run spread, verdict | groups of runs, not single runs |
| `HudGet/HudSet { scope, layout? , on?, presenter? }`, `HudToggle { session }`, `HudNext { session }` | | layouts revisioned like settings |

Events: `PerfCatalogChanged { session }`, `PerfUpdate` (per subscription only), `HudChanged`.

`Event::PerfUpdate` is the first event that isn't broadcast to every `Subscribe`r: today's
`Subscribe` is a single `broadcast` channel. Subscriptions need a per-connection sender in the
connection loop (`crates/ubid/src/main.rs`), which is a small change but a new pattern.

### 3.12 Consumers

**The HUD** (section 1): quiverd's HUD composer resolves the layout, subscribes internally at the
HUD's rate, formats values (units, precision, thresholds → levels), and writes the HUD frame for
the active presenter.

**Sight's Performance panel** (SIGHT §3.5): the same catalog and subscriptions at 4 Hz while
visible, plus graphs from `PerfStream`: frame times (presented and base), latency breakdown, CPU
per core, the display-path line; the HUD layout editor; MangoHud on/off.

**The app**: each play session gets a **performance summary** stored with it
(`PlaySession.perf: Option<PerfSummary>`): duration covered, presented and base FPS (mean, 1% and
0.1% lows), frame time p50/p95/p99, FG ratio, render latency mean/p95, GPU/CPU load and
temperature mean/max, GPU power mean, VRAM peak, hitch count (frame time > 4× median), and
which presenter drew the HUD (so a "desktop HUD" session's numbers are flagged). ~1 KB. The game's
Play history shows it, and **comparisons** pick sessions or recordings by profile, profile
revision, options revision and mod versions (all already on `PlaySession`): "DLSS vs Vanilla,
last 5 sessions each", "DLSS v5 vs v4".

**The CLI**:

```
$ quiver perf watch origins
21:14:03  FPS 79→158  FT 12.7  1%↓ 64  LAT 21  GPU 97% 71° 92W  CPU 38%   (DLSS K 1280×800, FG on)
$ quiver perf watch origins --metrics fps.base,latency.render,gpu.power --json   # JSON lines
$ quiver perf catalog origins                      # what's available and who provides it
$ quiver perf record origins --duration 60s --after-marker aco-dlss.bench.begin --label "Alexandria loop"
$ quiver perf list origins
$ quiver perf show r-20261116-2114
$ quiver perf compare --profile DLSS --against vanilla --label "Alexandria loop" [--last 3]
$ quiver perf compare r-…a r-…b
$ quiver perf export r-… --csv [--mangohud]
$ quiver hud toggle | next | layout set compact --game origins [--profile DLSS]
```

**Agents**: read-only perf requests (`watch`, `catalog`, `show`, `compare`) are Allow; recording
is Allow (it writes only under Quiver's data dir); launching follows the existing play policy.
The aco-dlss agent's loop for its own builds: `version add` → `quiver play origins --profile
DLSS-next` → the mod teleports to a fixed spot and emits `bench.begin`/`bench.end` markers
(aco-dlss already has a teleport facility) → `quiver perf record --after-marker … --json` →
`quiver perf compare` against the previous version's runs, with run-to-run spread so a 1% change
isn't reported as a win.

**Recording format** (`games/<product>/perf/<id>/`):

- `meta.json`: session, product, profile + revision, options revision, resolved mods and
  versions, Proton build, DXVK/vkd3d versions (from the layer's status block), GPU and driver,
  CPU, display mode/VRR/tearing/scanout state, the catalog (descriptors as used), clock fits,
  correlation offsets, label, markers.
- `records.qvr.zst`: the ring records verbatim (128-byte slots, already converted to session-
  relative ns, metric index → id table in `meta.json`), zstd-compressed. ~60 bytes per frame
  after compression; a 60 s run at 160 fps is well under 1 MB.
- `samples.jsonl.zst`: low-rate samples (gauges, states, sensors) with timestamps.
- `summary.json`: the `PerfSummary` plus per-metric stats and histograms.

CSV/MangoHud export is a projection of this. Raw recordings are made on request (or benchmark
runs); every session with an active layer gets only the summary by default (steer 13).

### 3.13 One model with MOD-CONFIG's readouts

MOD-CONFIG defined readouts (`[readout.<id>]`, `quiver_readout()`, the `readout` message,
`OptionsReadout` events, ≤ 4/s, display only) and C10 put FPS/frame-time metrics outside its
scope. Two parallel "values a running mod reports" channels would mean two catalogs, two rate
limits and two places the HUD and the panel have to look. **Readouts become metrics:**

- `[readout.<id>]` in `quiver-options.toml` is replaced by `[metric.<id>]` with a `kind`
  (`state` or `text` for what readouts were; `gauge`, `counter`, `frame`, `series`, `event` are
  new). A `[readout.x]` table is still read as `[metric.x] kind = "text"` for drafts written
  against the old shape.
- `quiver_readout(m, id, text)` stays in the header as a convenience: it declares (once) a
  `text` metric and sets it.
- The `readout` wire message is accepted and mapped to `m`; new SDKs send `metrics` + `m`.
- `OptionsReadout` events and `Subscribe { readouts: true }` (C5) are replaced by
  `PerfSubscribe` on the session; `SurfaceState.readouts` (C2) becomes a list of metric ids
  belonging to the surface, resolved through `PerfCatalog`.
- MOD-CONFIG C10 changes: FPS and frame-time metrics, and every mod-reported value, are
  delivered by this document's model; MOD-CONFIG still owns options, actions and keys.
- SIGHT M8 ("status fields") maps to metrics; Sight's Mods section shows a surface's `state` and
  `text` metrics under "Status", as before.

A mod's options page and its metrics share the surface id as namespace, so the panel can show
"Frame Generation: on · 79 → 158 fps" next to the FG toggle.

### 3.14 SDK surface (`quiver_mod.h` additions)

```c
/* ---- Metrics (protocol 2). All calls are no-ops on a handle that isn't connected, and
 * quiver_metric_declare() then returns QUIVER_NO_METRIC. Hot-path calls (gauge, count,
 * state_index, sample, frame, mark_at) take no locks, make no syscalls and never allocate;
 * they write into memory mapped at quiver_open() time or into a fixed-size coalescing table
 * that the SDK thread (or quiver_poll) sends at most 10 times a second. ---- */

typedef int32_t quiver_metric;            /* small index; < 0 = invalid */
#define QUIVER_NO_METRIC (-1)

typedef enum quiver_kind {
    QUIVER_GAUGE = 1, QUIVER_COUNTER, QUIVER_SERIES, QUIVER_FRAME,
    QUIVER_STATE, QUIVER_TEXT, QUIVER_EVENT
} quiver_kind;

enum {                                     /* quiver_frame.flags */
    QUIVER_FRAME_REAL      = 1u << 0,      /* rendered by the game */
    QUIVER_FRAME_GENERATED = 1u << 1,      /* generated (interpolated/extrapolated) */
    QUIVER_FRAME_NOFG      = 1u << 2,      /* FG off: real and the only present */
};

/* Declare (or look up) a metric. `name` is relative to the surface: "frame" becomes
 * "aco-dlss.frame". `descriptor_json` may be NULL when quiver-options.toml declares
 * [metric.<name>]; otherwise it is a JSON object with the descriptor fields (unit, label,
 * fields, roles, …). Call at init, not per frame. */
quiver_metric quiver_metric_declare(quiver_mod *m, const char *name, quiver_kind kind,
                                    const char *descriptor_json);

/* Low-rate values (coalesced, latest wins, sent ≤ 10 Hz). */
void quiver_gauge(quiver_mod *m, quiver_metric h, double value);
void quiver_count(quiver_mod *m, quiver_metric h, uint64_t delta);
void quiver_state(quiver_mod *m, quiver_metric h, const char *value);  /* a declared value */
void quiver_text (quiver_mod *m, quiver_metric h, const char *utf8);   /* ≤ 48 bytes, copied */

/* Per-event and per-frame records (shared-memory ring; dropped and counted if the ring
 * isn't mapped yet or nobody consumes them, see quiver_perf_wanted). Times are QPC ticks. */
typedef struct quiver_frame {
    uint64_t key;          /* your present sequence on this swapchain (1, 2, 3, …) */
    uint32_t flags;        /* QUIVER_FRAME_* */
    uint32_t swapchain;    /* any id; change it when the swapchain is recreated/resized */
    int64_t  t;            /* the record's time (e.g. your Present call) */
    int      nfields;      /* ≤ 12, in the order the descriptor's `fields` lists them */
    int64_t  f[12];        /* timestamps (QPC) or integers; 0 = absent */
} quiver_frame;

void quiver_frame_submit(quiver_mod *m, quiver_metric h, const quiver_frame *fr);
void quiver_sample(quiver_mod *m, quiver_metric h, int64_t t_qpc, double value);
void quiver_mark  (quiver_mod *m, quiver_metric h, int64_t t_qpc, int64_t dur_qpc,
                   const char *label /* may be NULL; ≤ 48 bytes */);

/* 1 while some consumer wants per-frame data (HUD on with a mod cell, a recording, the panel's
 * graphs); lets a mod skip optional work such as polling Reflex reports. One atomic load. */
int quiver_perf_wanted(const quiver_mod *m);

static inline int64_t quiver_now(void)
{ LARGE_INTEGER c; QueryPerformanceCounter(&c); return c.QuadPart; }
```

Behaviour:

- **Without Quiver** (`QUIVER_MODS_PIPE` unset): `quiver_open` returns the no-op handle; no
  thread, no file, no pipe. `quiver_metric_declare` returns `QUIVER_NO_METRIC`, and every other
  call returns after one branch.
- **Connecting**: the SDK thread sends `hello` / `metrics` / `ring_req`, maps the ring when
  `ring` arrives, and publishes the metric index table with a release store. Until then hot-path
  calls drop and count (`dropped` in the ring header once mapped, a `log` line at bye).
- **Hot path**: `quiver_frame_submit` is `fetch_add` + 128-byte copy + release store.
  `quiver_gauge` writes the value into a per-metric slot with a "dirty" bit; the SDK thread sends
  dirty slots at ≤ 10 Hz. Per-metric storage is preallocated at declare time (≤ 64 metrics per
  mod).
- **Threads**: any thread may call any hot-path function; `declare` is for init.
- MinGW and MSVC, C99, kernel32 only (`CreateFileW`, `CreateFileMappingW`, `MapViewOfFile`,
  `QueryPerformanceCounter`, Interlocked*), as MOD-CONFIG's header.

### 3.15 Worked example: aco-dlss

Static descriptors, in the version's `quiver-options.toml` next to its options:

```toml
[metric.frame]
kind = "frame"
label = "DLSS frames"
clock = "qpc"
key = "present_seq"
fields = ["game_present", "present_call", "present_return"]
roles = [
  { role = "frame.base", flags = ["real", "nofg"], on = "game_present" },
  { role = "frame.generated", flags = ["generated"], on = "present_call" },
]

[metric."fg.state"]
kind = "state"
label = "Frame Generation"
short = "FG"
values = [
  { value = "off", label = "off", level = "dim" },
  { value = "on", label = "on" },
  { value = "failed", label = "failed", level = "bad" },
]
roles = ["fg.state"]

[metric."fg.dropped"]
kind = "counter"
label = "Generated frames dropped"
help = "The generated frame wasn't ready in time; only the real frame was shown."
better = "lower"

[metric.upscaler]
kind = "text"
label = "Upscaler"
roles = ["upscaler"]          # "DLSS K", "DLSS M", "TAA"

[metric."res.internal"]
kind = "text"
label = "Internal resolution"
roles = ["render.resolution"]

[metric."dlss.ms"]
kind = "series"
unit = "ms"
label = "DLSS cost"
short = "DLSS"
format = { precision = 1 }
better = "lower"

[metric."reflex.mode"]
kind = "state"
label = "Reflex"
values = [{ value = "off", label = "off", level = "dim" }, { value = "on", label = "on" },
          { value = "boost", label = "boost" }]

[metric.latency]
kind = "frame"
label = "Reflex latency"
clock = "reflex_us"
key = "frame_id"
fields = ["input", "sim_start", "sim_end", "submit_start", "submit_end", "present_start",
          "present_end", "driver_end", "queue_end", "gpu_start", "gpu_end"]
roles = ["latency.frame"]
derive = [
  { id = "render", fn = "span", from = "sim_start", to = "gpu_end", label = "Render latency",
    short = "LAT", unit = "ms", thresholds = [{ above = 30, level = "warn" }, { above = 50, level = "bad" }] },
  { id = "breakdown", fn = "stack", label = "Latency breakdown",
    segments = ["sim_start", "sim_end", "submit_end", "present_end", "gpu_end"] },
]

[metric.bench]
kind = "event"
label = "Benchmark markers"     # labels "begin" / "end"
```

Code (C++, aco-dlss; about forty lines in the places that already exist):

```cpp
// src/core/qmetrics.cpp
#include "quiver_mod.h"

static quiver_mod* Q;
static quiver_metric M_FRAME, M_FG, M_DROP, M_UPS, M_RES, M_DLSS, M_REFLEX, M_LAT, M_BENCH;

void qmetrics_init(quiver_mod* q) {                  // after quiver_open("aco-dlss", …)
  Q = q;                                             // descriptors come from quiver-options.toml
  M_FRAME = quiver_metric_declare(Q, "frame", QUIVER_FRAME, nullptr);
  M_FG    = quiver_metric_declare(Q, "fg.state", QUIVER_STATE, nullptr);
  M_DROP  = quiver_metric_declare(Q, "fg.dropped", QUIVER_COUNTER, nullptr);
  M_UPS   = quiver_metric_declare(Q, "upscaler", QUIVER_TEXT, nullptr);
  M_RES   = quiver_metric_declare(Q, "res.internal", QUIVER_TEXT, nullptr);
  M_DLSS  = quiver_metric_declare(Q, "dlss.ms", QUIVER_SERIES, nullptr);
  M_REFLEX= quiver_metric_declare(Q, "reflex.mode", QUIVER_STATE, nullptr);
  M_LAT   = quiver_metric_declare(Q, "latency", QUIVER_FRAME, nullptr);
  M_BENCH = quiver_metric_declare(Q, "bench", QUIVER_EVENT, nullptr);
}

// Wherever the mod makes a DXGI Present: the present thread (FG on) or the hook (FG off).
// kind: 'N' FG off, 'G' generated, 'R' real (the existing fg trace's kinds).
void qmetrics_present(char kind, uint32_t swapchain, uint64_t seq,
                      int64_t game_present, int64_t call, int64_t ret) {
  quiver_frame fr{};
  fr.key = seq;  fr.swapchain = swapchain;  fr.t = call;
  fr.flags = kind == 'G' ? QUIVER_FRAME_GENERATED
           : kind == 'R' ? QUIVER_FRAME_REAL : (QUIVER_FRAME_REAL | QUIVER_FRAME_NOFG);
  fr.nfields = 3;  fr.f[0] = game_present;  fr.f[1] = call;  fr.f[2] = ret;
  quiver_frame_submit(Q, M_FRAME, &fr);
}

// Where config reload already runs every 64 frames, and on F9 / F10:
void qmetrics_status(const Status& s) {
  quiver_state(Q, M_FG, s.fg_failed ? "failed" : s.fg_on ? "on" : "off");
  quiver_text (Q, M_UPS, s.dlss_on ? s.preset_label : "TAA");      // "DLSS K"
  quiver_text (Q, M_RES, s.internal_res);                           // "1280×800"
  quiver_state(Q, M_REFLEX, s.reflex_mode);                         // "on"
}

void qmetrics_dlss_eval(int64_t t, double gpu_ms) { quiver_sample(Q, M_DLSS, t, gpu_ms); }
void qmetrics_gen_dropped() { quiver_count(Q, M_DROP, 1); }

// Reflex: every 64 frames, only when someone wants it.
void qmetrics_reflex_poll() {
  if (!quiver_perf_wanted(Q)) return;
  NV_LATENCY_RESULT_PARAMS r{};  r.version = NV_LATENCY_RESULT_PARAMS_VER;
  if (NvAPI_D3D_GetLatency(dev, &r) != NVAPI_OK) return;           // via dxvk-nvapi
  for (auto& f : r.frameReport) {
    if (f.frameID <= last_reported) continue;
    quiver_frame fr{};  fr.key = f.frameID;  fr.t = f.presentStartTime;  fr.nfields = 11;
    int64_t v[11] = { (int64_t)f.inputSampleTime, (int64_t)f.simStartTime, (int64_t)f.simEndTime,
                      (int64_t)f.renderSubmitStartTime, (int64_t)f.renderSubmitEndTime,
                      (int64_t)f.presentStartTime, (int64_t)f.presentEndTime,
                      (int64_t)f.driverEndTime, (int64_t)f.osRenderQueueEndTime,
                      (int64_t)f.gpuRenderStartTime, (int64_t)f.gpuRenderEndTime };
    memcpy(fr.f, v, sizeof v);
    quiver_frame_submit(Q, M_LAT, &fr);
    last_reported = f.frameID;
  }
}
```

(aco-dlss already has every input: the fg trace's N/G/R kinds and QPC timestamps
(`fg_d3d12.cpp`), `perf_real`/`perf_generated`/`gen_dropped` counters, Reflex frame ids
(`reflex.cpp`), DLSS evaluate timing. `NvAPI_D3D_GetLatency` is new for it; dxvk-nvapi
implements it.)

What quiverd derives (1 s window):

- `fps` = 158.1 (layer presents), `frametime` = 6.3 ms
- `fps.base` = 79.0 (records flagged REAL, `interval` on `game_present`), `frametime.base` =
  12.7 ms, `fps.base.p1low` = 64
- `fps.generated` = 78.6, `fg.ratio` = 2.0 (×), `aco-dlss.fg.dropped` = 0.4/s
- `latency.render` = mean of `gpu_end − sim_start` = 21 ms; `latency.breakdown` segments
- `upscaler` = "DLSS K", `render.resolution` = "1280×800", `output.resolution` = "2560×1600"

**The resulting compact HUD line:**

```
FPS 79→158  12.7ms  1%↓ 64  │ GPU 97% 71° 92W  │ CPU 38% 64°  │ DLSS K 1280×800 · FG 2× · LAT 21ms
```

With F9 pressed (FG off) the same layout shows `FPS 92  10.9ms  1%↓ 74 … FG off`: `fps.base`
follows `fps` because the records are flagged NOFG, and the `→158` part is dropped because
`fg.state` says off.

### 3.16 Security and robustness

The game process, its mods and anything they load are untrusted input to quiverd, the HUD and
the app.

- **Namespaces**: a mod channel can declare and publish only under its surface id (as MOD-CONFIG
  limits options); the layer only `present.*`. Role claims are accepted from producers in the
  session's resolved stack only.
- **Descriptors**: validated like schemas (ids, lengths, ≤ 64 metrics per producer, ≤ 512 per
  session, ≤ 16 state values, ≤ 12 fields, known units/kinds/levels, thresholds finite); invalid
  ones are rejected with a reason in `metrics_ok`, never half-applied.
- **Strings** (labels, text values, event labels): UTF-8 validated, NFC, control characters and
  bidi overrides removed, length-capped, rendered only with the HUD glyph set; never interpreted
  as markup, format strings or paths.
- **Numbers**: NaN/∞ dropped; timestamps outside `[session start − 1 s, now + 1 s]` dropped;
  values outside 1000× a declared range clamped for display and flagged.
- **Rates**: JSON ≤ 10 messages/s per producer after coalescing (excess: latest wins), declare
  ≤ 1/s after the first second, marks ≤ 20/s; rings ≤ 20,000 records/s per producer (quiverd
  stops draining a ring above that and marks it `invalid(rate)`).
- **Rings**: readers map read-only; every slot is checked (commit sequence, metric index in
  range, `nfields` ≤ 12, swapchain id churn bounded); a ring with more than 1% invalid slots over
  10 s is disabled for the session with a logged reason. A writer can only lie to itself.
- **The game side reading quiverd's output**: the layer bounds-checks the HUD frame and atlas
  (section 2.6). quiverd never sends secrets there; the HUD frame contains display strings only.
- **Isolation**: a producer's failure never stalls another; the HUD composer runs on its own
  task with a deadline; a slow consumer gets coalesced updates, never back-pressure into
  producers.
- **Privacy**: recordings stay local; mod strings in recordings are already sanitized; agents
  reading recordings see what the user sees.

---

## 4. Phasing and spikes

### 4.1 Spikes

| # | Spike | Question | De-risks |
|---|---|---|---|
| H1 | **Desktop HUD cost** | With AC Origins fullscreen on Hyprland (XWayland and Wine-Wayland), map a click-through overlay-layer surface; record `hyprctl monitors -j` blockers, and frame pacing (layer present timestamps, VRR on) with HUD commits at 0, 2, 10, 60 Hz; repeat with `direct_scanout = 2` and an `immediate` rule | the presenter choice, the desktop presenter's update cap |
| H2 | **In-layer HUD renderer** | A minimal cell renderer in the Sight layer (S3's base): DXVK (Origins), vkd3d-proton (a D3D12 title), aco-dlss FG on (two devices, two presents per frame) and Reflex on (no new DXVK frame-id warnings; latency-submission present id chained); GPU cost per present at 1600p; UNORM/sRGB/HDR10/scRGB swapchains; loader layer order relative to vkBasalt and lsfg-vk | the default presenter |
| H3 | **Wine shared memory** | A PE test program maps `Z:\dev\shm\…` (and `\\?\unix\…`) with `CreateFileMappingW`; a Linux reader sees stores with the ring protocol at 10k records/s; same inside pressure-vessel for a Steam copy (is `/dev/shm` the host's?) and in a podman container (relay path) | the data plane |
| H4 | **Clocks and correlation** | QPC = `CLOCK_MONOTONIC_RAW`/100 on the Proton builds in use (GE-Proton10, Proton 9/10, Experimental); DXVK's `VkPresentIdKHR` vs the mod's present sequence with FG on; offset fit stability across resize and alt-tab | per-frame views, latency to present |
| H5 | **Reflex reports** | `NvAPI_D3D_GetLatency` through dxvk-nvapi with aco-dlss's injected Reflex: fields filled, units, clock domain vs QPC; and `vkGetLatencyTimingsNV` from the layer for the same frames | latency metrics |
| H6 | **Sensors** | = SIGHT S9: NVML on the laptop without waking the dGPU outside sessions; hwmon CPU package temp; battery | system producer |
| H7 | **Keys** | Shift+F3 / Ctrl+Shift+F3 as Hyprland binds over XWayland and Wine-Wayland games; does a consumed Hyprland bind hide Shift_L+F2 from MangoHud's key polling; remapping MangoHud's `toggle_logging` via `MANGOHUD_CONFIG` | key plan |
| H8 | **Atlas and text** | quiverd rasterizes the Omarchy monospace font into an R8 atlas at the layer's size; legibility at 0.75× on 1080p; glyph coverage of the fixed set | HUD look |

### 4.2 Phases

**Phase 0** (no UI): spikes H1–H5 (with SIGHT S3, S12 since the layer and session ids are
shared), the session dir and `QUIVER_PERF_DIR`, the ring format as a shared crate
(`ubi-perf`: ring reader/writer, descriptor types, validation) with a C header generated or
checked against it.

**Phase 1 (MVP): a MangoHud replacement for Quiver launches, with mods' headline numbers.**

- Sight layer: present ring + status block; **HUD renderer** (cells, live fps/frame time/graph,
  atlas upload, sRGB/UNORM; HDR formats fall back to the desktop presenter until phase 2).
- quiverd: perf registry, layer ring reader, system producer (NVML, `/proc`, hwmon,
  power_supply), display producer (Hyprland IPC), aggregation (1 s, 10 s, session; p1low),
  canonical metrics for `frame.presented`, HUD composer, `hud.frame` + `hud.atlas`.
- Presets FPS only, Compact, Standard, Full (without latency breakdown); placement, opacity,
  scale; per game and per profile layout selection (built-in presets only; custom layouts as
  TOML files).
- Keys: Shift+F3 / Ctrl+Shift+F3 binds while a Quiver game is focused; `quiver hud toggle|next|
  layout`; MangoHud logging remap when Quiver launches MangoHud.
- **Mod metrics over JSON only**: MOD-CONFIG's SDK pipe (its phase 2 transport, pulled forward:
  `hello`/`welcome`, `metrics`, `m`, `mark`), `gauge`/`counter`/`state`/`text`/`event`;
  readouts folded into metrics. aco-dlss publishes FG state, upscaler, internal resolution,
  Reflex mode, and **a counter of real frames** → `fps.base` as a rate (mean only, no base 1%
  low yet) and a **gauge of mean render latency** it computes from Reflex reports.
- `PerfCatalog`, `PerfSubscribe`, `quiver perf watch|catalog`; `PlaySession.perf` summary.
- Sight's Performance section shows the same numbers (if Sight's MVP is there; the HUD doesn't
  depend on it).

**Phase 2**

- Mod rings: `ring_req`, frame/series records, `fps.base` percentiles and frame time, generated
  frame tint in the graph (the layer reads the mod's ring), correlation, Reflex latency
  breakdown (`latency.frame` role, `reflex_us` alignment).
- Desktop presenter (needs SIGHT S2's GPUI layer-shell) and the automatic presenter switch;
  anti-cheat-flagged and OpenGL games get the HUD.
- HDR10 and scRGB in the layer renderer.
- Recordings, `perf record|list|show|compare|export`, comparisons in the app's session history,
  benchmark markers, agent workflow; bridge relay for containers.
- HUD layout editor and metric picker in Sight and the app.

**Phase 3**

- gamescope/`mangoapp` data reader, lsfg-vk awareness, a Streamline producer for native DLSS-G
  titles (base vs presented without a Quiver mod), layer-side Reflex timings for games with
  native Reflex, present-wait display times, per-frame CPU/GPU split from the layer
  (timestamp queries).

---

## 5. Steers wanted

1. **Where the HUD draws.** (a) In the Sight layer by default, desktop layer-shell fallback
   chosen automatically; (b) desktop layer-shell only (SIGHT's original plan); (c) in-layer
   only, no fallback; (d) both, user picks per game, no automatic choice.
   *Recommend (a): a desktop surface over a fullscreen game turns off direct scanout and tearing
   in Hyprland and adds off-cadence flips under VRR; the fallback covers OpenGL and anti-cheat
   games.*
2. **Renderer in the layer.** (a) Own tiny cell renderer fed a pre-formatted HUD frame, glyph
   atlas rasterized out of process; (b) Dear ImGui in the layer (MangoHud's way), layout and
   formatting in-layer; (c) egui in the layer.
   *Recommend (a): smallest code in the game process, no font parsing there, and the same HUD
   frame drives both presenters.*
3. **Who formats.** (a) quiverd formats values and resolves layouts; the layer fills only live
   cells from its own presents; (b) the layer reads every ring and formats itself; (c) quiverd
   sends pixels (pre-rendered texture).
   *Recommend (a): one definition of every number, zero-lag FPS and graph anyway.*
4. **HUD keys.** (a) Shift+F3 toggle, Ctrl+Shift+F3 next preset, bound only while a Quiver game
   is focused, plus a hyprpad snippet; (b) a Super chord always bound; (c) take over MangoHud's
   Shift_R+F12 for muscle memory; (d) no default.
   *Recommend (a); also remap MangoHud's `toggle_logging` off Shift_L+F2 when Quiver launches
   MangoHud.*
5. **MangoHud.** (a) Quiver HUD by default, MangoHud a per-game option (Sight can toggle it),
   CSV export compatible with MangoHud logs; (b) MangoHud stays the default; (c) drop MangoHud
   integration.
   *Recommend (a).*
6. **Metric identity.** (a) Producer-namespaced ids plus canonical roles (`frame.base`,
   `latency.frame`, …) that layouts bind to; (b) free-form ids only, layouts name exact mod
   metrics; (c) a closed list of metrics Quiver knows, nothing else.
   *Recommend (a): "base FPS" works for every FG mod without new layouts, and mods keep their
   own extras.*
7. **Data plane.** (a) JSON for ≤ 10 Hz values, shared-memory rings in a per-session `/dev/shm`
   dir for per-frame/per-event data, bridge relay when quiverd can't see the dir; (b) everything
   JSON over the pipe (simple; a few hundred messages per second per mod); (c) everything in
   shared memory, including gauges.
   *Recommend (a).*
8. **Who creates mod rings.** (a) The bridge, on the SDK's request (same mount namespace as the
   game, enforces a budget); (b) the SDK creates its own file in `QUIVER_PERF_DIR`; (c) quiverd
   creates them (fails in containers).
   *Recommend (a).*
9. **Clock.** (a) Normalize to `CLOCK_MONOTONIC_RAW` ns, take QPC = RAW/100 (verified for Wine
   10), verify continuously against present correlation and fall back to a fitted mapping;
   (b) align every producer only by correlation; (c) trust producers' clocks.
   *Recommend (a).*
10. **Derived statistics.** (a) quiverd computes FPS, lows, percentiles and latency spans from
    raw records; producers may add gauges only they can measure; (b) producers publish
    finished numbers; (c) both, shown side by side.
    *Recommend (a).*
11. **"1% low".** (a) 1000 ÷ p99 frame time (CapFrameX P1), with the average-of-slowest-1%
    variant available; (b) average of the slowest 1% as the default; (c) show both in Standard.
    *Recommend (a), defined in the help text.*
12. **Layout scope and toggle memory.** (a) Global default + per game + per profile, toggle
    state remembered per game; (b) per game only; (c) global only, toggle not remembered.
    *Recommend (a): DLSS and Vanilla profiles want different HUDs.*
13. **What gets recorded automatically.** (a) A summary on every session with the layer, raw
    records only on request or in benchmark runs; (b) raw records for the last N sessions;
    (c) nothing unless asked.
    *Recommend (a): ~1 KB per session buys DLSS-vs-Vanilla history for free.*
14. **MOD-CONFIG readouts.** (a) Fold them into metrics (`[metric.*]` kinds `state`/`text`,
    `quiver_readout` kept as sugar, `OptionsReadout` replaced by `PerfSubscribe`); (b) keep
    readouts separate from metrics.
    *Recommend (a): one catalog, one rate limit, one place for the HUD and panel to look.*
15. **HUD in captures.** (a) The in-layer HUD appears in recordings and screenshots like
    MangoHud's, and Sight's own screenshot can hide it for that frame; (b) Sight always hides it
    from its screenshots; (c) never hide.
    *Recommend (a).*
16. **Anti-cheat-flagged games.** (a) Layer off, desktop presenter for the HUD; (b) no HUD;
    (c) layer on anyway.
    *Recommend (a).*
17. **Mods and drawing.** (a) Mods provide data only; Quiver renders everything from
    descriptors; (b) later, mods may contribute small custom widgets (a declarative cell list);
    (c) mods may draw into the HUD region.
    *Recommend (a) now, (b) only if a real mod needs it.*
18. **Where the HUD composer runs.** (a) In quiverd (always running; HUD works without the Sight
    UI); (b) in `quiver-sight`; (c) a separate `quiver-hud` helper per session.
    *Recommend (a): it's formatting and a shared-memory write, and recordings and the CLI need
    the same aggregation anyway.*

---

## 6. Changes to the other design documents

- **SIGHT.md**: steer 7 is superseded by this document (note added there); §3.5's "Own perf HUD
  (replace MangoHud)" moves from *later* to MVP via the in-layer renderer; §4.3 item 3's
  telemetry ring moves from "memfd over an abstract socket to quiver-sight" to files in the
  session dir read by quiverd (section 3.6); the "corner HUD" becomes the `clock` HUD layout.
  These are recorded here rather than rewritten there until the steers are decided.
- **MOD-CONFIG.md**: readouts become metrics (section 3.13; pointer added where readouts are
  defined); the SDK protocol goes to 2 with the messages in 3.6; C5's `OptionsReadout` and C10's
  "FPS/frametime is SIGHT's" are replaced by this document's model.
