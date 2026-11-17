# Sight HUD: a pinned performance HUD and pluggable metrics (design notes)

Status: design, steers decided (2026-11-16); see [Decisions](#5-decisions). Nothing here is
built. Extends `design/SIGHT.md` (the in-game overlay) and `design/MOD-CONFIG.md` (mod options
and the `quiver_mod.h` SDK). It **supersedes SIGHT steer 7**: Quiver gets its own pinned
performance HUD; MangoHud stays available as an option per game.

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
| **session dir** | `/dev/shm/quiver-<uid>/<session>/`: the rendezvous where rings and the HUD image live |
| **ring** | an append-only shared-memory log of per-frame or per-event records, one writer process |
| **HUD model** | what quiverd sends `quiver-sight`: the resolved layout with every value already formatted (text, level, bar fractions, sparkline points), no pixels |
| **HUD image** | what `quiver-sight` renders from the HUD model with GPUI, off-screen: premultiplied RGBA pixels in `hud.image`, plus the rectangles where the layer draws the live graph |
| **presenter** | as in SIGHT: who puts pixels on screen. Here: the **game presenter** (the Sight layer composites the HUD image into the swapchain) or the **desktop presenter** (a layer-shell surface from `quiver-sight`) |

---

## 0. Summary

- **Where it draws.** The pinned HUD is drawn **inside the game by the Sight Vulkan layer**,
  with an **automatic fallback to a compositor (layer-shell) surface** when the layer isn't
  active (OpenGL/native games, anti-cheat-flagged games, layer failure). Above all this keeps
  **VRR** pacing intact: a desktop surface that updates adds compositor flips off the game's
  cadence. It also keeps **direct scan-out** and **tearing** available: in Hyprland any
  overlay-layer (or visible top-layer) surface on the game's monitor disables both, and
  Hyprland has no overlay-plane offload to avoid it.
- **How it's drawn.** The layer is a **thin compositor**: it blends a premultiplied RGBA HUD
  image from shared memory (re-uploaded only when its publish counter changes) and draws the
  live frame-time graph itself on every present. The image is **rendered by GPUI, off-screen,
  in `quiver-sight`**, with the same `ubi-ui` components, theme and fonts (Geist, Geist Mono)
  as the app, so the HUD looks like Quiver. quiverd owns the registry, aggregation and
  formatting and sends `quiver-sight` a compact HUD model. A spike on GPUI 0.2.2 measured a
  warm HUD frame (layout, paint, GPU, readback) at 0.2–0.7 ms p50.
- **Metrics model.** Producers declare descriptors (namespaced id, kind, unit, labels, format,
  thresholds, display hints, roles) once; values flow either as **JSON over the existing
  channels** (≤ 10 Hz: gauges, states, counters, text) or through **shared-memory rings** in a
  per-session `/dev/shm` directory (per-frame and per-event records). quiverd is the hub:
  registry, alignment of clocks and frames, aggregation (windows, percentiles, 1% lows),
  derived metrics, subscriptions, the HUD model and recordings. Consumers render from
  descriptors with no per-metric code.
- **Wine shared memory works with a real file.** A PE DLL calling `CreateFileMappingW` on a
  file it opened under `Z:\dev\shm\…` gets a `MAP_SHARED` mapping of the same Unix file that
  Linux processes `mmap` (verified in Wine 10.0 / GE-Proton10 source); pagefile-backed sections
  are anonymous memfds and can't be shared by name. QPC under Wine is `CLOCK_MONOTONIC_RAW` in
  100 ns ticks (verified), so mod and layer timestamps share a clock.
- **Mods** get a handful of `quiver_mod.h` calls: declare metrics (or ship them in
  `quiver-options.toml`), set gauges/states, submit per-frame records and markers. Lock-free,
  allocation-free and syscall-free on the hot path; no-ops when Quiver isn't there. MOD-CONFIG's
  "readouts" are now metrics of kind `state`/`text`.
- **Captures.** A setting, "HUD in captures: Shown / Hidden" (default Shown). Sight's own
  screenshots always offer both; OBS game capture follows Vulkan layer order, which Quiver
  sets; for compositor screen capture, Hidden moves the HUD to the layer-shell fallback with
  Hyprland's `no_screen_share` rule while a share is running.
- **MVP:** in-layer compositor and GPUI off-screen HUD (on the GPUI bump Sight needs anyway)
  with four presets, present timing from the layer, system sensors from quiverd, mod metrics
  over the SDK pipe (JSON), a per-session performance summary, `quiver perf watch`. Mod rings,
  frame correlation, Reflex breakdowns, the desktop fallback, recordings and comparisons
  follow.

---

## 1. The pinned HUD

### 1.1 What it shows: presets

Four built-in presets plus Custom. Every preset is just a layout (section 1.4) over metric ids and
roles, so mod metrics appear in presets without special cases: a preset row that references a
role nobody provides is dropped, not shown empty. The sketches below are text approximations;
the real HUD is drawn with the app's components (section 2.7).

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

**Standard** (a small block with a frame-time graph; 284×140 logical px in the spike):

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
their desktop (a notification, a PiP, Quiver's own toast) is costing them direct scan-out.

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
| `captures` | `shown` / `hidden` (section 2.12) | `shown` |

Colours are theme **levels**, never raw colours: `normal`, `dim`, `good`, `warn`, `bad`,
`accent`, mapped to the theme tokens the app uses (the shared `ubi-ui` theme, SIGHT §4.3). A
mod can say "this value is bad above 50 ms"; it can't pick hot pink. Text uses the app's fonts:
Geist for labels, Geist Mono for numbers (tabular, so values don't jitter as they change).

Sizes are relative to the swapchain, not the output, in the game presenter: the HUD is part of
the game's image, and `quiver-sight` renders it at the pixel scale that follows from the
swapchain height the layer reports. If the game renders at a lower resolution than the output
and the compositor scales it up (XWayland's emulated modes), the HUD scales with it and is
softer; the desktop presenter doesn't have that problem. Noted, not solved (it's the same for
MangoHud).

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

Decided (decision 4):

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
hud.global   = { layout = "compact", on = false, captures = "shown" }
hud.game.<product>             = { layout = "standard" }
hud.profile.<product>/<profile> = { layout = "dlss-tuning", on = true, captures = "hidden" }
```

`on` is the default state at launch. The toggle chord changes it for the session and is
remembered as the game's last state (decision 12). `captures` resolves through the same scopes
(decision 15).

### 1.5 Update rates

| What | Rate | Where computed |
|---|---|---|
| Frame-time graph | every present | the layer, from its own presents, drawn into the graph rectangle of the HUD image |
| Presented FPS number, frame time number | 2 Hz (MangoHud's `fps_sampling_period` default is 500 ms); 1, 4, 10 Hz selectable | quiverd, from the layer's ring (drained every 50 ms), sent in the HUD model |
| Base FPS, 1% lows, latency, mod gauges | 2 Hz (window 1 s) | quiverd, sent in the HUD model |
| System sensors (GPU/CPU load, temps, power, memory) | 1 Hz | quiverd |
| Battery, disk, network | 0.5–1 Hz | quiverd |
| Display path (scan-out, VRR, tearing) | on Hyprland events | quiverd |
| Mod gauges and states | as published, coalesced to ≤ 10 Hz | SDK → quiverd |

`quiver-sight` re-renders the HUD image only when the HUD model's revision changes (at most the
HUD's rate, 2 Hz by default). The game presenter composites the image on every present, uploads
it again only when its publish counter changes, and redraws the graph from its own present
timestamps every present. The FPS number therefore lags the presents by at most one drain
interval plus one render (≈ 50 ms), which is invisible at a 2 Hz update rate.

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
editor, and since the HUD is drawn by the same `ubi-ui` components it can preview the layout
exactly as it will look in game.

CLI: `quiver hud layout show|set|edit`, `quiver hud catalog origins` (the same catalog as text),
`quiver hud status` (presenter in use and why).

### 1.7 MangoHud

MangoHud stays a supported option per game (`wrappers=mangohud` or `MANGOHUD=1`, MOD-CONFIG's
runtime surface for its config). Quiver's HUD is the default; if both are enabled Sight shows a
one-time hint, and the HUD toggle chord doesn't touch MangoHud. Reasons to keep it: OpenGL games
(MangoHud also hooks GL), gamescope sessions via `mangoapp`, its logging/upload ecosystem, users'
existing presets. `quiver perf export --mangohud` writes MangoHud-compatible CSV so tools that
read MangoHud logs (FlightlessMango's log viewer, spreadsheets) keep working.

---

## 2. Where and how it draws

### 2.1 Why in the game: VRR, direct scan-out, tearing

The HUD is drawn into the game's own image because anything Quiver puts *on the desktop* over a
fullscreen game costs the display path a performance HUD exists to measure. In order of
importance (source: the user's Hyprland fork, `~/code/hypxrland`, HypXRland on Hyprland
0.56/0.57, files `src/output/Monitor.cpp`, `src/render/Renderer.cpp`,
`src/config/values/ConfigValues.cpp`; consistent with the earlier hyprsc research,
`~/code/hyprsc/docs/research/gamescope-hyprland-integration.md` §4):

1. **VRR must not be disturbed.** With VRR the panel refreshes when a frame is flipped, so the
   display follows the game's frame rate. VRR itself survives a desktop HUD: `ensureVRR` for
   `misc:vrr` 2 and 3 checks only that the workspace has a real fullscreen window (and content
   type game/video for 3). But a layer surface that commits (a number changed) damages the
   output and schedules a compositor frame of its own. That flip lands wherever the HUD update
   happened, not on the game's cadence, and the game's next frame may then wait for the panel's
   minimum refresh interval. At 2 Hz number updates that is two off-cadence flips per second; a
   desktop-side frame-time graph updating at 30–60 Hz would be 30–60 (inferred from the
   scheduling code; spike H1 measures it). A HUD inside the game's frame flips with that frame
   and adds none.
2. **Direct scan-out must keep working.** Hyprland scans a fullscreen client's buffer out
   directly only when it is the monitor's **solitary client** (`isDSBlocked` →
   `DS_BLOCK_CANDIDATE`), and `isSolitaryBlocked` refuses that when, among others:
   - `!m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY].empty()`: **any surface on the
     overlay layer of that monitor, regardless of size, opacity or content** (`SC_OVERLAYS`);
   - any `top` layer surface whose fade alpha isn't 0 (`SC_OVERLAYS`);
   - a floating window allowed over the fullscreen one (`SC_FLOAT`), so a pinned float is no
     better;
   - a Hyprland notification (`SC_NOTIFICATION`), the fullscreen window not being opaque or not
     covering the monitor, the window being maximized rather than fullscreen (`SC_WINDOWED`).

   `render:direct_scanout` defaults to 0 (`ConfigValues.cpp:576`; values `disable` / `enable` /
   `auto`); `auto` scans out only a true-fullscreen window whose content type is `game`
   (`Monitor.cpp` ≈ 2398, `DS_BLOCK_WINDOWED` / `DS_BLOCK_CONTENT`). **Hyprland has no
   overlay-plane offload**: it never moves a layer surface onto a hardware plane to keep the
   game's buffer scanned out, so the only way to have both a HUD and direct scan-out is to put
   the HUD in the game's buffer.
3. **Tearing** (an `immediate` window rule for a competitive game) has the same solitary-client
   requirement (`isTearingBlocked` → `TC_CANDIDATE`) and is lost the same way.

`hyprctl monitors -j` reports `solitaryBlockedBy`, `directScanoutBlockedBy` and
`tearingBlockedBy`, which makes all of this measurable (and gives the Full preset its
display-path line).

The user's current configuration, read-only: `misc:vrr = 3`, `cursor:no_break_fs_vrr = 2`,
Omarchy's default `general:allow_tearing = false`, and `render:direct_scanout` unset (0, off).
So today a desktop-side HUD would cost the VRR cadence but no scan-out or tearing. Direct
scan-out is nonetheless a requirement, not an accident of today's config: the design must not
be the thing that blocks the fastest display path the moment the user sets
`direct_scanout = auto` or adds an `immediate` rule. (On hybrid laptops where the panel hangs
off the iGPU and the game renders on the dGPU, direct scan-out of the game's buffer is often
impossible anyway; the VRR and tearing arguments still hold.)

### 2.2 What other platforms do

KWin, Mutter and sway scan fullscreen windows out directly by default, so a desktop overlay
there costs the same as in Hyprland. gamescope keeps the game's buffer scanned out by putting
overlays (the Steam overlay, `mangoapp`) on **hardware planes**. Windows gets the same effect
from independent flip and multiplane overlays (MPO), and Windows game overlays (RTSS, Steam,
NVIDIA, Discord, Ubisoft's `overlay64.dll`) either draw in-process or **composite an image
rendered out of process, in process**. Without plane offload in Hyprland, the last pattern is
the one that fits: Quiver renders the HUD out of process and the Sight layer composites it.

### 2.3 What MangoHud does

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

### 2.4 Frame generation, Reflex, anti-cheat

- **FG.** A HUD drawn into a frame that is then fed to an interpolator gets interpolated:
  in generated frames its text is warped by the game's motion vectors and shimmers. That's what
  happens to anything drawn in-process before the mod's DLSS-G pass (a DXGI `Present` hook, for
  instance). The layer sits at `vkQueuePresentKHR`, after DXVK and after the mod's FG, and
  composites on every present, real and generated, with the same pixels: crisp. Order matters
  with other layers: **Sight's layer must be below (closer to the driver than) any
  frame-generation or post-processing layer** (lsfg-vk, vkBasalt, a driver present layer), or it
  would be interpolated or post-processed. The launcher controls the order via
  `VK_INSTANCE_LAYERS`; spike H2 checks the loader's actual chain (`VK_LOADER_DEBUG=layer`).
  Capture layers are the exception that may go either side (section 2.12).
- **Reflex.** Compositing adds GPU work on the present queue right before present. With
  `VK_NV_low_latency2` in use, every `vkQueueSubmit` should carry
  `VkLatencySubmissionPresentIdNV` so the driver attributes the work to a frame: the HUD's submit
  copies the present id from the `VkPresentIdKHR` in the app's present (DXVK passes its frame id
  there) and chains it. No sleeps, no waits, no fences on the hot path; `pNext` passed through
  untouched.
- **Anti-cheat.** Drawing doesn't change the layer's exposure category: the same `.so` is
  already in the game process for telemetry, and it's Linux code outside the Windows address
  space (MangoHud-class). The per-game kill switch (SIGHT steer 13) still applies, and those
  games get the desktop presenter (decision 16).

### 2.5 Trade-offs between the presenters

| | Game presenter (in-layer) | Desktop presenter (layer-shell) |
|---|---|---|
| VRR cadence | untouched (part of the game's frame) | an extra off-cadence flip per HUD update |
| Direct scan-out, tearing | kept | **lost while shown** (any overlay-layer surface) |
| FG frames | composited on real and generated, after FG | n/a (outside the game) |
| GPU cost in the game | one textured quad and a line graph per present, an upload when the image changes (tens of µs, inferred; spike H2) | none in the game; compositor composition every frame while visible |
| Rendering | GPUI off-screen in `quiver-sight`, composited by the layer | GPUI in `quiver-sight`, directly to the surface |
| In captures | configurable (section 2.12) | only in output captures; window captures never include it |
| OpenGL / native games | no (Vulkan only) | yes |
| Anti-cheat-flagged games | layer off: no | yes |
| gamescope as session compositor | yes (it's in the game image) | no |
| Windowed games | follows the swapchain | must track the window (layer-shell anchors to outputs) |
| Crash exposure | compositor bug = game crash; kept to a few hundred lines | none |

### 2.6 Choosing the presenter

Per session, `hud.presenter = auto | game | desktop`, default `auto`:

1. `auto` uses the **game presenter** when the session's layer has registered a swapchain in the
   session dir and composites successfully (its status block carries a heartbeat and a "drawn"
   counter).
2. Otherwise the **desktop presenter** (`quiver-sight` maps a click-through layer-shell surface
   with an empty input region, namespace `quiver-sight-hud`, on the game's monitor), and the Full
   preset's display line says "HUD on desktop: scan-out off".
3. If the layer stops drawing (error, device lost, a swapchain format it can't handle), quiverd
   switches to the desktop presenter within a second and logs why (`quiver hud status`).
4. With `captures = hidden`, a compositor screen share of the game's monitor or window moves
   the HUD to the desktop presenter for the duration of the share (section 2.12).

Toasts and the Sight panel stay on the desktop as SIGHT designed (they're transient or
interactive; losing scan-out for the six seconds a toast is up is acceptable).

### 2.7 Processes and data flow

```
 quiverd                                 quiver-sight (GPUI, ubi-ui)                game process
 ───────                                 ───────────────────────────                ────────────
 registry, stats, formatting   HUD model    one off-screen GPUI window   hud.image   Sight layer
 (layer ring, sensors, mods) ───────────▶   per session with the HUD  ────────────▶ (composite +
       ▲                       ubi-api,     on; renders on model change  /dev/shm,   live graph)
       │                       ≤ 10 Hz      → CPU readback → RGBA        2 buffers       │
       └───────────────────── present ring (/dev/shm, per present) ─────────────────────┘
```

- **quiverd** resolves the layout for the session's scope, subscribes internally at the HUD's
  rate, formats every value (units, precision, thresholds → levels; the same strings the CLI,
  agents and the app get, decision 3) and sends `Event::HudModel` to the `quiver-sight`
  connection that sent `HudAttach { session }`. The model is the resolved layout as rows of
  cells (text with a level, badges, bar fractions or segment stacks, sparkline points, a graph
  placeholder), plus anchor, offset, scale, background, the swapchain extent and format from
  the layer's status block, and the presenter in use. A few KiB, sent whole on each change.
- **quiver-sight** renders the model with `ubi-ui` components (section 2.9). For the game
  presenter it writes the result into `hud.image`; for the desktop presenter it draws the same
  view into a layer-shell surface.
- **The Sight layer** composites the current image and draws the graph (section 2.8).

### 2.8 The layer: a thin compositor

The layer knows nothing about metrics, layouts, text or fonts. Per present it does:

1. **Pick up a new image** if `hud.image`'s publish counter changed (section 3.8): copy the
   pixels into a host-visible staging buffer, validated against the descriptor, and record a
   `vkCmdCopyBufferToImage` into a per-swapchain `R8G8B8A8` texture. At 2 Hz and ≈ 0.6 MB for
   Standard at 2×, this is a small, rare copy.
2. **Composite**: one render pass with `LOAD_OP_LOAD` on the swapchain image, scissored to the
   HUD's rectangle; a textured quad at the descriptor's position with premultiplied-alpha
   blending and the descriptor's opacity.
3. **Draw the live graph** in each graph rectangle the descriptor names: the last `span_ms` of
   presented frame intervals from the layer's own present timestamps (a small ring in the
   layer's memory), as a line strip with an optional fill, coloured `line` / `warn` / `bad` by
   the descriptor's thresholds. In phase 2 a second series (base frame times) and a per-frame
   tint for generated frames come from the mod's ring (3.10).
4. Submit (waiting on the app's semaphores, chaining the latency present id, 2.4), signal the
   layer's semaphore, present with it.

Two pre-compiled SPIR-V pipelines (textured quad, coloured lines/quads), one vertex buffer per
swapchain image (rewritten for the graph each present, a few KiB). With the HUD off, or no
valid image, the hook is one atomic load and a pass-through: zero GPU work.

**Formats and colour.** The image is sRGB-encoded and premultiplied, exactly what GPUI produces
for a window. UNORM swapchains blend it as is, which matches how the app's own windows look.
sRGB swapchains sample it through an `_SRGB` view and blend in linear. `HDR10_ST2084`
swapchains decode to linear and encode to PQ at a configurable paper white (default 203 nits);
`EXTENDED_SRGB_LINEAR` (scRGB FP16) uses linear with 1.0 = 80 nits scaled to paper white (both
phase 2). Anything else (exotic formats, no `COLOR_ATTACHMENT` usage that can be added at
swapchain creation, a present queue family without graphics): the layer reports "can't draw"
and quiverd falls back to the desktop presenter.

**Resize.** The descriptor names the swapchain extent it was laid out for. After a resize the
layer stops drawing until an image for the new extent arrives (quiverd sees the new extent in
the status block and sends a new model; normally well under 100 ms).

**Devices and swapchains.** Per-device state (aco-dlss runs two `VkDevice`s; only the one that
owns the DXGI swapchain presents), per-swapchain resources rebuilt on recreate and on
`VK_ERROR_OUT_OF_DATE_KHR`, all resources destroyed with the swapchain/device. Multiple
swapchains: draw on the largest (the game window), configurable.

**Safety.** Every entry point catches panics (Rust: `catch_unwind`, `panic = "abort"` is not
acceptable inside a game) and permanently disables drawing for the session on the first one,
leaving telemetry and pass-through intact. Image descriptors are bounds-checked (3.16); a bad
one isn't drawn. The layer never blocks: if a per-image fence isn't signalled it skips the
upload and composites the texture it already has rather than wait.

**When `quiver-sight` isn't there, or the image is stale, the layer draws nothing.** "Stale"
means no `hud.image`, a heartbeat older than 3 s, or an image laid out for another extent. The
alternative, a minimal FPS-only fallback the layer draws itself, was rejected: it needs a text
renderer and a font in the game process, which is exactly the code this design removed; a HUD
that suddenly looks different is more confusing than a short gap; and stale numbers lie (a
frozen "158 fps" during a hitch). `quiver-sight` missing is a bug with a quick recovery: its
systemd unit restarts it, quiverd re-attaches the session, and the HUD comes back with the
first image. `quiver hud status` says why it's missing meanwhile.

### 2.9 quiver-sight: the off-screen HUD renderer

`quiver-sight` is already the GPUI process that owns Sight's UI (SIGHT §4.3), so the HUD is one
more view there: one **off-screen GPUI window per session** with the HUD on, sized to the HUD
at the pixel scale the swapchain implies, rendered on each HUD model change, read back to CPU
memory as premultiplied RGBA and written into `hud.image`. The view is built from `ubi-ui`
components with the app's theme tokens and fonts (Geist, Geist Mono), so the HUD, the Sight
panel, the app's game page and its layout preview are the same widgets.

- **Lifetime.** While the HUD is enabled for a session, `quiver-sight` runs for the whole
  session: quiverd starts it at launch when the HUD (or Sight) is on for the game, and starts
  it on `quiver hud toggle` if it isn't running. It serves every running session (normally one)
  and exits a little after the last one ends (SIGHT §5).
- **Why not in quiverd.** The rendering cost is small, but the process cost isn't: the RSS of a
  process with a Vulkan device is dominated by the driver, about 100 MiB on the AMD 890M iGPU
  and about 280 MiB on the NVIDIA RTX 5070 in the spike. That belongs in `quiver-sight`, which
  is a GPUI process anyway, not in the always-running daemon. quiverd stays the hub (registry,
  formatting, aggregation), so recordings, the CLI and agents don't depend on `quiver-sight`.
- **Which GPU.** Under Wayland GPUI picks the AMD iGPU by default, which is what we want: HUD
  rendering stays off the game's NVIDIA GPU, and doesn't wake the dGPU when no game uses it.
  Because the layer receives pixels through CPU memory, the producing GPU is irrelevant to it;
  no cross-device dma-buf import, no DRM modifiers, no external semaphores. The same path works
  headless (no Wayland connection needed for off-screen rendering).
- **GPUI.** GPUI 0.2.2 (the app's version) can't render off-screen. Upstream Zed GPUI has since
  split into crates and moved its Linux renderer from blade to wgpu; it already has
  `layer_shell.rs` (which Sight needs, SIGHT spike S2) and a `WgpuHeadlessRenderer`
  (`render_scene_to_image`) gated behind test/bench features, which renders opaque over a black
  clear. The plan builds on the **GPUI bump Sight already needs**: un-gate the headless wgpu
  renderer, make it render transparent and premultiplied, and port the spike's off-screen
  window and API (`App::open_offscreen_window`, `Window::render_now`; about 400 lines) to
  `gpui_linux`, with the aim of upstreaming it. The spike's blade changes are throwaway.

### 2.10 Spike results (GPUI off-screen)

Branch `spike/gpui-offscreen`, commit `792819b` (not merged): an `ubi-app` example that renders
the Standard HUD off-screen with the app's fonts and theme tokens.

- **Patch size.** gpui 0.2.2: +681/−26 (an off-screen `BladeTarget`, `OffscreenWindow`,
  `App::open_offscreen_window`, `Window::render_now`). blade: +15, to read back through
  host-cached memory; reading back from plain host-visible shared memory took 4.6 ms against
  about 10 µs host-cached.
- **Size.** Standard HUD 284×140 logical px.
- **Time.** Warm frame (layout + paint + GPU + readback) p50 0.2–0.7 ms; GPU + readback
  0.07–0.25 ms; GPUI layout and paint about 0.15 ms. At 2 Hz that is well under 0.1% of a core.
- **Environments.** Works headless and under Wayland, at 1× and 2×.
- **Process cost.** RSS dominated by the Vulkan driver: about 100 MiB on the AMD 890M iGPU,
  about 280 MiB on the NVIDIA RTX 5070 (section 2.9).

### 2.11 Desktop presenter

`quiver-sight` maps a click-through layer-shell surface (namespace `quiver-sight-hud`, overlay
layer, empty input region) on the game's monitor and renders the same HUD view into it directly.
The graph is a GPUI `canvas` fed from the layer's ring through quiverd at ≤ 10 Hz (or absent for
OpenGL games, where only system and mod metrics exist: "FPS" shows `—` without a frame source).
Commits are coalesced to the HUD's update rate, and the graph updates at most 10 Hz, to limit
the extra flips of section 2.1. Built in phase 2.

### 2.12 The HUD in captures

A setting, **"HUD in captures: Shown / Hidden"** (`hud.*.captures`), with the same scopes as
layouts (global, per game, per profile) and default **Shown**, like MangoHud. Each kind of
capture is handled where it happens:

- **Sight's own screenshots and clips** always offer both versions: the layer keeps the pre-HUD
  image (it copies the swapchain image before compositing when a capture is pending), so
  "with HUD" and "without HUD" are both exact. The setting picks the default.
- **OBS game capture** (obs-vkcapture) is itself a Vulkan layer and sees whatever is in the
  image when the present reaches it. Quiver sets the layer order in the launch environment:
  for Hidden, Sight's layer goes **below** obs-vkcapture (closer to the driver), so OBS captures
  before the HUD is composited; for Shown, above it. Both orders keep Sight below FG and
  post-processing layers (2.4). Spike H2 confirms how the loader orders an implicit capture
  layer relative to ours.
- **Compositor screen capture** (the portal / PipeWire screencast that OBS screen capture,
  Steam and browsers use) gets the HUD baked into the game's buffer. With Hidden, when Hyprland
  reports a screen share starting, quiverd moves the HUD to the desktop presenter with the layer
  rule `no_screen_share` on `quiver-sight-hud` (layer rules have it:
  `src/desktop/rule/layerRule/`), and moves it back when the share stops. The IPC events are
  **`screencast>>1,<type>`** / **`screencast>>0,<type>`** and, with the target,
  **`screencastv2>>1,<type>,<name>`** / `screencastv2>>0,…` (`ScreenshareSession.cpp`,
  `screenshareEvents`), where `<type>` is `monitor`, `window` or `region` and `<name>` is the
  monitor name (monitor, region) or the window title (window). quiverd uses `screencastv2` and
  switches only for shares of the game's monitor or window. Start fires when the first frame is
  copied; stop fires 500 ms after the last one.
  - **Cost.** No direct scan-out is lost by the switch: Hyprland already blocks direct scan-out
    during any screen share (`ScreenshareSession.cpp:177` sets `m_directScanoutBlocked` while a
    frame is pending; `Monitor.cpp` reports it as `DS_BLOCK_RECORD`; it clears only when no
    session is sharing). The VRR cadence cost of section 2.1 applies only while recording.
    Tearing isn't blocked by a share in Hyprland, so a game with an `immediate` rule does lose
    tearing for the duration of a Hidden recording.
  - **What the recording shows.** For a `window` share the result is clean: layer surfaces
    aren't part of a window capture, so the HUD simply isn't there. For `monitor` and `region`
    shares, Hyprland implements `no_screen_share` by painting the surface's box **solid black**
    over the already-composited output (`ScreenshareFrame.cpp`, `renderMonitor`), so the
    recording shows a black rectangle where the HUD is, not the game underneath (open question
    3 in 4.4).
- **One-shot screenshot tools** (grim, Omarchy's screenshot) read a single frame before any
  switch could happen, so they include the HUD whatever the setting. They also produce a
  `screencast` 1/0 pair half a second apart; the resulting short switch is harmless. For a
  screenshot without the HUD, use Sight's screenshot.

### 2.13 Why not a renderer in the layer

An earlier version of this design had the layer render the HUD itself: a pre-formatted cell
list from quiverd, a glyph atlas rasterized out of process, and a small cell renderer (text,
badges, bars, sparklines) in the game process. It kept fonts out of the game, but it meant a
second, smaller design system: its own text layout, its own widgets, a desktop presenter that
had to reproduce it in GPUI, and a look that would drift from the app's. Dear ImGui (MangoHud's
way) and egui in the layer have the same problem and also bring layout and font loading into
the game process. GPUI off-screen measured cheap enough (2.10) that the HUD can simply be app UI,
and the layer gets smaller still: one textured quad and a line graph, no text at all.

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
   └──◀ /dev/shm/…/<s>/hud.image ◀── written by quiver-sight ─────────┐             formatting,
                                                                      │             HUD model,
                                                                      │             recordings
 sensors: NVML, /proc, hwmon, power_supply, Hyprland IPC ─────────────┼─────────────▶   │
                                                                      │                 │ ubi-api
            app, CLI, agents, quiver-sight (panel, HUD renderer) ◀────┴─────────────────┘
```

Two planes:

- **Control plane** (JSON lines, existing transports): producer hello, descriptor declarations,
  low-rate samples (≤ 10 Hz coalesced), markers, ring announcements; consumer catalog queries
  and subscriptions; HUD layout changes; the HUD model quiverd sends `quiver-sight`.
- **Data plane** (shared memory): per-frame and per-event records in rings; the HUD image
  going the other way, from `quiver-sight` to the layer.

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
exists for people who want MangoHud-style numbers (decision 11).

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
the environment, below) and checks `hud.image`'s publish counter once per present. quiverd
discovers rings with inotify on the session dir. This replaces SIGHT §5's "memfd over an
abstract socket to quiver-sight": the hub is quiverd, so recording, the CLI and agents work
without the Sight UI running. (The HUD itself does need `quiver-sight`, section 2.9.)

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
  benchmarking agent gets its numbers in containers too. The HUD image then flows the other
  way: quiverd reads each newly published buffer of the host's `hud.image` and sends it to the
  bridge, which writes it into the container's session dir (≈ 0.6 MB per update for Standard
  at 2×, at the HUD's 2 Hz; acceptable for the dev-loop containers this path exists for).

**Cleanup.** quiverd removes the session dir when the session ends and sweeps stale ones at
start. Files are 0600; readers open them `O_RDONLY` and map `PROT_READ`.

### 3.8 Ring and HUD image layout

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

The ring's **status block** uses a classic seqlock: the writer increments `seq` to odd, writes,
increments to even; the reader copies and retries if `seq` changed or is odd.

The **HUD image** (`hud.image`) is written by `quiver-sight` and read only by the layer. quiverd
creates and sizes the file at session start (a 4 KiB header plus two buffers; tmpfs allocates
pages only as they're touched). It is double-buffered with a per-buffer seqlock and a publish
counter, so the layer never sees a half-written image and can tell "new image" with one load:

```c
#define QV_HUD_MAGIC   0x4944554856515551ull   /* "QUQVHUDI" */
#define QV_HUD_VERSION 1

typedef struct qv_hud_header {      /* offset 0, one 4 KiB page */
    uint64_t magic;                 /* written last at creation (release) */
    uint16_t version, header_bytes;
    uint32_t buffer_bytes;          /* capacity of each buffer incl. its descriptor; ≤ 4 MiB */
    _Alignas(64) uint64_t published;/* publish counter; buffer (published % 2) is current */
    uint64_t heartbeat;             /* quiver-sight, CLOCK_MONOTONIC_RAW ns, ≥ 1/s */
    uint32_t writer_pid;
} qv_hud_header;

typedef struct qv_hud_graph {       /* a rectangle the layer fills with the live graph */
    int32_t  x, y, w, h;            /* in image pixels */
    uint32_t span_ms;               /* 1000–10000 */
    float    min_ms, max_ms;        /* vertical range; max_ms = 0: auto */
    uint32_t line, fill, muted;     /* premultiplied RGBA8, resolved from theme levels */
    uint32_t warn_above_ms, bad_above_ms, warn, bad;
    uint32_t series;                /* 1 presented; later 2 base, 4 generated tint (phase 2) */
} qv_hud_graph;

typedef struct qv_hud_buffer {      /* at header_bytes + i * buffer_bytes */
    _Alignas(64) uint64_t seq;      /* odd while quiver-sight writes this buffer */
    uint32_t width, height, stride; /* pixels, pixels, bytes per row (multiple of 4) */
    uint32_t format;                /* 1 = RGBA8, premultiplied alpha, sRGB-encoded */
    float    scale;                 /* image pixels per logical px */
    uint32_t extent_w, extent_h;    /* the swapchain extent it was laid out for */
    int32_t  x, y;                  /* where it goes, in swapchain pixels (anchor resolved) */
    float    opacity;               /* applied by the layer, so fades need no re-render */
    uint32_t ngraphs;               /* ≤ 2 */
    qv_hud_graph graph[2];
    uint64_t model_rev;             /* the HUD model revision this image shows */
    uint8_t  pixels[];              /* at offset 256 within the buffer */
} qv_hud_buffer;
```

Writer (`quiver-sight`): pick `w = (published + 1) % 2`, increment `buffer[w].seq` to odd, write
descriptor and pixels, increment `seq` to even (release), then increment `published`
(release). Reader (the layer, once per present): load `published` (acquire); if it equals the
last value uploaded, do nothing; else read `buffer[published % 2].seq` (must be even), copy the
descriptor and pixels to its staging buffer, re-read `seq`; if it changed, discard and retry on
the next present. The layer treats every byte as untrusted (section 3.16).

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
session), the correlator, the HUD model builder (layout resolution and formatting, section
2.7), and any active recording.

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
| `HudGet/HudSet { scope, layout?, on?, presenter?, captures? }`, `HudToggle { session }`, `HudNext { session }` | | layouts revisioned like settings; `captures` = `shown` / `hidden` (section 2.12) |
| `HudAttach { session }` | ok | `quiver-sight` only: the connection then receives `Event::HudModel` for that session (section 2.7) |
| `HudStatus { session }` | presenter in use and why, image age, layer draw counter, last error | `quiver hud status` |

Events: `PerfCatalogChanged { session }`, `PerfUpdate` (per subscription only), `HudModel`
(per attachment only), `HudChanged`.

`Event::PerfUpdate` and `Event::HudModel` are the first events that aren't broadcast to every
`Subscribe`r: today's `Subscribe` is a single `broadcast` channel. They need a per-connection
sender in the connection loop (`crates/ubid/src/main.rs`), which is a small change but a new
pattern.

### 3.12 Consumers

**The HUD** (section 1): quiverd resolves the layout, subscribes internally at the HUD's rate,
formats values (units, precision, thresholds → levels) and sends the resulting HUD model to
`quiver-sight`, which renders it with the app's components for the active presenter (section
2.7).

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
runs); every session with an active layer gets only the summary by default (decision 13).

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
  bidi overrides removed, length-capped; rendered as plain text by GPUI in `quiver-sight`, never
  interpreted as markup, format strings or paths. No string reaches the game process: the layer
  only ever receives pixels.
- **Numbers**: NaN/∞ dropped; timestamps outside `[session start − 1 s, now + 1 s]` dropped;
  values outside 1000× a declared range clamped for display and flagged.
- **Rates**: JSON ≤ 10 messages/s per producer after coalescing (excess: latest wins), declare
  ≤ 1/s after the first second, marks ≤ 20/s; rings ≤ 20,000 records/s per producer (quiverd
  stops draining a ring above that and marks it `invalid(rate)`).
- **Rings**: readers map read-only; every slot is checked (commit sequence, metric index in
  range, `nfields` ≤ 12, swapchain id churn bounded); a ring with more than 1% invalid slots over
  10 s is disabled for the session with a logged reason. A writer can only lie to itself.
- **The game side reading Quiver's output**: the layer validates every HUD image descriptor
  before use (magic and version; width, height ≤ 2048; `stride ≥ width × 4` and
  `stride × height + 256 ≤ buffer_bytes`; the image rectangle and graph rectangles inside the
  swapchain and the image; finite floats; `ngraphs ≤ 2`). A bad descriptor isn't drawn. The
  image holds rendered pixels of display values only; nothing secret is rendered into it.
- **Isolation**: a producer's failure never stalls another; the HUD model builder runs on its
  own task with a deadline; a slow consumer gets coalesced updates, never back-pressure into
  producers; `quiver-sight` falling behind only makes the HUD image older (section 2.9).
- **Privacy**: recordings stay local; mod strings in recordings are already sanitized; agents
  reading recordings see what the user sees.

---

## 4. Phasing, spikes, risks

### 4.1 Spikes

| # | Spike | Question | De-risks |
|---|---|---|---|
| H1 | **Desktop HUD cost** | With AC Origins fullscreen on Hyprland (XWayland and Wine-Wayland), map a click-through overlay-layer surface; record `hyprctl monitors -j` blockers, and frame pacing (layer present timestamps, VRR on) with HUD commits at 0, 2, 10, 60 Hz; repeat with `direct_scanout = auto` and an `immediate` rule | the fallback's update cap; how much the fallback costs |
| H2 | **In-layer compositor** | A minimal compositor in the Sight layer (S3's base): textured quad from `hud.image` plus the live graph; DXVK (Origins), vkd3d-proton (a D3D12 title), aco-dlss FG on (two devices, two presents per frame) and Reflex on (no new DXVK frame-id warnings; latency-submission present id chained); GPU cost per present at 1600p, with and without an upload; UNORM/sRGB/HDR10/scRGB swapchains; loader layer order relative to vkBasalt, lsfg-vk and obs-vkcapture | the default presenter, capture ordering |
| H3 | **Wine shared memory** | A PE test program maps `Z:\dev\shm\…` (and `\\?\unix\…`) with `CreateFileMappingW`; a Linux reader sees stores with the ring protocol at 10k records/s; same inside pressure-vessel for a Steam copy (is `/dev/shm` the host's?) and in a podman container (relay path) | the data plane |
| H4 | **Clocks and correlation** | QPC = `CLOCK_MONOTONIC_RAW`/100 on the Proton builds in use (GE-Proton10, Proton 9/10, Experimental); DXVK's `VkPresentIdKHR` vs the mod's present sequence with FG on; offset fit stability across resize and alt-tab | per-frame views, latency to present |
| H5 | **Reflex reports** | `NvAPI_D3D_GetLatency` through dxvk-nvapi with aco-dlss's injected Reflex: fields filled, units, clock domain vs QPC; and `vkGetLatencyTimingsNV` from the layer for the same frames | latency metrics |
| H6 | **Sensors** | = SIGHT S9: NVML on the laptop without waking the dGPU outside sessions; hwmon CPU package temp; battery | system producer |
| H7 | **Keys** | Shift+F3 / Ctrl+Shift+F3 as Hyprland binds over XWayland and Wine-Wayland games; does a consumed Hyprland bind hide Shift_L+F2 from MangoHud's key polling; remapping MangoHud's `toggle_logging` via `MANGOHUD_CONFIG` | key plan |
| H8 | **GPUI off-screen on the bumped GPUI** | The GPUI 0.2.2 half is done (2.10). Port it onto upstream's wgpu `WgpuHeadlessRenderer`: transparent, premultiplied output; readback through a mappable buffer without the 4.6 ms penalty seen with blade's shared memory; timings against the spike; RSS; adapter choice (the iGPU under Wayland and headless); Geist Mono legibility at 0.75× on 1080p | how the HUD is drawn |
| H9 | **Capture switching** | With Hidden: a portal screencast of the game's monitor and of its window (OBS, gpu-screen-recorder in portal mode); time from `screencastv2>>1` to the HUD leaving the game image, and how many captured frames still show it; the black box in monitor captures; obs-vkcapture with both layer orders | decision 15 |

### 4.2 Phases

**Phase 0** (no UI): spikes H1–H5 and H8 (with SIGHT S2, S3, S12, since the GPUI bump, the
layer and session ids are shared), the session dir and `QUIVER_PERF_DIR`, the ring and HUD
image formats as a shared crate (`ubi-perf`: ring reader/writer, `hud.image` reader/writer,
descriptor types, validation) with a C header generated or checked against it.

**Phase 1 (MVP): a MangoHud replacement for Quiver launches, with mods' headline numbers.**

- GPUI bump with layer-shell and the off-screen window (SIGHT S2 + H8), and the `ubi-ui` crate
  extracted from `ubi-app` (theme, fonts, the components the HUD uses).
- Sight layer: present ring + status block; **compositor** (image upload, quad, live graph,
  sRGB/UNORM; HDR swapchains use the desktop presenter until phase 2).
- `quiver-sight`: the HUD view, the off-screen renderer writing `hud.image`, `HudAttach`;
  started per session while the HUD is on. **Desktop presenter** for games without the layer
  (the bump brings layer-shell anyway, so the fallback costs little here).
- quiverd: perf registry, layer ring reader, system producer (NVML, `/proc`, hwmon,
  power_supply), display producer (Hyprland IPC), aggregation (1 s, 10 s, session; p1low),
  canonical metrics for `frame.presented`, HUD model builder and formatting, `hud.image`
  creation, the presenter choice.
- Presets FPS only, Compact, Standard, Full (without latency breakdown); placement, opacity,
  scale; per game and per profile layout selection (built-in presets only; custom layouts as
  TOML files).
- Captures: the setting, and the obs-vkcapture layer order it implies.
- Keys: Shift+F3 / Ctrl+Shift+F3 binds while a Quiver game is focused; `quiver hud toggle|next|
  layout|status`; MangoHud logging remap when Quiver launches MangoHud.
- **Mod metrics over JSON only**: MOD-CONFIG's SDK pipe (its phase 2 transport, pulled forward:
  `hello`/`welcome`, `metrics`, `m`, `mark`), `gauge`/`counter`/`state`/`text`/`event`;
  readouts folded into metrics. aco-dlss publishes FG state, upscaler, internal resolution,
  Reflex mode, and **a counter of real frames** → `fps.base` as a rate (mean only, no base 1%
  low yet) and a **gauge of mean render latency** it computes from Reflex reports.
- `PerfCatalog`, `PerfSubscribe`, `quiver perf watch|catalog`; `PlaySession.perf` summary.
- Sight's Performance section shows the same numbers (if Sight's MVP is there; the HUD doesn't
  depend on the panel, only on `quiver-sight`).

**Phase 2**

- Mod rings: `ring_req`, frame/series records, `fps.base` percentiles and frame time, generated
  frame tint and base series in the graph (the layer reads the mod's ring), correlation, Reflex
  latency breakdown (`latency.frame` role, `reflex_us` alignment).
- Captures: the screencast-driven switch for Hidden (H9); Sight's screenshots and clips with
  and without the HUD (with SIGHT's capture work, v2).
- HDR10 and scRGB in the layer compositor.
- Recordings, `perf record|list|show|compare|export`, comparisons in the app's session history,
  benchmark markers, agent workflow; bridge relay for containers.
- HUD layout editor and metric picker in Sight and the app.

**Phase 3**

- gamescope/`mangoapp` data reader, lsfg-vk awareness, a Streamline producer for native DLSS-G
  titles (base vs presented without a Quiver mod), layer-side Reflex timings for games with
  native Reflex, present-wait display times, per-frame CPU/GPU split from the layer
  (timestamp queries).
- Declarative mod widgets, only if a real mod needs one (decision 17).

### 4.3 Risks

- **The HUD now depends on the GPUI bump.** The off-screen path needs upstream GPUI (wgpu,
  `gpui_linux`) plus a patch that isn't upstream yet, and the bump touches the whole app
  (SIGHT's "GPUI churn"). Mitigation: H8 early; keep the patch small (≈ 400 lines) and pinned in
  a fork until it's upstream.
- **`quiver-sight` memory for the whole session** (≈ 100 MiB on the iGPU, ≈ 280 MiB if it ends
  up on the NVIDIA GPU). Mitigation: prefer the integrated adapter explicitly (wgpu's low-power
  preference) rather than relying on GPUI's default, so it holds outside Wayland too.
- **Code in the game process.** A compositor bug crashes the game. Mitigation: the layer does no
  text, layout or parsing; panics disable drawing; descriptors are bounds-checked and fuzzed;
  per-game kill switch.
- **Fallback cost.** When the desktop presenter is in use (OpenGL, anti-cheat, layer failure,
  Hidden during a screen share), the VRR cadence and scan-out costs of 2.1 apply. Mitigation:
  update caps (H1) and the Full preset's display line says so.
- **Capture ordering relies on the Vulkan loader** placing obs-vkcapture where Quiver asks
  (H2), and on Hyprland's screencast events arriving before many frames have been captured (H9).
- **Staleness.** If `quiver-sight` stalls, the HUD disappears after 3 s rather than freezing;
  the user sees a gap, `quiver hud status` explains it.

### 4.4 Open questions

1. **Image size cap.** 4 MiB per buffer covers Standard comfortably (≈ 0.6 MB at 2×). Does Full
   at scale 2.5 on a 4K swapchain fit, or should the cap grow (tmpfs pages are allocated
   lazily) or the image be rendered at a lower scale for the layer to scale up?
2. **Upstreaming.** Whether Zed takes the off-screen window API, or Quiver carries it in a fork
   alongside any layer-shell fixes.
3. **Hidden during a monitor share.** Hyprland's `no_screen_share` paints the HUD's box black in
   monitor and region captures (2.12). Choices: accept the black box; hide the HUD entirely
   while a monitor or region share of the game's monitor runs (window shares stay clean
   either way); or patch the user's Hyprland fork to leave such layers out of the captured
   image (which means compositing the capture separately from the output). To decide after H9.
4. **HDR blending.** Blending an sRGB-authored image into a PQ swapchain at a fixed paper white
   is what MangoHud does; whether that looks right next to the game's own HDR UI is for H2.

---

## 5. Decisions

Decided by the user, 2026-11-16. Each: the decision, why, and what was rejected.

1. **Where the HUD draws: in the game (Sight Vulkan layer), automatic fallback to a
   layer-shell surface.** Why: above all it keeps VRR pacing (no off-cadence compositor flips),
   and it keeps direct scan-out and tearing available, which any desktop surface over the game
   disables in Hyprland (2.1). Rejected: desktop layer-shell only (SIGHT's original plan);
   in-layer only (no HUD for OpenGL or anti-cheat games); both with a manual per-game choice and
   no automatic fallback.
2. **How it's drawn in the game: the layer composites a GPUI-rendered HUD image and draws the
   live frame-time graph itself.** The image is rendered off-screen by GPUI in `quiver-sight`
   with `ubi-ui` components, theme and fonts, and shared through a double-buffered,
   sequence-checked shared-memory buffer (3.8). Why: visual consistency with the main app, and
   the layer gets smaller. Rejected: a cell renderer with an out-of-process glyph atlas in the
   layer (this document's earlier pick, 2.13); Dear ImGui in the layer; egui in the layer.
3. **quiverd formats values and resolves layouts.** Why: the CLI, agents, the app and the HUD
   show the same strings. Rejected: the layer reading rings and formatting itself; quiverd
   rendering pixels (pixels now come from `quiver-sight`, from quiverd's strings).
4. **Keys: Shift+F3 toggles, Ctrl+Shift+F3 cycles presets**, bound only while a Quiver game is
   focused, plus a hyprpad snippet; MangoHud's `toggle_logging` is moved off Shift_L+F2 when
   Quiver launches MangoHud. Why: free of every known default and next to Sight's chord.
   Rejected: an always-bound Super chord; taking over MangoHud's Shift_R+F12; no default.
5. **Quiver's HUD by default, MangoHud a per-game option**, with MangoHud-compatible CSV export.
   Why: own HUD where it matters, MangoHud for OpenGL, gamescope and existing setups. Rejected:
   MangoHud as the default; dropping MangoHud integration.
6. **Producer-namespaced metric ids plus canonical roles** that layouts bind to. Why: "base FPS"
   works for any FG mod without new layouts, and mods keep their own extras. Rejected:
   free-form ids only; a closed list of metrics.
7. **JSON for ≤ 10 Hz values, shared-memory rings for per-frame and per-event data**, bridge
   relay when quiverd can't see the session dir. Why: each kind of data on the transport that
   fits its rate. Rejected: everything as JSON over the pipe; everything in shared memory.
8. **The bridge creates mod rings** on the SDK's request. Why: it's always in the game's mount
   namespace and can enforce a budget. Rejected: the SDK creating its own file; quiverd
   creating them (fails in containers).
9. **Clock: `CLOCK_MONOTONIC_RAW` ns; QPC = RAW/100**, checked continuously against present
   correlation with a fitted mapping as fallback. Why: verified for Wine 10, guarded against a
   future change. Rejected: correlation-only alignment; trusting producers' clocks.
10. **quiverd computes derived statistics** (FPS, lows, percentiles, latency spans) from raw
    records; producers may add gauges only they can measure. Why: one definition of every
    number. Rejected: producers publishing finished numbers; both side by side.
11. **"1% low" = 1000 ÷ the p99 frame time** (CapFrameX P1), stated in the help text, with the
    average-of-slowest-1% variant available. Rejected: the average as the default; both in
    Standard.
12. **Layout scope: global + per game + per profile**, toggle state remembered per game. Why:
    DLSS and Vanilla profiles want different HUDs. Rejected: per game only; global only.
13. **A performance summary on every session, raw records on request** (and in benchmark runs).
    Why: ~1 KB per session buys DLSS-vs-Vanilla history. Rejected: raw records for the last N
    sessions; nothing unless asked.
14. **MOD-CONFIG readouts are folded into metrics** (`[metric.*]`, `quiver_readout` kept as
    sugar, `OptionsReadout` replaced by `PerfCatalog` / `PerfSubscribe`). Why: one catalog, one
    rate limit, one place to look. Rejected: readouts separate from metrics.
15. **HUD in captures: configurable**, "Shown / Hidden", same scopes as layouts, default Shown
    (2.12). Sight's screenshots and clips always offer both; OBS game capture follows the layer
    order Quiver sets; compositor screen capture with Hidden switches to the layer-shell
    fallback with `no_screen_share` for the share's duration. Why: some people want the HUD in
    benchmark videos and some never want it in recordings. Rejected: always in captures with
    only Sight's screenshot able to hide it (this document's earlier pick); always hidden from
    Sight's screenshots; never hidden.
16. **Anti-cheat-flagged games: layer off, HUD on the desktop presenter.** Rejected: no HUD;
    layer on anyway.
17. **Mods provide data only**; declarative mod widgets later, only if a real mod needs them.
    Rejected: mods drawing into the HUD region.
18. **The HUD is composed in `quiver-sight`** (GPUI off-screen). quiverd still owns the
    registry, formatting and aggregation and sends a compact HUD model. Why: the renderer is
    GPUI, and a Vulkan device's 100–280 MiB RSS belongs in the GPUI process, not the
    always-running daemon (2.9). Rejected: composing in quiverd (this document's earlier pick);
    a separate `quiver-hud` helper per session (a second GPUI process).

---

## 6. Changes made to the other design documents

Applied with these decisions (2026-11-16). SIGHT's and MOD-CONFIG's own steers are still open;
only what the HUD decisions force was changed, and the SIGHT steers whose options they affect
are flagged there.

- **SIGHT.md**: steer 7 superseded (already noted there, now with the decided design); §3.5's
  rows point here (own HUD in the MVP, MangoHud optional, readouts as metrics, perf logging as
  recordings); the corner HUD is the `clock` HUD layout; §4.3 items 3 and 4 note that the
  Sight layer composites the HUD from the MVP, separately from the dma-buf presenter 2; §5's
  process model replaces the layer's abstract socket with the session dir read by quiverd and
  gives `quiver-sight` its HUD-rendering role and session lifetime; §8's MangoHud, vkBasalt and
  OBS rows, §10's M8 and §11/§12's spikes and phases follow; steers 2, 3, 11, 13 and 16 carry
  notes on how the HUD decisions affect their options.
- **MOD-CONFIG.md**: readouts are `[metric.*]` (SDK-only entries, the aco-dlss example, live
  apply); the SDK is protocol 2 with the metrics messages of 3.6 and the calls of 3.14;
  `OptionsReadout` and `Subscribe { readouts }` are replaced by `PerfCatalog` / `PerfSubscribe`;
  C2's `readouts` becomes a list of metric ids; C10 is updated; phase 1 notes that the HUD MVP
  pulls the SDK pipe forward; steer 10 carries a note.
