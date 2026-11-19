# Stereo UI: the app in each eye, with depth (design notes, draft)

Status: spiked 2026-11-19 (a prototype behind `QUIVER_STEREO_UI`, off by default). The packed
frame is verified off-screen ([§9](#9-evidence-2026-11-19)); the live window path (Wayland
viewport, xdg tag, Hyprland events) compiles and is unit-tested but has **not** been seen on a
stereo output yet. Companion to [SIGHT.md §15](SIGHT.md#15-stereo-the-hud-and-the-panel-in-each-eye-2026-11-19)
(Sight in each eye), which shares this document's layout names and depth vocabulary. Sight's
depth is built and verified on the GPU ([§12](#12-sight-depth-in-the-game-built-2026-11-19)).

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

**Sight's rungs** (`quiver_perf::stereo::rung`, thousandths in the descriptors; P = the eye's
logical width, 1920 here; strength 100%). Sight draws inside the game's own packed frame, where
the game's plane is 0 (a fullscreen game is pinned to 0 by the compositor too, so the game, its
own HUD at zero disparity, and the screen plane coincide):

| Rung | Depth | Total px | Left / right eye | What |
|---|---|---|---|---|
| (the game) | 0 | 0 | 0 / 0 | the game, its HUD; the panel's scrim and the backdrop blur |
| `HUD` | 0.3 | 2 | +1 / −1 | the perf HUD (and its graph) |
| `PANEL` | 0.45 | 3 | +2 / −1 | the panel's card |
| `FOCUS` | 0.75 | 5 | +3 / −2 | the focused row or section |
| `DIALOG` | 0.75 | 5 | +3 / −2 | a dialog's box (over the card's scrim, which is at card depth) |
| `DIALOG_FOCUS` | 1.0 | 7 | +4 / −3 | the dialog's focused button |

Every step between two things seen together is 2 px total (≈0.05° on the XReal); the nearest
part is 7 px (≈0.17°). The focused row and a dialog never coexist (a dialog takes the focus), so
they share a rung. `quiver_perf::stereo::disparity_px` is the shared formula (whole pixels,
rounded); the app's prototype has a local copy (`stereo_ui::disparity_px`) to fold into it when
the app's version is built.

**Toasts** are layer-shell surfaces on the desktop, not part of the game's frame: on a stereo
output HypXRland lifts them with every `top`/`overlay` layer (`depth_layers` 0.8, ≈5 px). Nothing
for Sight to do; a `layerrule` can pin them to Sight's HUD rung if 0.8 feels too near:
`hl.layer_rule({ match = { namespace = "^quiver-sight-toast$" }, depth = 0.3 })` (optional, not
proposed).

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

## 12. Sight: depth in the game (built, 2026-11-19)

Sight already drew its HUD and panel once per eye on a packed frame at zero disparity
([SIGHT.md §15](SIGHT.md#15-stereo-the-hud-and-the-panel-in-each-eye-2026-11-19)). Each part now
gets a horizontal shift per eye from its rung.

**Wire** (zero from writers before: everything at the game's plane).
- `qv_hud_buffer.depth` (offset 200, u16 thousandths): the whole image's depth. quiver-sight
  writes `HUD` for the perf HUD and 0 for the panel.
- `qv_hud_buffer.nlifts` / `lifts[4]` (`qv_hud_lift`: x, y, w, h in image px, depth): parts of the
  image at their own depth, back to front. The panel sends its card, then the focused row or
  section, or a dialog and its focused button. quiver-perf validates them (inside the image,
  ≤ 1000).
- `qv_layer_status.depth_setting` / `depth_drawn` (appended; the struct is 416 bytes): the
  strength and the largest total disparity the layer last drew. `quiver hud status`,
  `quiver sight status` and `quiver lab sight N status` print `depth 100% (…; last drawn 5 px)`.

**Where the parts come from.** The panel view (`panel/view.rs`) puts an invisible marker canvas
in the card, the focused row or section, the dialog and its focused button. Each records its
element's bounds at prepaint, padded 6 px (the focus ring sits 5 px outside the element) and
clipped to the content mask (a row scrolled under the header isn't lifted over it). The in-game
publisher turns them into lifts in image pixels. A frame whose pixels didn't change but whose
lifts did is published whole.

**Drawing** (`compositor::Eyes::of`, `shaders/quad.frag`).
- The whole image is moved by its eye's share of its own depth. The shift goes in the push
  constants (`shift`), and the quad, its graph and the scissor all move with it.
- A lift is drawn in the same single quad pass. A fragment shows the front-most lift whose moved
  rectangle covers it, else the image as it lies.
- When a lift moves, it leaves a strip of its own pixels behind. Those fragments are filled from
  the column just beside the lift, on the side it left. The 6 px pad makes that column
  background, so there's no second copy of the lift's edge.
- Every fragment is written once (no double blending of translucent parts), and shifts are whole
  logical pixels, so text stays crisp. For `hsbs`, a logical pixel is half a physical one: the
  2:1 box filter just pairs different texels, so it's as sharp as `hsbs` gets.
- The push constants are 128 bytes, Vulkan's guaranteed minimum. The shader indexes them only by
  its (uniform) loop counter. Indexing them by a fragment's own layer, which differs across a
  wave, drew garbage on RADV, caught by the GPU test.

**Frame edges.** A shift is clamped to the greater of the part's margin from the eye's edge and
2 px, HypXRland's rule. Parts are also kept inside their image.

**The cursor** is drawn at the depth of the front-most lift under its tip, else the card's (never
behind the panel). In each eye it lands on what it points at, so a click hits what it looks to be
over. Pointer mapping is unchanged: the X11 position maps into the eye it's over
(`Stereo::to_logical`) and the panel gets that logical point.

**The blur** stays at the game's plane and inside each eye, as before. The scrim over it is part
of the image at depth 0.

**Knobs** (`sight.depth`, Display page, Advanced, applies at launch):

| Value | Strength | Env |
|---|---|---|
| Normal (default) | 100% | unset |
| Off | 0, the screen plane, as before depth | `QUIVER_SIGHT_DEPTH=off` |
| Subtle | 50% | `QUIVER_SIGHT_DEPTH=50` |
| Strong | 150% | `QUIVER_SIGHT_DEPTH=150` |
| Strongest | 200%, ≈14 px at the nearest | `QUIVER_SIGHT_DEPTH=200` |

The layer reads it like `QUIVER_SIGHT_STEREO`. Any whole percent from 0 to 200 works there. In
a lab, set it with `quiver lab edit N env.QUIVER_SIGHT_DEPTH=150`.

**Apparent depth on the XReal.** The XReal's numbers: 1920 px across 46° per eye, ≈41.7 px/°
(the factory calibration says 42.2°, ≈45.5 px/°). The angles are robust. The distance needs the
virtual screen's distance D, which HypXRland's docs don't give for the glasses, so two Ds are
shown. 1.5 m is HypXRland's model; ≈4 m is the vendor's figure for the virtual image.

| Part | Total px | Angle | In front at D = 1.5 m | In front at D = 4 m |
|---|---|---|---|---|
| HUD | 2 | 2.9′ (0.048°) | 3 cm | 21 cm |
| panel card | 3 | 4.3′ (0.072°) | 5 cm | 31 cm |
| focused row / dialog | 5 | 7.2′ (0.12°) | 8 cm | 49 cm |
| dialog's focused button | 7 | 10.1′ (0.17°) | 10 cm | 66 cm |
| (200%, the nearest) | 14 | 20′ (0.34°) | 19 cm | 113 cm |

All of these are far inside the 1° ceiling (≈42 px on the XReal). The steps are about half the
foveal ≈0.1° limit.

**The desktop panel** (the layer-shell fallback) can't draw itself packed. `xdg-toplevel-tag-v1`
tags toplevels only, and HypXRland has no `layerrule = stereo`. A layer surface can't declare
stereo content, so the compositor would show a packed panel whole in both eyes. On a stereo
output today it's an `overlay` layer at `depth_layers` 0.8. It spans the monitor (the scrim), so
the frame-violation clamp holds it to the 2 px slack: a slight lift as a whole, with no
per-element depth. Getting per-element depth there needs one of these:
- a HypXRland `layerrule = stereo` with the same contract as §8.6 (small, but in the compositor);
- the panel as a fullscreen xdg toplevel. It would lose the layer-shell semantics the panel relies
  on: above everything, keyboard exclusivity, never in the window list.

Neither is cheap, so it isn't built. The in-game panel is the usual path whenever the layer and
the preload are in the game.

**Verification.**
- Unit tests:
  - the ladder against HypXRland's table, the eye split, the frame clamp and the setting
    (`stereo::depth_tests`);
  - descriptor validation and round trip (`hud::tests::depth_and_lifts`);
  - the status block's zero-extension (`layer_status_bytes`);
  - the layer's per-eye shifts, lifts, cursor depth, hsbs and the edge clamp
    (`compositor::eye_tests::depth_per_eye`);
  - the Tweak's plan.
- GPU, opt-in: `tests/vulkan_depth.rs` (headless RADV, 3840x400 sbs) reads back a panel drawn
  through the real layer:
  - the whole image at the HUD rung: the card's edge at 101 / 99 (2 px);
  - the card and focused row as lifts: card 102 / 99 (3 px), row 403 / 398 (5 px), its contents
    with it (423 / 418);
  - the strip the row moved off shows the card, not the row's edge again;
  - the cursor over the row is 5 px apart;
  - the status block reports 100% and 5 px.
- quiver-sight (`tests/ingame_panel.rs`): a panel reopened for an sbs frame publishes depth 0 with
  the card as its only lift. After Left, the card and the focused section follow, at their rungs,
  the section inside the card.

**Not verified.**
- A real game in the lab. The root disk was full for most of this work. Once there was room, a
  fresh AWR instance (Torchbearer 0.4.9, `output = hsbs`; its SDK call reached the lab's
  quiver-sight as `hsbs`) stopped at AWR's "DX12 feature level 12_0" dialog on every launch. It
  did the same with the installed Sight layer, so the cause is the host, not this change.
- How it looks in the glasses: whether the ladder reads as intended and the steps are
  comfortable over an evening. The numbers are HypXRland's provisional ones.
- Depth changes snap. HypXRland eases its own over about 80–200 ms; Sight's focus moves are 2 px
  steps, so they snap.
