# Stereo UI: the app in each eye, with depth (design notes, draft)

Status: spiked 2026-11-19 (a prototype behind `QUIVER_STEREO_UI`, off by default). The packed
frame is verified off-screen ([§9](#9-evidence-2026-11-19)); the live window path (Wayland
viewport, xdg tag, Hyprland events) compiles and is unit-tested but has **not** been seen on a
stereo output yet. Companion to [SIGHT.md §15](SIGHT.md#15-stereo-the-hud-and-the-panel-in-each-eye-2026-11-19)
(Sight in each eye), which shares this document's layout names and depth vocabulary. Sight's
rung table is to be added in [§4](#4-depth-vocabulary-shared-with-sight).

**Why.** The person wears an XReal Air 2 Ultra (a flat side-by-side output) and a Quest 3
(OpenXR) under HypXRland, their Hyprland fork. HypXRland already lifts whole windows (its depth
desktop), so the focused Quiver window floats above the wallpaper. Inside the window everything
is still one flat card. The library is a natural fit for a few depth steps: hero art on the
page, cover tiles a little above it, the selected tile above them, dialogs on top. HypXRland's
side of this exists: a window may draw both eyes itself and tag its toplevel `stereo:sbs`, and
the compositor un-packs it per eye. This is the app's side.

## 1. How it works

```
GPUI paint ──► Scene (each paint op tagged with a depth) ──► pack_side_by_side ──► 2W x H frame
                                                               │ left eye: x + left(depth)
                                                               │ right eye: W + x + right(depth)
Wayland: buffer 2W x H ──wp_viewport──► surface W x H ──xdg tag "stereo:sbs"──► HypXRland
                                                         halves the surface: left half → left eye
```

- **Depth at paint time.** `Window::with_stereo_depth(d, f)` tags every primitive `f` paints
  with depth `d`; `Styled::stereo_depth(d)` (a `Style` field) does it for an element and its
  children, and refines like any style, so `.hover(|s| s.stereo_depth(0.6))` works. The tag is
  stored per paint operation in the `Scene` (`paint_depths`) and survives cached-view replays.
  An inner depth replaces the outer one; depths don't add.
- **Packing at present time.** `Window::set_stereo_packing(Some(StereoPacking { shift }))`
  makes `Window::present` call `Scene::pack_side_by_side(eye, shift)`: the finished scene's
  paint operations are replayed twice into a new scene, every primitive moved right by its
  eye's origin plus `shift(depth)[eye]`, its content mask moved with it and **clipped to its
  eye**, so nothing crosses the seam. Quads, shadows, underlines, sprites (glyphs, icons,
  images), paths (tiles and curves) and surfaces are all handled; animated sprites are packed
  at their current pose. The renderer is unchanged: it draws one scene into one 2W-wide target.
- **The drawable.** `PlatformWindow::set_stereo_side_by_side(true)` doubles the drawable's
  width while the window keeps its size. The Wayland window keeps `wp_viewport`'s destination at
  the window's logical size, so the surface stays W x H and only the buffer is 2W (GPUI already
  creates a viewport for fractional scaling). The off-screen window renders 2W x H. X11 and other
  platforms say no, and the app stays mono.
- **The tag.** The Wayland window sends `xdg_toplevel_tag_v1.set_toplevel_tag("stereo:sbs")`
  when packing starts and `"stereo:mono"` when it stops: exactly HypXRland's strings
  (05-configuration §8.6, "The tag").

**Where it hooks** (all marked `Quiver:` in the vendored GPUI):

| Piece | File | Function |
|---|---|---|
| depth per paint op | `third_party/zed/gpui/src/scene.rs` | `Scene::set_stereo_depth`, `paint_depths`, `replay` |
| packing | same | `Scene::pack_side_by_side`, `Primitive::translated`, `PaintedPath::translated` |
| path tiles | `gpui/src/path_winding.rs` | `PathTiles::translated` |
| scope, packing switch, present | `gpui/src/window.rs` | `Window::with_stereo_depth`, `set_stereo_packing`, `present`; `StereoPacking` |
| style | `gpui/src/style.rs`, `styled.rs`, `elements/div.rs` | `Style::stereo_depth`, `Styled::stereo_depth`, `Interactivity::paint` |
| platform | `gpui/src/platform.rs` | `PlatformWindow::set_stereo_side_by_side` (default: unsupported) |
| Wayland | `gpui_linux/src/linux/wayland/window.rs`, `client.rs` | `drawable_size`, `set_stereo_side_by_side`; the `xdg_toplevel_tag_manager_v1` global |
| off-screen | `gpui_linux/src/linux/offscreen.rs` | `draw` at 2W |
| the app | `crates/quiver-app/src/stereo_ui.rs` | detection, rungs, tags, `shot` |

**Alternatives looked at.**
- *Buffer scale.* `wl_surface.set_buffer_scale` scales both axes, so a 2W-wide buffer at scale 2
  is also 2H tall. Only a viewport gives a 2:1 buffer in a 1:1 surface. HypXRland crops in
  surface UV after the viewport's source rectangle (`calculateUVForSurface`, research/24 F7),
  so a 2W buffer behind a W surface gives each eye exactly W buffer columns: 1:1 at scale 1.
- *Two renders with two viewports.* The renderer could draw the scene twice with
  `set_viewport` per eye, but GPUI's fragment shaders (rounded quads, path coverage) read the
  framebuffer position, so the right eye would need a second, offset copy of every bound
  anyway. Packing the scene keeps the renderer untouched.
- *A per-instance depth on the GPU.* A depth field in every instance struct and a per-eye
  uniform would avoid the CPU replay, but changes every primitive's layout and every shader. The
  replay is cheap enough ([§8](#8-cost)); this is the upgrade if it ever isn't.
- *Painting the element tree twice.* GPUI's paint pass has side effects (hitboxes, listeners,
  element state); doing it per eye is a much larger change than replaying the result.

## 2. Detection: when to pack

Packing is right only on an output that presents two panes. On anything else a 2W frame would
show squeezed and doubled, so the default is mono.

- **Hyprland / HypXRland (built).** `hyprctl -j monitors` has `"stereo": "sbs"` (plus
  `scanoutWidth`, `scanoutHeight`, `stereoContent`, `stereoComposites`) on a packed output and
  no such field elsewhere (`HyprCtl.cpp`, `getMonitorData`). Our window is the client with our
  pid in `hyprctl -j clients`; its `monitor` picks the monitor. Both a flat stereo output
  (`monitor = …, stereo:sbs`, the XReal) and an XR monitor under the depth desktop (§8.9,
  `stereo: sbs`) report this, so one check covers both headsets. The app re-checks on the
  event socket's `monitoradded`/`monitorremoved`/`movewindow`/`moveworkspace`/`workspace`/
  `focusedmon`/`openwindow`/`configreloaded`/`fullscreen` (v1 and v2 names), coalesced over
  120 ms, and every 5 s regardless (a stereo pack can come and go on the XReal's mode watch
  without an event the app can rely on).
- **`wl_output` (not usable).** On a stereo output `wl_output.mode` and `xdg_output` report the
  *pane* (1920x1080) on purpose (§8.5, "What `wl_output` … report"). `wl_surface.enter` tells us
  which output we're on, not that it is packed.
- **`wlr-output-management` (a heuristic, not built).** It still reports the real mode list:
  the current mode is 3840x1080 while `xdg_output`'s logical size is 1920x1080 at scale 1. "The
  current mode is exactly twice the logical width at scale 1" is a compositor-neutral hint, but it
  only means anything on HypXRland, which has the precise answer over IPC, and GPUI doesn't bind
  that protocol, so it would be a second Wayland connection for a guess. Not worth it.
- **Other compositors: no-op.** `QUIVER_STEREO_UI=sbs` stays mono off Hyprland.
  `QUIVER_STEREO_UI=sbs-always` packs regardless, for testing and for a 3D TV a compositor
  can't describe.
- *Not handled:* a rotated stereo output (HypXRland turns depth off there; the app should too,
  from the monitor's `transform`); several windows (only the main window packs).

## 3. Layouts: `sbs` only

The spellings are `quiver_perf::stereo::Stereo`'s (HypXRland's): `mono`, `sbs`, `hsbs`, `tab`,
`htab`. The app uses **`sbs`**: each eye gets the window's full width, so text is rendered at
1:1 in each eye. **Why not `hsbs`:** each eye would get half the horizontal samples, squeezed
2:1. That's ~21 px/degree horizontally on the XReal (HypXRland §8.6), and GPUI would have to
rasterise text at half width: 13 px UI text would be unreadable. `hsbs` exists for game mods
that can't grow their swapchain. `tab`/`htab` have no presenter for a flat panel in HypXRland
yet. On the depth-desktop tier the pane shape doesn't depend on the declaration (§8.9), and on
the quad-pair tier `sbs` changes the panel's shape (§8.8), which doesn't apply to a windowed app
([§6](#6-the-quest-path)).

## 4. Depth vocabulary (shared with Sight)

**Depth** is HypXRland's unitless rung: 0..1 above the plane it is drawn on (for the app, the
window's own plane; the compositor lifts the window itself). Nothing goes behind the plane:
crossed (in front) disparity is the comfortable direction (research/24 §8.2, Shibata 2011).

**Per-eye pixels** use HypXRland's formula and defaults (05 §8.7, research/24 §8.1):
rise `h = depth · 0.12 m · strength`; total disparity `t = b·h / (D − h) · P/W` px with
`D = 1.5 m`, `W = 1.6 m`, `b = 0.063 m` and `P` the pane's width in device pixels; capped at 1°
(≈31 px total at P = 1920). `t` is rounded to whole pixels; the **left eye's content moves right
by ⌈t/2⌉ and the right eye's moves left by ⌊t/2⌋**. `stereo_ui::disparity_px` and `eye_shifts`;
the unit test checks HypXRland's own table (0.2 → 1.2 px, 0.6 → 3.8, 0.8 → 5.2, 1.0 → 6.6).

**The app's rungs** (`stereo_ui::depth`, P = 1920):

| Rung | Depth | Total px (exact) | Left / right eye | What |
|---|---|---|---|---|
| `HERO` | 0.0 | 0 | 0 / 0 | hero art, page background: the window's plane |
| `CHROME` | 0.15 | 1 (0.92) | +1 / 0 | top bar |
| `TILE` | 0.3 | 2 (1.86) | +1 / −1 | cover tiles (art and caption) |
| `FOCUSED` | 0.6 | 4 (3.81) | +2 / −2 | the focused (controller/keyboard) or hovered tile |
| `DIALOG` | 0.8 | 5 (5.17) | +3 / −2 | overlays, dialogs, toasts |

Steps between rungs are 1, 1, 2 and 1 px: under the ~3 px foveal-fusion step
([§5](#5-comfort)). They compose with the compositor's lift: a focused, windowed Quiver is at
0.6 already, so a dialog sits at roughly 0.6 + 0.8 of HypXRland's scale above the wallpaper,
≈9 px total, still well under 1°. `QUIVER_STEREO_UI_STRENGTH` (0..2) multiplies the rise.

*Sight's rungs go here* (the caller's table: HUD, panel, toasts over a game). Sight draws in the
game's own frame, where the game's plane is 0.

## 5. Comfort

From HypXRland (05 §8.7, research/24 §7.3, §8.2):
- **Steps, not totals, are what bind.** Two things in the foveal field at different depths may
  differ by about 0.1° ≈ **3 px total** at these numbers; the whole span should stay
  single-digit pixels; the hard ceiling is **1°** (≈31 px total, ≈15.7 px per eye).
- **A small ladder, not a continuum.** Four or five rungs, used consistently.
- **Discrete events, not hover.** HypXRland deliberately doesn't change depth on hover:
  pointer-driven depth changes are vergence micro-adjustments and the likeliest source of
  strain. The prototype pops the hovered tile anyway (it was asked for, and it is one 2 px
  step per tile change, not continuous). Recommendation for the full version: pop on focus
  (controller/keyboard selection) only, and keep hover as the ring it is today, or pop after a
  dwell.
- **Frame edges.** An element raised against the window's left or right edge is cut
  differently in each eye. The top bar spans the window at 1 px: a 1-px sliver at most. A full
  version should clamp an element's shift to its margin from the window edge (HypXRland's
  `depth_edge_slack` rule, 2 px).
- **Integer pixels.** Measured: whole-pixel shifts keep glyphs and art bit-identical to mono
  in each eye; fractional shifts resample them ([§9](#9-evidence-2026-11-19)). HypXRland applies
  its window disparity in floating point because it wants a continuous ladder; ours is a few
  fixed rungs, so whole pixels cost nothing. The rounding moves the rungs by ≤ 0.2 px.

## 6. The Quest path

- **Depth desktop on (the default, §8.9).** The XR monitor composites once per eye and reports
  `stereo: sbs`, so the app packs. HypXRland un-packs a stereo window **per surface inside each
  eye's composite**, so it works **windowed**, alongside other windows, with its chrome and ray
  cursor; the window's declaration isn't consulted a second time at submission. This is the
  path the app targets.
- **Depth desktop off (`openxr:depth_desktop = 0`, §8.8).** The XR monitor isn't packed, so
  `stereo` is absent and the app stays mono. The quad-pair tier only engages for a stereo
  window that **covers the whole monitor** (fullscreen at the monitor's size); a windowed app
  would send the left half of the desktop to the left eye. Packing there would need
  `hyprctl -j openxr` and a fullscreen check; not worth building for a launcher.
- **Scale.** XR monitors keep `openxr:default_monitor_scale`, which may not be 1. The app's
  shifts are whole *device* pixels from `P = width × scale`; the compositor's own geometry for an
  XR quad (its metres) isn't visible to the app, so the rungs are approximate there.
- **Foveated streaming** (WiVRn) resamples after composition: 1–2 px of disparity in the
  periphery may soften (research/24 §8.2). Depth reads strongest where you look.

## 7. The Hyprland rule, and the two ways to tag

HypXRland never engages stereo for a window on its own; a rule must opt in.

- **The xdg tag (compositor-neutral; the recommendation).** The Wayland window tags itself
  `stereo:sbs` / `stereo:mono` (`xdg-toplevel-tag-v1`). The person's config
  (`~/.config/hypr/hyprland-xr.lua`) has **no** `xdg_tag` rule today; it needs one line:

  ```lua
  hl.window_rule({ name = "stereo-xdg-tag", stereo = "auto", match = { xdg_tag = "^stereo:" } })
  ```

  Under `auto` the tag picks the layout, skips the fullscreen gate and `stereo:mono` turns it
  off. It is one Wayland request, arrives in order with the buffers (no frame where a 2W buffer
  is shown unsplit, beyond the first), and any compositor that adopts the convention gets it.
- **The Hyprland window tag (works with today's config).** The config maps window tags
  `stereo-<layout>` to `stereo <layout> always` (the route Torchbearer and the mpv script use).
  The app finds its window by pid in `hyprctl -j clients` and sends
  `dispatch hl.dsp.window.tag({ tag = "+stereo-sbs", window = "address:0x…" })`, falling back to
  the classic `dispatch tagwindow +stereo-sbs address:0x…`, and `-stereo-sbs` to clear it. It
  lags the buffer by an IPC round trip (a frame or so of an unsplit 2W buffer when packing
  starts), it's Hyprland-only, and it's a compositor write from the app.
- **The prototype sends both** (`QUIVER_STEREO_UI_TAG=xdg` for the xdg tag alone), so it works
  with the config as it is and with the rule above. They agree: both resolve to `sbs`, and both
  are withdrawn together. The full version should send only the xdg tag once the rule is in the
  person's config.

## 8. Cost

Measured off-screen (`QUIVER_STEREO_SHOT`), the demo library at 1920x1080, scale 1, on this
machine's discrete GPU (RTX 5070 Mobile; GPUI's wgpu, Vulkan):

| | mono | sbs |
|---|---|---|
| render + readback (p50) | 5.3 ms | 14.1 ms |
| `render_now`: layout + paint + pack + render + readback (p50, debug build) | 40 ms | 91 ms |
| render + readback (p50, release build) | 1.3 ms | 3.7 ms |
| `render_now` (p50, release build, two runs) | 4.2–7.3 ms | 11.2–12.1 ms |
| CPU pack alone (release, ~2,100 paint ops) | n/a | 1.3 ms p50, 3.4 ms p90 |

The off-screen numbers include reading the frame back to the CPU, which doubles with the frame
and isn't paid on screen; they bound the GPU cost from above. What packing costs:
- **Fill and vertex work: 2x** (every primitive is drawn twice). GPUI's frames are small:
  thousands of instances, not millions of fragments of overdraw.
- **Buffers: 2x.** The swapchain is 3840x1080 instead of 1920x1080: 3 images × 16.6 MB vs
  8.3 MB, +25 MB, and GPUI's depth buffer +8 MB. The compositor side is already paid by the
  stereo output itself (HypXRland §8.9: +~100 MB for a 2560x1440 XR monitor).
- **CPU: one replay of the scene per present**, proportional to the paint operations.
- **Nothing when off.** One `Option` check in `present`, a `Vec<f32>` push per paint op (the
  depth tags), and an `Option<f32>` on `Style`.

## 9. Evidence (2026-11-19)

`QUIVER_DEMO=1 QUIVER_STEREO_SHOT=DIR quiver-app` renders the demo library (fixture data, art
from the cache) through GPUI's off-screen renderer and writes `mono.png`, `sbs.png` (3840x1080),
`sbs-focus-N.png` (after controller Down presses), `mono-focus.png` and `sbs-fractional.png`
(each eye moved by half the *unrounded* disparity), plus timings. Per-eye offsets were measured
by matching 60x60 blocks of each eye against the mono frame (best integer `dx`, residual):

| Region | Depth | Left eye | Right eye | Residual |
|---|---|---|---|---|
| top bar | 0.15 | +1 | 0 | 0 (bit-exact) |
| hero art and text | 0.0 | 0 | 0 | 0 |
| cover tiles (art and captions) | 0.3 | +1 | −1 | 0 to ±1 |
| focused tile (art and caption) | 0.6 | +2 | −2 | 0 to ±1 |

Text: with whole-pixel shifts the glyphs in each eye are identical to mono (edge energy 1.000x,
same peak). With the fractional shifts (±0.46, ±0.93, ±1.90 px) they're resampled by the
atlas's linear sampler: edge energy drops to 0.92–0.95x, peak brightness drops (234 vs 239), and
a best-integer match leaves residuals up to 30–40 levels. The left/right edges of each stem
smear by a pixel, visible at 8x zoom. A sub-pixel path that stayed sharp would have to rasterise
glyphs per eye at the shifted subpixel phase (GPUI has 4 x-variants), i.e. shift before
painting, not after. Whole pixels are the right call for UI.

*Not verified:* the live Wayland path. A scratch HypXRland couldn't start headless here (its
headless backend needs an allocator from DRM, which needs the seat the live session holds, or
from a parent Wayland session, which would open a nested window on the person's desktop), and
nothing was run against the live session. So the viewport scaling, the xdg tag reaching
`hyprctl clients` (`xdgTag`, `stereo`), the crop and the fused result in the glasses are
unchecked.

## 10. Input

Pointer, touch and the IME stay in **logical window coordinates**: the surface is still W x H,
the compositor maps the pane position back through the pack (§8.8/§8.9: "ray clicks land
exactly where you aim"), and GPUI's hit-testing uses the unshifted (mono) bounds. A raised
element's fused image is at its logical position (the two shifts are symmetric to within half a
pixel), so hit-testing matches what is seen. One known oddity: the compositor's cursor takes
the depth of the *window* it's over, not of our element, so over a raised tile it sits a few
pixels behind it ("subtitle behind the object"). Small rungs keep that mild; a fix needs the
compositor to ask the client for a depth at a point (no such protocol).

## 11. The full version: effort and recommendation

**Recommendation: build it**, as a setting ("3D interface on stereo displays", Settings →
Display; on by default once validated on the glasses), with the xdg tag and the one-line rule
above. The mechanism is small, contained in the vendored GPUI (about 400 lines), and free when
off. What remains:

| Work | Estimate |
|---|---|
| Live validation on the XReal and the Quest: the viewport path, the tag, fusion, rung tuning with the person | 0.5–1 day |
| GPUI hardening: edge clamp; atomic tag + first 2W buffer; resize while packed; fractional scales; a pixel test through the off-screen renderer; damage is full-frame already | 1 day |
| Depth across the app: game page (hero, tabs, cards), Downloads, Settings, the palette, dialogs as cards over a page-depth scrim, tooltips, drag lift | 1–1.5 days |
| Detection: `transform`, multiple windows, the XR quad-pair tier (only if wanted); a setting with strength | 0.5 day |
| One vocabulary with Sight: the rungs and `eye_shifts` in `quiver_perf::stereo` | 0.25 day |

About **3.5–4.5 days** of agent time, most of it in applying depth well across screens and
in a live tuning session, not in GPUI.
