# Stereo UI: the app in each eye, with depth (design notes)

Status: **built 2026-11-19**, on by default as Settings → Display → "3D on stereo displays"
(Auto). Every screen has its depth ([§4](#4-depth-vocabulary-one-ladder-for-the-app-and-sight),
[§13](#13-rules-for-new-components)); packed frames are verified off-screen per screen
([§9](#9-evidence-2026-11-19)) and through real elements by a test. The live window path (Wayland
viewport, xdg tag, Hyprland events) compiles and is unit-tested but has **not** been seen on a
stereo output yet: [§14](#14-tuning-in-the-glasses-a-checklist) is the checklist for that.
Companion to [SIGHT.md §15](SIGHT.md#15-stereo-the-hud-and-the-panel-in-each-eye-2026-11-19)
(Sight in each eye), which shares this document's layout names and depth vocabulary. Sight's
depth is built and verified on the GPU ([§12](#12-sight-depth-in-the-game-built-2026-11-19)).

**Why.** The person wears an XReal Air 2 Ultra (a flat side-by-side output) and a Quest 3
(OpenXR) under HypXRland, their Hyprland fork. HypXRland already lifts whole windows (its depth
desktop), so the focused Quiver window floats above the wallpaper. Inside the window everything
was one flat card. The app now draws both eyes itself, tags its toplevel `stereo:sbs`, and the
compositor un-packs it per eye: art at the back, content surfaces a step up, controls a step
more, focus and hover more again, overlays above everything. The prototype's home page "looks
very nice" in the glasses (the person, 2026-11-19); the same care now goes everywhere.

## 1. How it works

```
GPUI paint ──► Scene: each paint op in a depth scope ──► pack_side_by_side ──► 2W x H frame
   (element styles resolved against                     │ left eye:  x + left(depth), clamped
    the depth they're drawn on)                         │ right eye: W + x + right(depth), clamped
Wayland: buffer 2W x H ──wp_viewport──► surface W x H ──xdg tag "stereo:sbs"──► HypXRland
                                                         halves the surface: left half → left eye
```

- **Depth at paint time, relative.** An element's style asks for depth in four ways
  (`gpui::Styled`): `stereo_depth(d)` (absolute: overlays), `stereo_lift(d)` (a rise above what
  it's drawn on: surfaces), `stereo_focused(on)` (it takes the app's focus: a control, more when
  focused) and `stereo_control(bool)` (override the guess). The window's `StereoPolicy` turns
  that, the element's cursor (a pointer or text cursor makes it a control), its hover and GPUI's
  focus into a depth with `gpui::resolve_stereo_depth`, relative to the depth of its parent, so
  a button on a raised card pops from the card. Resolution happens in `Interactivity::paint`
  (`div.rs`), only while the window packs; mono frames skip it.
- **Scopes.** Each resolved depth opens a scope (`Scene::push_stereo_scope`: depth, the
  element's bounds); every primitive records its scope, kept in step through the scene's sort.
  Cached views keep their scopes when replayed. Deferred elements (menus) get the depth of the
  place they were deferred from, also when nested in another deferred element. Tooltips and
  dragged elements are drawn at the policy's tooltip depth.
- **Hover redraws.** A control's hover changes its depth, so a control without a hover style of
  its own (GPUI only tracks hover for those) gets a mouse listener that redraws when its hover
  changes. GPUI's hover is input-modality aware: after a key or pad press nothing is hovered.
- **Packing at present time.** `Window::set_stereo_packing(Some(StereoPacking))` makes
  `Window::present` call `Scene::pack_side_by_side(eye, shift, edge_slack)`. It works on the
  *finished, sorted* primitives: each primitive is copied twice, moved by its eye's origin plus
  its scope's shift, its content mask moved with it and **clipped to its eye**; both copies keep
  the primitive's draw order (they never overlap), so nothing is re-sorted. While packing, draw
  orders are worked out as if every primitive were as wide as the largest shift on both sides
  (`Scene::set_stereo_order_slack`), so neighbours that only overlap once moved still draw in
  paint order. A present without a new draw reuses the packed frame (`StereoFrame`, keyed by
  the scene's generation and the eye's size); its animated sprites keep animating.
- **The frame's edges.** Each scope's shift is limited to the greater of its margin from the eye's
  edge it moves toward and 2 px (`gpui::clamp_eye_shift`, HypXRland's `depth_edge_slack`
  rule): nothing slides off one eye only. A primitive spanning the whole eye (a scrim, a bar's
  background) stays put and doesn't hold its scope back.
- **The drawable.** `PlatformWindow::set_stereo_side_by_side(true)` doubles the drawable's
  width while the window keeps its size: on Wayland `wp_viewport`'s destination stays at the
  window's logical size, so the surface is W x H and the buffer 2W (each eye `round(W·scale)`
  device pixels, as `present` rounds it). Resizes, maximise, fullscreen and scale changes go
  through the same `drawable_size`. Only toplevels pack: a popup or layer surface can't say it's
  packed, so it stays mono. The off-screen window renders 2W x H. X11 says no and stays mono.
- **The tag** goes out right before the first buffer it describes (`draw`): the window keeps the
  tag the compositor last got and sends `stereo:sbs` / `stereo:mono` whenever packing differs
  from it. Requests on the connection are ordered, so the compositor reads the tag before that
  buffer's commit. Turning packing on and off again between two frames sends nothing.

**Where it hooks** (all marked `Quiver:` in the vendored GPUI):

| Piece | File | Function |
|---|---|---|
| depth rules | `third_party/zed/gpui/src/stereo.rs` | `StereoPolicy`, `StereoRequest`, `StereoState`, `resolve_stereo_depth`, `clamp_eye_shift`, `StereoStats` |
| per element | `gpui/src/elements/div.rs` | `Interactivity::stereo_depth`, its use in `Interactivity::paint` |
| style | `gpui/src/style.rs`, `styled.rs` | `Style::stereo_{depth,lift,focused,control}`, `Styled::stereo_*` |
| scopes, packing | `gpui/src/scene.rs` | `push_stereo_scope`, `replay`, `pack_side_by_side`, `PrimitiveScopes`, `sort_in_place` |
| path tiles | `gpui/src/path_winding.rs` | `PathTiles::translated` |
| window | `gpui/src/window.rs` | `set_stereo_policy`, `set_stereo_packing`, `present`, `StereoFrame`, `with_stereo_scope`, deferred draws, tooltips, drags |
| deferred | `gpui/src/elements/deferred.rs` | depth captured at paint |
| platform | `gpui/src/platform.rs` | `PlatformWindow::set_stereo_side_by_side` (default: unsupported) |
| Wayland | `gpui_linux/src/linux/wayland/window.rs`, `client.rs` | `drawable_size`, `set_stereo_side_by_side`, `stereo_tag_sent`; the `xdg_toplevel_tag_manager_v1` global |
| off-screen | `gpui_linux/src/linux/offscreen.rs` | `draw` at 2W |
| the ladder | `crates/quiver-perf/src/stereo.rs` | `ui::*`, `disparity_px`, `eye_shift` (shared with Sight) |
| roles | `crates/quiver-ui/src/depth.rs` | `policy`, `packing`, `eye_shifts`, `DepthExt` |
| components | `crates/quiver-ui/src/components.rs` | `card` (a surface), `FocusExt::focus_ring` / `focus_glow` (focus) |
| the app | `crates/quiver-app/src/stereo_ui.rs` | the setting, detection, tags, `shot` |

**Alternatives looked at.**
- *Buffer scale.* `wl_surface.set_buffer_scale` scales both axes, so a 2W-wide buffer at scale 2
  is also 2H tall. Only a viewport gives a 2:1 buffer in a 1:1 surface. HypXRland crops in
  surface UV after the viewport's source rectangle (`calculateUVForSurface`, research/24 F7),
  so a 2W buffer behind a W surface gives each eye exactly W buffer columns: 1:1 at scale 1.
- *Two renders with two viewports.* GPUI's fragment shaders (rounded quads, path coverage) read
  the framebuffer position, so the right eye would need a second, offset copy of every bound
  anyway. Packing the scene keeps the renderer untouched.
- *A per-instance depth on the GPU.* A depth field in every instance struct and a per-eye
  uniform would avoid the CPU copy, but changes every primitive's layout and every shader. The
  copy costs well under a millisecond ([§8](#8-cost)).
- *Replaying the paint operations per eye* (the prototype): it re-ran GPUI's bounds tree and
  sort for every packed frame, ≈1.3 ms. Copying the sorted primitives is ≈15x cheaper.
- *Painting the element tree twice.* GPUI's paint pass has side effects (hitboxes, listeners,
  element state); doing it per eye is a much larger change than copying the result.

## 2. Detection: when to pack

Packing is right only on an output that presents two panes. On anything else a 2W frame would
show squeezed and doubled, so the app is mono unless it knows better.

- **The setting.** Settings → Display → "3D on stereo displays": **Auto** (the default) or
  **Off**, and the depth strength (50, 100, 150, 200%). Kept in `~/.config/quiver/app.json`
  (`stereo_ui`, `stereo_depth`; `QUIVER_APP_CONFIG` overrides the path; other keys are kept).
  The page says what the window is on and whether it's in 3D now. Demo mode doesn't write it.
- **Hyprland / HypXRland.** `hyprctl -j monitors` has `"stereo": "sbs"` (plus `scanoutWidth`,
  `stereoComposites`, …) on a packed output and no such field elsewhere (`HyprCtl.cpp`). The
  window is the client with our pid in `hyprctl -j clients`; with several (more windows), the one
  whose logical size is nearest the window's, then the most recently focused. Its `monitor` picks
  the monitor; a rotated or flipped one (`transform` ≠ 0) stays mono, as HypXRland's depth does.
  Both a flat stereo output (`monitor = …, stereo:sbs`, the XReal) and an XR monitor under the
  depth desktop (§8.9) report this, so one check covers both headsets. The queries go straight
  to Hyprland's request socket (`j/monitors`, `j/clients`; `hyprctl` if it doesn't answer).
- **When it looks.** At start; on the event socket's `monitoradded`/`monitorremoved`/
  `movewindow`/`moveworkspace`/`workspace`/`focusedmon`/`openwindow`/`configreloaded`/
  `fullscreen`/`changefloatingmode` (v1 and v2); when GPUI says the window entered another output
  or changed size; when the setting changes; and every 5 s regardless (a stereo pack can come and
  go on the XReal's mode watch without an event). Bursts are coalesced over 120 ms. Hyprland not
  answering keeps the current state rather than flapping.
- **Moving between monitors.** Onto a mono monitor: packing off, the buffer back to W, the xdg tag
  `stereo:mono` with that first W buffer, the Hyprland window tag removed. Back onto the stereo
  one: the reverse. A change of the pane's width (another stereo monitor, another scale) or of
  the strength repacks with new shifts.
- **`wl_output` (not usable).** On a stereo output `wl_output.mode` and `xdg_output` report the
  *pane* (1920x1080) on purpose (§8.5). `wl_surface.enter` says which output, not that it's
  packed; the app uses it only as a cue to ask Hyprland again.
- **Other compositors: mono.** Auto needs Hyprland's answer. `QUIVER_STEREO_UI` overrides the
  setting for testing: `sbs`/`auto` as Auto, `sbs-always`/`always` packs regardless (any
  compositor, a 1920-px pane at the window's scale when the monitor isn't known), `off`/`mono`
  never. `QUIVER_STEREO_UI_STRENGTH` (a percent, 0..200) overrides the strength.
- *Not handled:* the XR quad-pair tier with the depth desktop off ([§6](#6-the-quest-path)).

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

## 4. Depth vocabulary: one ladder for the app and Sight

**Depth** is HypXRland's unitless rung: 0..1 above the plane it is drawn on (for the app, the
window's own plane; the compositor lifts the window itself; for Sight, the game's plane).
Nothing goes behind the plane: crossed (in front) disparity is the comfortable direction
(research/24 §8.2, Shibata 2011).

**Per-eye pixels** use HypXRland's formula and defaults (05 §8.7, research/24 §8.1), in one
place, `quiver_perf::stereo::disparity_px` (the app's prototype copy is gone; `quiver_ui::depth`
calls it): rise `h = depth · 0.12 m · strength`; total disparity `t = b·h / (D − h) · P/W` px
with `D = 1.5 m`, `W = 1.6 m`, `b = 0.063 m` and `P` the eye's width in device pixels; capped at
1°. `t` is rounded to whole pixels and split by `eye_shift`: the **left eye's content moves right
by ⌈t/2⌉, the right eye's left by ⌊t/2⌋**. Whole device pixels keep text and art identical to
mono in each eye ([§9](#9-evidence-2026-11-19)).

**The ladder** (`quiver_perf::stereo::ui` for the app, `quiver_perf::stereo::rung` for Sight;
thousandths; pixels at 100% on a 1920-px eye). The app has *absolute* rungs, which something
sits on whatever it's drawn over, and *lifts*, added to the depth of what something is drawn
on. Every app lift is one step of 150 (≈1 px), focus two; resolved depths are clamped to `MAX`.

| | Name | Depth | Total px | Left / right | What |
|---|---|---|---|---|---|
| app, absolute | `ui::PLANE` | 0 | 0 | 0 / 0 | the page, art (heroes, the sign-in mosaic, a download's backdrop), scrims |
| | `ui::CHROME` | 0.15 | 1 | +1 / 0 | the top bar, the controller hint bar |
| | `ui::OVERLAY` | 0.6 | 4 | +2 / −2 | menus, popovers, dropdowns, dialogs, the palette, toasts, floating notices, now playing, the game-list drawer, welcome over the app |
| | `ui::TOOLTIP` | 0.75 | 5 | +3 / −2 | tooltips, a dragged element |
| | `ui::MAX` | 1.0 | 7 | +4 / −3 | the ceiling |
| app, lift | `ui::SURFACE` | +0.15 | +1 | | a content surface: card, panel, side rail, list, the grid's rows, text and buttons over a hero's art |
| | `ui::CONTROL` | +0.15 | +1 | | a control: anything with a pointer or text cursor, or the app's focus |
| | `ui::HOVER` | +0.15 | +1 | | a hovered control (never more than focus; not added to it) |
| | `ui::FOCUS` | +0.3 | +2 | | the focused control (keyboard, controller, a text field being typed in) |
| Sight | `rung::HUD` | 0.3 | 2 | +1 / −1 | the perf HUD (and its graph) |
| | `rung::PANEL` | 0.45 | 3 | +2 / −1 | the panel's card |
| | `rung::FOCUS` | 0.75 | 5 | +3 / −2 | the focused row or section |
| | `rung::DIALOG` | 0.75 | 5 | +3 / −2 | a dialog's box |
| | `rung::DIALOG_FOCUS` | 1.0 | 7 | +4 / −3 | the dialog's focused button |

**What that resolves to** (total px; rest / hovered / focused):

| Element | px |
|---|---|
| page, hero art | 0 |
| top bar | 1; its tabs and buttons 2 / 3 / 4 |
| a button or link on the page (the game page's tabs) | 1 / 2 / 3 |
| card, panel, side rail, a hero's text | 1 |
| a button, row, chip, toggle or input on a card (a hero's Play) | 2 / 3 / 4 |
| a library tile (on the grid's row) | 2 / 3 / 4 |
| a button inside a clickable row on a card | 3 / 4 / 5 |
| the sign-in form and the full-screen welcome (two steps over their art) | 2; inputs and buttons 3 / 4 / 5 |
| overlay | 4; its entries and buttons 5 / 6 / 7 |
| tooltip, drag | 5 |

The step between any two things seen together is at most 3 px at 100% (a dialog's focused
button over its dialog); between an overlay and the page under it, 2 px over the nearest tile.
The span is 7 px (≈0.17° on the XReal, about a sixth of HypXRland's 1° ceiling).

**Strength** is `sight.depth`'s vocabulary: Settings → Display → Depth 50, 100 (default), 150,
200%; the nearest thing is then 3, 7, 10 or 14 px. Steps scale with it. (Sight's own knob also
has Off; the app's off is the 3D setting.)

**The window's own lift composes.** HypXRland lifts the focused window (0.6 by default), so the
app's rungs sit on top of that: a dialog is ≈0.6 + 0.6 of the scale above the wallpaper, still
single-digit pixels.

## 5. Comfort

From HypXRland (05 §8.7, research/24 §7.3, §8.2):
- **Steps, not totals, bind.** Two things in the foveal field at different depths may differ by
  about 0.1° ≈ **3 px total**; the whole span should stay single-digit pixels; the hard ceiling
  is **1°**. The ladder keeps steps to 1–2 px and the span to 7 px at 100%.
- **A small ladder, not a continuum.** Three absolute rungs and four lifts, used the same way on
  every screen ([§13](#13-rules-for-new-components)).
- **Hover.** HypXRland deliberately doesn't change depth on hover (pointer-driven vergence
  micro-adjustments). The person asked for hover to pop, so it does, as one discrete 1-px step
  per element the pointer enters, never more than focus and never added to it, and only while
  the last input was the pointer (GPUI's hover is modality-aware). Depth changes snap; nothing
  animates. If it strains in the glasses, `HOVER = 0` in `quiver_perf::stereo::ui` turns it off
  ([§14](#14-tuning-in-the-glasses-a-checklist)).
- **Frame edges.** Every scope's shift is clamped at the eye's edges (HypXRland's rule, 2 px
  slack), and full-width fills stay put, so no sliver shows in one eye only.
- **Integer pixels.** Whole-pixel shifts keep glyphs and art bit-identical to mono in each eye;
  fractional shifts resample them ([§9](#9-evidence-2026-11-19)). The rounding moves rungs by
  ≤ 0.2 px.
- **Occlusion agrees with depth.** Overlays sit above anything they can cover; a scrim is a flat,
  textureless dim at the plane, so it gives no depth cue of its own. Known exception: a tooltip
  (5 px) shown over a dialog's hovered button (6 px) is drawn over something nearer. Rare
  (dialogs have few tooltips); not handled.

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

HypXRland never engages stereo for a window on its own; a rule must opt in. The app sends
**both** tags while it packs, so it works with the person's config as it is today and with the
rule below. They agree (both say `sbs`) and are withdrawn together.

- **The xdg tag** (`xdg-toplevel-tag-v1`, compositor-neutral): `stereo:sbs` while packed,
  `stereo:mono` after, exactly HypXRland's strings (05 §8.6, "The tag"). It goes out with the
  first buffer of the new width, so no frame is shown split wrongly. **The rule that makes the xdg
  tag alone sufficient** (not in the person's config today; documented here only, nothing installs
  it):

  ```lua
  -- ~/.config/hypr/hyprland-xr.lua (Lua front end)
  hl.window_rule({ match = { xdg_tag = "^stereo:" }, stereo = "auto" })
  ```
  ```ini
  # classic syntax
  windowrule = stereo auto, match:xdg_tag ^stereo:.*
  ```

  Under `auto` the tag *is* the declaration: it picks the layout, skips the fullscreen gate, and
  `stereo:mono` turns it off. Any client that tags itself the same way gets it too (Torchbearer's
  mods, mpv).
- **The Hyprland window tag** `stereo-sbs` (Hyprland only; works with today's config, which
  maps window tags `stereo-<layout>` to `stereo <layout> always`). The app finds its window
  ([§2](#2-detection-when-to-pack)) and sends
  `dispatch hl.dsp.window.tag({ tag = "+stereo-sbs", window = "address:0x…" })`, falling back to
  the classic `dispatch tagwindow +stereo-sbs address:0x…`; `-stereo-sbs` clears it, also from a
  window it moved away from. It lags the buffer by an IPC round trip (a frame or so of an unsplit
  2W buffer when packing starts) and is a compositor write from the app (to its own window only).
- `QUIVER_STEREO_UI_TAG=xdg` sends only the xdg tag. Once the rule above is in the config, the
  window tag can go (make `xdg` the default).

## 8. Cost

Measured off-screen (`QUIVER_STEREO_SHOT_TIME=1`), release build, 1920x1080 at scale 1, this
machine's discrete GPU (RTX 5070 Mobile; GPUI's wgpu on Vulkan), 30 frames each:

| Screen | | render + readback p50 | `render_now` p50 (layout, paint, pack, render, readback) | pack (CPU) |
|---|---|---|---|---|
| library | mono | 3.7 ms | 9.0 ms | |
| | sbs | 6.1 ms | 10.9 ms | 58 µs: 3,708 primitives (both eyes), 60 depth scopes |
| game page, Options tab (the busiest) | mono | 5.3 ms | 11.1 ms | |
| | sbs | 5.8 ms | 12.7 ms | 97 µs: 6,114 primitives, 122 scopes |

The prototype replayed every paint operation per eye (re-running GPUI's bounds tree and sort):
1.3 ms p50, 3.4 ms p90 for ~2,100 paint ops. Copying the sorted primitives instead is ≈15–20x
cheaper, so packing is now a rounding error next to layout and paint. The off-screen numbers
include reading the frame back, which doubles with the frame and isn't paid on screen.

- **Unchanged frames aren't repacked.** A present without a new draw (the compositor asking
  again, an animation tick) reuses the packed frame; `Window::stereo_stats` counts packs and
  reuses. A redraw that happens to produce the same scene is repacked: at <0.1 ms, comparing
  scenes would cost about what it saves.
- **GPU: 2x fill and vertices** (every primitive is drawn twice); GPUI's frames are thousands of
  instances, not overdraw-bound.
- **Buffers: 2x.** 3840x1080 instead of 1920x1080: +25 MB of swapchain and +8 MB of depth
  buffer. The compositor side is already paid by the stereo output itself.
- **Paint: per element, a depth resolution** (a few comparisons) and one scope push for those
  with a depth; a mouse listener for each control without a hover style.
- **Nothing when off.** One `Option` check in `present`, a `u32` per primitive (its scope), and
  the four optional style fields. Depth isn't resolved on mono frames.

## 9. Evidence (2026-11-19)

`tools/stereo-shots.sh OUT target/release/quiver-app` renders 47 demo scenes (every screen in
the brief, focus, hover, menus, a dialog, the palette, a toast, a tooltip, 200% strength, scale
1.5, narrow windows) through GPUI's off-screen renderer, mono and packed (3840x1080), and
`tools/stereo-evidence.py` measures every element with a depth: it finds, by matching the
element's own textured pixels in each eye against mono, how far each eye actually moved it.
Per scene it writes `NAME-mono.png`, `NAME-sbs.png`, `NAME-anaglyph.png` (red = left eye),
`NAME-disparity.png` (each measured element tinted by its total disparity) and
`NAME-evidence.json`. No compositor, daemon or settings are touched (demo mode, no Wayland, X
or Hyprland variables).

- **Every measured element moved exactly as packed** in both eyes. "Measured" needs texture
  (flat fills match any shift); the misses are container scopes whose only texture belongs to
  their children, scrolled or covered elements, and text whose background moved with it (a
  hero's caption over art): matched at the right shift but with < 90% identical pixels.
- **Pops are relative.** Controls measure +1 px over the surface they're on, wherever that is
  (page, card, row, overlay). Hover adds one more and focus two: a library tile on its row is
  2 px, hovered 3, focused 4 (`library`, `library-hover`, `library-focus`); a palette entry is
  5 over the palette's 4, the selected one 7; the focused segment in Settings → Display and the
  focused button in Downloads 4 over their card's 1.
- **Text stays crisp.** With whole-pixel shifts each eye's glyphs are identical to mono: the
  strongest 1% of horizontal gradients is unchanged (1.000–1.002x in every scene), and total edge
  energy is within 0.5% (differences are where a raised element's edge now meets a different
  background, not blur). The prototype measured the alternative: fractional shifts resample
  glyphs through the atlas's linear sampler (edge energy 0.92–0.95x, stems smear by a pixel).

| Screen | Elements with depth | Measured / as packed | Rungs seen (total px × count) | Pops over parent (px) | Edges L/R vs mono |
|---|---|---|---|---|---|
| activity | 40 | 32 / 32 | 1×9, 2×23 | +1 | 1.000 / 1.000 |
| confirm | 40 | 36 / 36 | 1×8, 2×25, 4×1, 5×2 | +1 | 1.001 / 1.000 |
| downloads | 61 | 56 / 56 | 1×16, 2×40 | +1 | 1.000 / 1.000 |
| downloads-focus | 62 | 55 / 55 | 1×16, 2×38, 4×1 | +1, +3 | 1.000 / 1.000 |
| game-actions | 97 | 73 / 70 | 1×17, 2×52, 3×1 | +1, +2 | 0.998 / 1.000 |
| game-addons | 81 | 65 / 61 | 1×14, 2×47 | +1 | 0.996 / 1.000 |
| game-files | 82 | 66 / 62 | 1×15, 2×47 | +1 | 0.997 / 1.000 |
| game-launch | 102 | 84 / 79 | 1×18, 2×61 | +1 | 0.997 / 1.000 |
| game-logs | 78 | 62 / 58 | 1×19, 2×39 | +1 | 0.997 / 1.000 |
| game-narrow | 94 | 48 / 46 | 1×15, 2×31 | +1 | 0.996 / 1.000 |
| game-options | 122 | 102 / 98 | 1×59, 2×39 | +1 | 0.998 / 1.000 |
| game-options-changes | 101 | 81 / 77 | 1×25, 2×52 | +1 | 0.997 / 1.000 |
| game-options-keys | 87 | 70 / 66 | 1×25, 2×41 | +1 | 0.997 / 1.000 |
| game-options-unsaved | 124 | 104 / 100 | 1×58, 2×39, 4×1, 5×2 | +1 | 0.998 / 1.000 |
| game-overview | 97 | 73 / 70 | 1×18, 2×52 | +1 | 0.998 / 1.000 |
| game-performance | 140 | 67 / 63 | 1×19, 2×44 | +1 | 0.997 / 1.000 |
| game-profiles | 108 | 85 / 81 | 1×31, 2×50 | +1 | 0.997 / 1.000 |
| game-profiles-dialog | 126 | 94 / 92 | 1×23, 2×51, 4×1, 5×17 | +1 | 0.998 / 1.000 |
| game-saves | 87 | 68 / 64 | 1×18, 2×46 | +1 | 0.997 / 1.000 |
| game-steam | 87 | 71 / 69 | 1×22, 2×47 | +1 | 0.995 / 1.000 |
| game-tweaks | 107 | 83 / 79 | 1×27, 2×52 | +1 | 0.997 / 1.000 |
| install-dialog | 63 | 57 / 57 | 0×1, 1×4, 2×49, 4×1, 5×2 | +1 | 1.000 / 1.000 |
| library | 60 | 54 / 54 | 0×1, 1×4, 2×49 | +1 | 1.000 / 1.000 |
| library-200 | 60 | 54 / 54 | 0×1, 2×4, 4×48, 6×1 | +2 | 1.000 / 1.000 |
| library-focus | 60 | 45 / 42 | 1×2, 2×39, 4×1 | +1 | 1.000 / 1.000 |
| library-hover | 60 | 54 / 54 | 0×1, 1×4, 2×48, 3×1 | +1 | 1.000 / 1.000 |
| library-menu | 67 | 51 / 49 | 1×2, 2×40, 4×1, 5×5, 7×1 | +1, +3 | 1.000 / 1.000 |
| library-narrow | 66 | 17 / 14 | 2×13, 4×1 |  | 1.000 / 1.000 |
| library-scale150 | 62 | 23 / 21 | 2×20, 4×1 |  | 1.000 / 1.000 |
| now-playing | 63 | 57 / 57 | 0×1, 1×4, 2×50, 4×1, 5×1 | +1, +4 | 1.000 / 1.000 |
| palette | 71 | 65 / 65 | 0×1, 1×4, 2×49, 4×1, 5×8, 7×2 | +1, +3 | 1.000 / 1.000 |
| settings-about | 29 | 25 / 25 | 1×5, 2×20 | +1 | 1.004 / 1.000 |
| settings-accounts | 37 | 33 / 33 | 1×8, 2×25 | +1 | 1.003 / 1.000 |
| settings-appearance | 30 | 25 / 25 | 1×5, 2×19, 5×1 | +1 | 1.003 / 1.000 |
| settings-automation | 27 | 23 / 23 | 1×5, 2×18 | +1 | 1.003 / 1.000 |
| settings-compat | 26 | 22 / 22 | 1×4, 2×18 | +1 | 1.004 / 1.000 |
| settings-display | 30 | 26 / 26 | 1×3, 2×22, 4×1 | +1, +3 | 1.007 / 1.000 |
| settings-library | 29 | 25 / 25 | 1×6, 2×19 | +1 | 1.003 / 1.000 |
| settings-performance | 65 | 52 / 52 | 1×3, 2×26, 3×23 | +1 | 1.002 / 1.000 |
| settings-steam | 45 | 33 / 33 | 1×6, 2×27 | +1 | 1.002 / 1.000 |
| settings-tweaks | 53 | 43 / 43 | 1×12, 2×31 | +1 | 1.002 / 1.000 |
| signin | 6 | 5 / 5 | 1×1, 2×1, 3×2, 5×1 | +1, +3 | 1.000 / 1.000 |
| signin-2fa | 31 | 26 / 25 | 0×1, 1×2, 2×21, 3×1 | +1 | 1.000 / 1.000 |
| toast | 61 | 55 / 55 | 0×1, 1×4, 2×49, 4×1 | +1 | 1.000 / 1.000 |
| tooltip | 61 | 55 / 55 | 0×1, 1×4, 2×48, 3×1, 5×1 | +1, +2 | 1.000 / 1.000 |
| welcome | 72 | 16 / 12 | 2×1, 3×11 | +1 | 1.000 / 1.000 |
| welcome-steam | 88 | 23 / 18 | 2×1, 3×17 | +1 | 1.000 / 1.000 |

(Edges: total horizontal edge energy of each eye over mono's.) `library-200` is the same page at
200%: every rung doubles (tiles 4 px, the attention panel's buttons 6). `library-scale150`
renders at scale 1.5 (1280x720 logical): shifts stay whole device pixels.

*Not verified:* the live Wayland path. A scratch HypXRland can't start headless here (its
headless backend needs DRM or a parent Wayland session, which would open a window on the
person's desktop), and nothing was run against the live session. So the viewport scaling, the
tags reaching `hyprctl clients`, the crop and fusion in the glasses are for
[§14](#14-tuning-in-the-glasses-a-checklist).

**Tests.**
- `crates/quiver-ui/tests/stereo_depth.rs` (off-screen window, real elements; passes without a
  GPU by saying so): a card is a surface; buttons pop from the page, the card and an overlay;
  focus two steps, hover one, hover on a focused control nothing more; hover redraws a control
  with no hover style; a clickable wrapper and its ringed pill are one control; a clickable area
  marked flat stays flat; a menu deferred from a card, and one deferred from that menu, keep
  their places' depths; the ceiling; the card's packed shifts are +1 / 0; mono frames carry no
  depths.
- `quiver_ui::depth` tests: the resolution with the app's policy (nesting, controls, focus,
  hover, the wrapper rule, key focus on containers, overlays, the ceiling, strengths).
- `quiver_perf::stereo` tests: the ladder against HypXRland's table, the app's ladder, the eye
  split.
- `stereo_ui` tests: packing a real `Scene` (per-eye moves, masks clipped to the eye, the edge
  clamp, full-width fills pinned, draw order), detection (stereo monitor, rotated, several windows
  of one process, what to pack per mode), the prefs file, Hyprland events and dispatches.
- GPUI's own unit tests in `stereo.rs` mirror these, but GPUI isn't a workspace member, so
  `cargo test --workspace` doesn't run them; the quiver-ui tests cover the same functions.

## 10. Input

Pointer, touch and the IME stay in **logical window coordinates**: the surface is still W x H,
the compositor maps the pane position back through the pack (§8.8/§8.9: "ray clicks land
exactly where you aim"), and GPUI's hit-testing uses the unshifted (mono) bounds. A raised
element's fused image is at its logical position (the two shifts are symmetric to within half a
pixel), so hit-testing matches what is seen. One known oddity: the compositor's cursor takes
the depth of the *window* it's over, not of our element, so over a raised tile it sits a few
pixels behind it ("subtitle behind the object"). Small rungs keep that mild; a fix needs the
compositor to ask the client for a depth at a point (no such protocol).

## 11. What's left

| Work | Notes |
|---|---|
| The tuning pass in the glasses | [§14](#14-tuning-in-the-glasses-a-checklist); the rungs are HypXRland's provisional numbers |
| The xdg rule in the person's config, then `QUIVER_STEREO_UI_TAG=xdg` as the default | [§7](#7-the-hyprland-rule-and-the-two-ways-to-tag); the person's call |
| Tooltips relative to what they point at | a tooltip over a dialog's hovered button is 1 px behind it ([§5](#5-comfort)) |
| The XR quad-pair tier (depth desktop off) | only if wanted ([§6](#6-the-quest-path)) |
| Easing depth changes (HypXRland eases its own over 80–200 ms) | they snap; 1–2 px steps |

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

## 13. Rules for new components

Depth comes from the shared components and from three roles; a new screen built from them is
right without thinking about stereo. In order of how often they matter:

1. **Controls pop by themselves.** Anything with `cursor_pointer()` (or a text cursor) is a
   control: it rises a step above whatever it's drawn on, one more hovered, two more focused.
   `components::button`, `icon_button`, chips, toggles, rows with a click handler: nothing to add.
2. **Focus goes through `focus_ring` / `focus_glow`** (`quiver_ui::components::FocusExt`): they
   mark the element as something the keyboard or controller can focus, and lift it while it is.
   Something that shows focus another way (a menu entry's highlight, a session row's tint) says
   so with `.stereo_focused(on)`. Mark **one** element per focus target: if a row's button holds
   the ring, don't also mark the row, or the two lifts stack.
3. **Surfaces**: anything with its own background that holds content (a card, a panel, a side
   rail, a list's box, glass over art, a hero's text block) is `components::card` or
   `.depth_surface()`. Don't nest surfaces for decoration: each adds a pixel.
4. **Overlays**: menus, popovers, dropdowns, dialogs, the palette, toasts, floating notices,
   drawers: `.depth_overlay()` on the *panel*, not on its scrim. Overlays are absolute (they sit
   at the overlay rung whatever they cover), so a dropdown opened from a raised card still sits
   above the card; their controls pop from them.
5. **Art and backgrounds** need nothing: they're on whatever they're drawn on, the plane for a
   page. `.depth_plane()` pins something back to the plane under a raised parent.
6. **Not a control:** a scrim or a whole-area click target with a pointer cursor:
   `.depth_flat()`. A clickable wrapper without focus of its own around the element with the
   ring is one control automatically (the inner one adds only its focus). A split button whose
   halves have their own rings: `.stereo_control(false)` on the halves, `.stereo_focused(either)`
   on the wrapper (`library::play_button`).
7. **Use relative depths.** `stereo_depth` (absolute) only for overlays and the plane; everything
   else lifts from its parent, so nesting stays right when the component moves.
8. **Check it.** `tools/stereo-shots.sh DIR target/release/quiver-app SCENE` renders a demo
   screen mono and packed and measures every element (`DIR/SCENE-disparity.png`,
   `-anaglyph.png`, `-evidence.json`); add a scene line for a new screen.
   `crates/quiver-ui/tests/stereo_depth.rs` checks the rules through real elements.

## 14. Tuning in the glasses: a checklist

For a session with the XReal Air 2 Ultra in 3D (side-by-side) mode under HypXRland, then the
Quest. Nothing here changes the config; the person decides about the rule in step 2.

1. **Detected.** `hyprctl -j monitors`: the XReal's monitor has `"stereo": "sbs"`. Put Quiver on
   it. Settings → Display says "In 3D now: on <monitor>, a stereo display, at 100%".
   `hyprctl -j clients` for Quiver: `xdgTag: stereo:sbs`, `tags` has `stereo-sbs`, and its
   `stereo` field says `sbs`. The app's log has `stereo ui: … packing side by side`.
2. **Not doubled.** The window must look like one app, not two squeezed copies side by side. If it
   is doubled, the rule isn't matching: check the `stereo-<layout>` tag rule, or add the xdg rule
   ([§7](#7-the-hyprland-rule-and-the-two-ways-to-tag)) and set `QUIVER_STEREO_UI_TAG=xdg`.
3. **The first frame.** Move the window off the XReal and back. Watch for one doubled frame as 3D
   turns on: none expected with the xdg rule; a frame or so with only the window tag.
4. **Fusion, screen by screen.** Library (hero art flat, its text and Play a step in front; tiles
   above the grid; the focused tile nearer), the game page (hero, tabs, Overview, Profiles,
   Tweaks, Performance, Saves, Options), Downloads, Activity, Settings, the palette (Ctrl K), a
   game menu (Y), a dropdown (Launch options), a dialog (sign out), a toast, a tooltip. Note any
   element that shows double, and which rung it's on (§4's table).
5. **Ladder.** For each of: card over page, button on card, hovered, focused, overlay over page,
   its focused entry: too little / right / too much. One step is ≈1 px at 100%.
6. **Hover vs focus.** Move the pointer across tiles: each lifts one step; with the pad, focus
   lifts two; a hovered focused tile doesn't lift more. After a few minutes: does hover strain?
   If yes, the fix is `HOVER = 0` in `quiver_perf::stereo::ui` (one line; focus stays).
7. **Edges.** Focus a tile at the window's left and right edges and open the game-list drawer:
   no sliver visible to one eye only.
8. **Text.** Read 12-px captions on cards, on overlays and on focused controls: as sharp as in 2D.
9. **Strength.** Settings → Display → Depth 50 / 100 / 150 / 200%, about ten minutes each on
   the library and a game page. The chosen value is the default to ship.
10. **Window changes.** Resize, maximise and fullscreen (and back) on the XReal: still fused, never
    stretched. A fractional scale on that monitor, if used: same.
11. **Monitor changes.** Move the window to the laptop's display: flat immediately, no squeezed
    frame; `hyprctl -j clients`: `xdgTag: stereo:mono`, no `stereo-sbs` tag. Back: 3D again.
    Unplug the glasses while on them: the app goes flat on the monitor it lands on.
12. **Off.** Settings → Display → 3D: Off: flat right away on the XReal, and the tags withdrawn.
13. **The Quest** (depth desktop on): 1–9 again; ray clicks land on raised buttons; the cursor
    sits a few pixels behind a raised tile (known, [§10](#10-input)).

Report back per step, with the strength chosen and every element that doubled or felt wrong.
