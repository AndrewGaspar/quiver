# Mod options: a pluggable configuration mechanism (design notes)

Status: proposal (2026-11-16), nothing built. Companion to [MODS-V2.md](MODS-V2.md) (the mod
model this extends) and `SIGHT.md` (the in-game overlay, designed separately; it consumes
the contract in [Contract with the in-game overlay](#contract-with-the-in-game-overlay)).

**Naming.** In this codebase *overlay* already means two things: overlayfs mounts
(`ubi_core::overlay`) and the app's own dialogs and palette (`ubi-app/src/ui/overlay.rs`).
*Config* is taken three times: config patches (MODS-V2), `profile_config` folders and the
launch settings (`quiver get/set`). This document calls the new feature **options** (what
games call their settings menus), always writes **in-game overlay** in full, and never uses
"overlay" for anything of its own. See steer 1.

## Problem

Every mod keeps its settings somewhere different, in a format of its own, with its own way to
change them in game:

- **aco-dlss** (the "DLSS" mod for AC Origins): `aco_dlss.ini` next to the game exe, about
  sixty keys across `[upscaler] [reflex] [fg] [cloth] [world]`, documented only by comments.
  It re-reads the file every 64 presented frames (about once a second), except keys read once
  at a fixed moment: `render_scale` (DllMain, writes `PixelDensity` into the game's own
  `ACO.ini`), `[fg] mode`, `present_thread` (swapchain creation), `ngx_log` (NGX init),
  `debug_fill` (texture creation), `descriptor_heap` (device creation). F9 and F10 flip Frame
  Generation and DLSS in game, but those flips live only in memory and are reverted the next
  time the file changes. `[world] npc_density` takes effect "within ~1 minute".
- **ReShade**: `ReShade.ini` plus a preset `.ini` per look; its own in-game UI (Home) writes
  both itself. Effect parameters are described in the shader source by uniform annotations.
- **DXVK / VKD3D-Proton**: `dxvk.conf` (flat `dxgi.maxFrameRate = 60`) or env vars
  (`DXVK_CONFIG`, `DXVK_HUD`, `VKD3D_CONFIG`); read once at device creation.
- **Ultimate ASI Loader plugins**: an `.ini` next to each `.asi`, read at start.
- **Special K**: a huge INI and its own in-game control panel.
- **The game itself**: Ubisoft games keep graphics settings in INI/XML under `Documents`
  (`Assassin's Creed Origins/ACO.ini`, Far Cry's `GamerProfile.xml`), rewritten by the game's
  own menu.
- Elsewhere: JSON, TOML, XML, Lua, the Windows registry, command-line flags.

Today a Quiver user edits these by hand inside an overlayfs upper folder they don't know
exists, and the app can't show what a profile changes, can't undo it, and can't change
anything while the game runs. Mod authors (and the agents writing mods, like the aco-dlss
session) have no way to say "this key is a 1–5 slider and applies live".

## Goals

1. One **declarative description** of a mod's (or game's, or tool's) options, written by the
   mod author, by Quiver, by an agent, or guessed from the file, from which the app, the CLI
   and the in-game overlay all render the same controls.
2. **Values are the person's, per profile**, stored as deltas over the mod's defaults, with
   revisions, provenance (who changed it, when, from where) and undo, like everything else.
3. **Quiver writes the mod's own files**, preserving comments, order and unknown keys, so
   mods need no changes to be configured.
4. **Live where the mod allows it**: aco-dlss can be tuned in game on day one (it already
   re-reads its file), and mods that adopt a tiny SDK get instant, two-way updates.
5. **A contract the in-game overlay can build on** without knowing about files or formats.
6. **Safe**: validation before anything is written, clear "needs restart" marking, crash
   correlation and one-step rollback.

## Non-goals

- Rendering, input capture or injection for the in-game overlay (SIGHT.md).
- Memory poking or patching game code to change values. Mods do that; Quiver configures mods.
- Arbitrary code plugins in the daemon (see [Pluggability](#2-pluggability)).
- Nexus-style "mod configuration as a downloadable recipe" marketplace. Schemas travel with
  mod versions and Quiver's audited catalog only.
- Steam launches (`quiver steam-run`) don't mount mods today, so install-folder options don't
  apply there either until that changes.

## Model

```
Surface   one set of options with a schema and a backend
          ├─ mod surface      shipped with a mod version ("DLSS": aco_dlss.ini)
          ├─ game surface     Quiver catalog, per product ("AC Origins": ACO.ini graphics)
          └─ runtime surface  Quiver catalog, per tool ("DXVK", "MangoHud": env / conf)
Schema    the description: files, pages, option definitions, presets, metrics, actions
Option    one typed value bound to one place (INI key, XML attribute, env var, arg, SDK key)
Value     the person's choice for an option in one profile (absent = the default)
Backend   how a value reaches the program: file adapter at launch, launch settings,
          live file rewrite, or the SDK channel
```

**Resolution** of one option in one launch, lowest to highest:

1. the **default**: the value in the file the mod version ships (for `owner = "quiver"` files)
   or the schema's `default`, or the file's own value (game-written files);
2. **mod config patches** (MODS-V2) that set the same key, in stack order;
3. the **profile's value** (stored, revisioned);
4. a **session value** (set live and not saved; only if steer 12 picks "try first").

Vanilla is a profile for this purpose (id `vanilla`), so game surfaces and fixes get options
on vanilla too.

A profile's option values are a **delta**: only what the person changed. A new mod version
that changes a default reaches everyone who never touched that option, and "reset" means
"delete my value", never "write the old default back".

## 1. Schema

### File and identity

A schema is a TOML file, `quiver-options.toml` (`quiver-options.json` with the same fields is
read too, as for `quiver-mod.toml`). Unknown fields fail loudly (`deny_unknown_fields`), except
under `x-*` keys reserved for experiments.

- `surface`: a stable id chosen by the author (`aco-dlss`). Values are stored under
  `(mod id, surface)`, so they survive new versions, pins and `merge-versions`.
- `format = 1`: the schema format version. A newer format than Quiver knows is refused with a
  clear message; older ones are upgraded in memory.
- `[file.<id>]`: the files the surface binds to, each with `path`, `root` (`install`: the
  merged install folder, default for mod surfaces; `prefix`: the Wine prefix; `docs`: shortcut
  for the prefix user's `Documents`), `format`, `owner` and `reload` (below). `*` in a path
  component matches any one name, as config patches do.
- Option ids are dotted (`fg.toggle_key`). For INI the id defaults to `section.key` and needs
  no binding; anything else binds explicitly (`file`, `section`/`element`/`pointer`, `key`).

### Types

| Type | Fields | Control |
|---|---|---|
| `bool` | `format`: `01` (default), `truefalse`, `TrueFalse`, `onoff`, `yesno`, or `{on, off}` | toggle |
| `enum` | `values = [{value, label, help, level}]`, `open = true` allows other text | segmented / list |
| `int`, `float` | `min`, `max`, `step`, `unit`, `scale = "log"`, `marks = [{value, label}]`, `special = [{value, label}]`, `precision` | slider or stepper; `special` values are named sentinels (0 = "no cap", `auto`) shown as a choice next to the slider |
| `string` | `pattern`, `max_len`, `multiline` | text |
| `path` | `relative_to` (`install`, `prefix`, `docs`), `style` (`windows`, `unix`), `filter` | picker |
| `key` | `format` (`vk-name` "F9", `vk-hex` "0x78", `dik`, `reshade` "36,0,0,0", `sdl`), `allowed`, `none` | key capture |
| `color` | `format` (`#rrggbb`, `#rrggbbaa`, `r,g,b` 0–255, `r,g,b,a` 0–1, `0xAARRGGBB`) | swatch + picker |
| `list` | `item` (any scalar type), `sep` (default `,`), `min_items`, `max_items` | chip list |

Every option also takes: `label`, `help` (one or two plain sentences), `default` (only when the
file has none), `level` (`basic`, `advanced`, `debug`; the app shows basic, a switch shows the
rest; the in-game overlay shows basic and advanced), `apply` (below), `when`, `danger`
(a sentence; confirmation needed to change it), `experimental` (a badge), `renamed_from`
(old ids, for migrations), `overlay = false` (not shown in game: paths, long strings).

### Apply modes

| `apply` | Meaning | Shown as |
|---|---|---|
| `live` | the running program picks the change up by itself (`latency = "about a minute"` optional) | "Live" |
| `at` | read at a specific moment; `at = "swapchain creation (change resolution or restart)"` | "At swapchain creation" |
| `restart` | read at start | "Restart" |
| `launch` | Quiver-side only: env, args, DLL overrides; always next launch | "Next launch" |

`live` is a promise about the program, not about Quiver. Quiver delivers live changes only
through a backend that can reach the running program (the file has `reload`, or the SDK is
connected); otherwise a `live` option is saved and marked "applies when the mod re-reads it".

### Grouping, conditions, presets

- `[[page]]` (`id`, `title`, `options = [...]` in order, optional `[[page.group]]` with
  `title` and `options`). Options not listed on any page land on an "Other" page, so a
  schema can start as a flat list.
- `when`: visibility. A table of option id → allowed values (`{ "fg.mode" = "d3d12" }`), or
  `{ min, max }` for numbers; all entries must hold. `when_any = [ {…}, {…} ]` for OR. Hidden
  options keep their values. A deliberately tiny language: agents can write it, Quiver can
  validate it, and nothing evaluates code.
- `[[preset]]`: a named set of values (`set = { … }`), applied as one change ("Busy cities").
  Presets show as buttons on their page and as one-step choices in game.

### SDK-only entries

- `[metric.<id>]`: a value the running mod reports (FG on/off right now, render
  resolution, upscaler cost, base FPS, Reflex latency). Display only. The descriptor fields
  and kinds (`state` and `text` for status values; `gauge`, `counter`, `series`, `frame`,
  `event` for performance data) are defined in [SIGHT-HUD.md](SIGHT-HUD.md) §3.3, and values
  are delivered through its metrics model (decided 2026-11-16: readouts were folded into
  metrics, SIGHT-HUD §3.13). A `[readout.<id>]` table from an older draft is still read, as
  `[metric.<id>] kind = "text"`.
- `[action.<id>]`: a button the mod handles (toggle FG, reload shaders), optionally
  `hotkey_option = "<key option>"` naming the mod's own binding for it.

### Example: `quiver-options.toml` for aco-dlss

Shipped in the package folder next to `aco_dlss.ini` (see [Agent workflow](#agent-workflow)).
The `apply` classifications come from reading `src/core/config.cpp` and the ini's comments;
the mod's author should confirm them, since it's their contract.

```toml
format = 1
surface = "aco-dlss"
title = "DLSS"
help = "DLSS 4.5 upscaling, NVIDIA Reflex and DLSS Frame Generation for AC Origins."

[file.ini]
path = "aco_dlss.ini"      # root = "install": next to ACOrigins.exe in the merged install
format = "ini"
owner = "quiver"           # the mod only reads it; Quiver renders it (shipped file + values)
reload = "poll"            # the mod re-reads it about once a second: Quiver may rewrite it live

[[page]]
id = "upscaling"
title = "Upscaling"
options = ["upscaler.enabled", "upscaler.preset", "upscaler.render_scale",
           "upscaler.toggle_key", "upscaler.mip_bias", "upscaler.mip_bias_offset"]

[[page]]
id = "latency"
title = "Latency and frame generation"
options = ["reflex.mode", "reflex.fps_limit", "fg.mode", "fg.enabled", "fg.toggle_key",
           "fg.pacing", "fg.refresh_hz", "fg.present_thread"]

[[page]]
id = "world"
title = "World"
options = ["world.npc_density", "world.npc_distance", "world.lod_distance",
           "cloth.mode", "cloth.scale"]

[option."upscaler.enabled"]
type = "bool"
label = "DLSS"
help = "Off leaves the game's own TAA alone."
apply = "live"

[option."upscaler.preset"]
type = "enum"
label = "DLSS model"
values = [
  { value = "M", label = "M: DLSS 4.5, performance" },
  { value = "L", label = "L: DLSS 4.5, ultra performance" },
  { value = "K", label = "K: DLSS 4 transformer" },
]
open = true                # "default" and older letters are still accepted
apply = "live"

[option."upscaler.render_scale"]
type = "float"
label = "Render scale"
help = "Written into the game's ACO.ini (PixelDensity) when the game starts."
min = 0.33
max = 1.0
step = 0.01
special = [{ value = 0, label = "Use the game's Resolution Scale" }]
marks = [
  { value = 0.5, label = "Performance" }, { value = 0.58, label = "Balanced" },
  { value = 0.667, label = "Quality" }, { value = 1.0, label = "DLAA" },
]
apply = "restart"

[option."upscaler.toggle_key"]
type = "key"
format = "vk-name"
allowed = "F1-F24"
none = "none"
label = "DLSS / TAA switch key"
apply = "live"

[option."upscaler.mip_bias"]
type = "float"
label = "Texture LOD bias"
special = [{ value = "auto", label = "Automatic (from the render scale)" }]
min = -3.0
max = 1.0
step = 0.05
level = "advanced"
apply = "live"

[option."upscaler.mip_bias_offset"]
type = "float"
label = "Automatic bias offset"
min = -1.0
max = 1.0
step = 0.05
when = { "upscaler.mip_bias" = "auto" }
level = "advanced"
apply = "live"

[option."reflex.mode"]
type = "enum"
label = "NVIDIA Reflex"
values = [
  { value = "off", label = "Off" },
  { value = "on", label = "On" },
  { value = "boost", label = "On + Boost", help = "Keeps GPU clocks high." },
]
apply = "live"

[option."reflex.fps_limit"]
type = "float"
label = "Frame rate cap"
unit = "fps"
min = 30
max = 360
step = 1
special = [{ value = 0, label = "No cap" }]
help = "Through Reflex; e.g. 160 on a 165 Hz VRR display."
apply = "live"

[option."fg.mode"]
type = "enum"
label = "Frame Generation"
values = [
  { value = "off", label = "Off" },
  { value = "d3d12", label = "On (D3D12 route)" },
  { value = "on", label = "Vulkan route (research; never generates)", level = "debug" },
]
apply = "restart"

[option."fg.enabled"]
type = "bool"
label = "Generate frames"
help = "Whether Frame Generation runs right now; the toggle key flips it in game."
when = { "fg.mode" = "d3d12" }
apply = "live"

[option."fg.toggle_key"]
type = "key"
format = "vk-name"
allowed = "F1-F24"
none = "none"
label = "Frame Generation key"
when = { "fg.mode" = "d3d12" }
apply = "live"

[option."fg.pacing"]
type = "bool"
label = "Smooth pacing"
help = "Shows the real frame half a frame after the generated one."
when = { "fg.mode" = "d3d12" }
level = "advanced"
apply = "live"

[option."fg.refresh_hz"]
type = "int"
label = "Display refresh rate"
unit = "Hz"
min = 30
max = 500
special = [{ value = 0, label = "Not set" }]
when = { "fg.mode" = "d3d12" }
level = "advanced"
apply = "live"

[option."fg.present_thread"]
type = "enum"
label = "Present thread"
values = [{ value = "1", label = "On" }, { value = "0", label = "Off (inline; stalls the game)" }]
when = { "fg.mode" = "d3d12" }
level = "advanced"
apply = "at"
at = "swapchain creation"

[option."fg.hide_handle64"]
type = "bool"
label = "Hide 64-bit image view handles"
level = "debug"
danger = "Frame Generation fails with this on; it exists for experiments."
apply = "restart"

[option."fg.extra"]
type = "list"
item = { type = "string", pattern = '^DLSSG\.\w+:\d+$' }
label = "Extra DLSS-G parameters"
level = "debug"
apply = "live"

[option."world.npc_density"]
type = "float"
label = "Crowd density"
help = "Multiplies the cap on ambient characters. 3 to 5 lifts it out of the way."
min = 1
max = 5
step = 0.5
unit = "×"
apply = "live"
latency = "about a minute"

[option."world.npc_distance"]
type = "float"
label = "Crowd spawn distance"
min = 1
max = 3
step = 0.25
unit = "×"
experimental = true
apply = "live"
latency = "about 10 seconds"

[option."world.lod_distance"]
type = "float"
label = "Draw distance"
min = 0.5
max = 4
step = 0.25
unit = "×"
apply = "live"

[option."cloth.mode"]
type = "enum"
label = "Cloth simulation"
values = [
  { value = "game", label = "Game default" },
  { value = "render", label = "Every frame", help = "Never slower than the game; costs CPU in crowds." },
  { value = "scale", label = "Scaled (diagnostics)", level = "debug" },
]
apply = "live"

[option."cloth.scale"]
type = "float"
label = "Cloth interval scale"
min = 0.25
max = 4
when = { "cloth.mode" = "scale" }
level = "debug"
apply = "live"

[[preset]]
id = "crowds"
label = "Busy cities"
help = "About 2.6–3× Alexandria's crowds. Costs CPU."
set = { "world.npc_density" = 5, "world.npc_distance" = 2 }

[metric."fg.state"]            # needs the SDK: F9 flips FG without touching the file
kind = "state"
label = "Frame Generation"
short = "FG"
values = [
  { value = "off", label = "off", level = "dim" },
  { value = "on", label = "on" },
  { value = "failed", label = "failed", level = "bad" },
]
roles = ["fg.state"]           # the rest of aco-dlss's metrics: SIGHT-HUD §3.15

[action."fg.toggle"]
label = "Toggle Frame Generation"
hotkey_option = "fg.toggle_key"
```

Keys in `aco_dlss.ini` that this schema doesn't describe (`jitter_x`, `ngx_log`, `skip`, …)
still show, under **Other keys**, as raw text with types guessed (see inference below), so a
partial schema is never a dead end.

### Where schemas come from, and precedence

| Source | Where it lives | Trust |
|---|---|---|
| **(a) Author** | `quiver-options.toml` in the mod package or plain folder, stored with the **version** | the mod's own contract |
| **(b) Catalog** | `options/<id>.toml` in Quiver's repo, compiled in like built-in mods, with a `[match]` block: mod manifest ids, file names + SHA-256 of the shipped file or the main DLL, product ids, version ranges | audited (MODS-V2 decision 3) |
| **(c) Local** | written by a person or an agent for an installed mod: `mods/<product>/options/<mod id>.toml`, revisioned | local |
| **(d) Inferred** | generated on demand from the file, never stored unless promoted to (c) | a guess |

Precedence is **per surface, whole schema**: the first of a, b, c, d that exists is used, and
keys it doesn't describe fall through to "Other keys" (inferred). No deep merging of two
schemas in the MVP: per-key merging makes it impossible to tell why a slider has the range it
has. A catalog schema can later `extends = "aco-dlss"` and override single fields (a known
crash above 4, a better label), recorded as such (steer 4).

A version without a schema of its own **carries forward** the previous version's, as a plain
folder already keeps the current manifest (MODS-V2 "Adding a version"). Adding a version
lints the carried schema against the new files: keys that vanished, new keys without a
description, types the new defaults don't fit.

### Inference

Quiver can guess a schema from any INI-like file, good enough for "Other keys" and as a
starting point for authors (`options infer`):

- comments directly above a key become `help`;
- `a | b | c` (or `a, b or c`) in that comment becomes an enum (aco-dlss: `; dlss | bilinear`);
- `0`/`1`, `true`/`false`, `on`/`off` values become bools; numbers become int/float with no
  range; `F1..F24`, `VK_*` become keys; `#rrggbb` becomes a color;
- section → page; keys whose name or comment says `debug`, `experiment`, `trace` → `debug`;
- BepInEx-style comment metadata (`# Setting type: Boolean`, `# Default value:`,
  `# Acceptable value range: From 0 to 5`) is parsed exactly.

Everything inferred has `apply = "restart"` (unknown), and the app marks it "guessed".

## 2. Pluggability

The plugin unit is **the schema (data)**. Formats are **built-in Rust adapters**. There is
**no scripting runtime** in the MVP.

| Option | For | Against |
|---|---|---|
| Declarative schemas + built-in adapters (**recommended**) | auditable data, agents can write and lint it, one implementation of round-tripping per format, no trust problem | an odd format needs a Quiver release |
| + WASM adapters (wasmtime, bytes in, edits out, no FS) | sandboxed, any language, deterministic | a toolchain for authors, a runtime in the daemon, versioned ABI, hard to audit |
| + Rhai/Lua transforms | easy to write | a second language in the product, weaker sandbox story, same audit problem |

Built-in adapters, each a parse → get/set → render round trip that changes only the bytes it
must (the property `confpatch.rs` already has for INI and XML, including BOMs, UTF-16 and line
endings):

| Adapter | Phase | Notes |
|---|---|---|
| `ini` | 1 | today's `confpatch::Ini`: case-insensitive sections/keys, repeated keys, preamble keys |
| `xml` | 2 | today's `confpatch::Xml` (attributes); add element text |
| `kv` | 2 | flat `key = value` with `#`/`;` comments: `dxvk.conf`, MangoHud, `.cfg` files |
| `json` | 2 | span-preserving editor (JSONC comments tolerated); `pointer = "/graphics/fov"` |
| `toml` | 2 | `toml_edit` (format-preserving) |
| `env` | 2 | values become launch settings (`env.NAME`), merged like a mod's `[launch]`; `DXVK_CONFIG`-style "one var holds `k = v; k = v`" via `env_list` |
| `args` | 2 | values become arguments (`-dx12`, `--fov=90`) |
| `lines` | 3 | regex per key (`'^\s*r_fov\s*=\s*(?P<v>[^;]*)'`) for Lua, `.cfg` with `set k "v"` and other line formats: the escape hatch before anyone asks for code |
| `registry` | 3 | Wine `user.reg`/`system.reg` edited offline before wineserver starts; live through the bridge (`RegSetValueExW` inside the prefix) |
| `sdk` | 2 | no file: the value exists only through the SDK channel and Quiver's store |

The same adapters serve **config patches**: `PatchFormat` grows the same formats, so a mod's
fixed edits and the person's chosen values go through one engine and one record.

If a real mod ever needs logic beyond `lines` (a binary format, values derived from other
values), the answer is a WASM adapter with a fixed interface (`parse(bytes) -> keys`,
`set(bytes, edits) -> bytes`), no imports, fuel-limited, shipped only in Quiver's audited
catalog (steer 2).

## 3. Storage and resolution

### Values

`games/<product>/options/<profile id>.jsonl`, whole-state revisions like `saves.jsonl`:

```json
{"revision":7,"at":"2026-11-16T21:19:04Z","source":"in-game","session":"s-4kq2",
 "message":"Changed in game 21:04–21:19",
 "changes":["DLSS world.npc_density 3 → 4","DLSS fg.enabled on → off"],
 "values":{"m7d2k1x/aco-dlss":{"world.npc_density":4,"fg.enabled":false,"upscaler.preset":"K"},
           "game/aco":{"Graphics.PixelDensity":0.667}},
 "raw":{"m7d2k1x/aco-dlss":{"ini:fg/skip":"eval"}}}
```

- Keyed by `(mod id, surface)` for mod surfaces (`game/<surface>` and `runtime/<surface>`
  otherwise). Mod ids are stable across versions and merges (MODS-V2 decision 7), so a value
  set on DLSS v3 is there on v4 and when a profile pins v2.
- Typed JSON values, normalized by the schema; `raw` holds strings for keys with no schema
  (bound by `file:section/key`).
- No file means revision 0: no values, everything default.
- `source` uses the existing actors (`gui`, `cli`, agent tokens) plus `in-game` (the in-game
  overlay), `game` and `mod:<id>` (changes imported from files or reported over the SDK).
- Deleting a profile keeps its options history, as profile deletion keeps saves.

Why not inside `Profile` revisions: profile history would fill with slider moves, and a
profile restore would silently revert options. Instead, play sessions record the options
revision next to `profile_revision` (`PlaySession.options_revision`), "known good" tags apply
to the pair, and restoring a profile revision offers to restore the options revision that ran
with it (steer 6).

### Scopes

MVP: **profile only**, plus "Copy to other profiles…" and "Compare with profile". Later, if
asked for: a mod-wide layer ("for every profile that stacks DLSS") between defaults and the
profile (steer 5). A game-wide layer isn't needed: vanilla is a profile, and game surfaces
in a folder without `profile_config` are per-launch reconciled anyway (below).

### Schema migration across versions

- Option ids are the contract. `renamed_from = ["old.id"]` maps stored values on read; the
  next revision rewrites them under the new id.
- A stored value that doesn't validate against the resolved version's schema (type changed,
  enum value gone, range narrowed) is **dormant**: kept, not applied, shown with a warning
  ("r12 set fg.mode=on; v5 only has off and d3d12; using the default").
- Options a version no longer has are dormant too, folded under "Not used by v5". Pinning
  back to v4 brings them back to life. Nothing is deleted by a version change.

## 4. Backends and round-tripping

### Where written files live

| File | Where Quiver writes | When |
|---|---|---|
| A mod's own file, `owner = "quiver"` (aco_dlss.ini) | the profile's install upper (`profiles/<id>/upper/aco_dlss.ini`) | rendered before every launch (the namespace isn't mounted yet) and live inside the running namespace |
| A file the program writes, `owner = "program"` (ReShade.ini, ACO.ini) under `root = "install"` | same upper, patched in place | before launch; live only through the SDK |
| A prefix file (`root = "prefix"`, `docs`) | the profile's copy if the folder is in `profile_config`, else the prefix's file, exactly as config patches do (`confpatch::View`) | before launch, reconciled per launch |
| `env`, `args` | launch settings | at launch |

**`owner = "quiver"`: render, don't patch.** The upper copy is regenerated each launch from
the **shipped file of the resolved version** (the topmost lower layer that has it) plus the
profile's values. Patching a stale copy-up instead would hide every later version's new keys,
defaults and comments behind the first copy forever, which is today's behaviour for any edited
mod file. Since the mod never writes this file, anything else found in the upper copy is a
person's hand edit, imported first (below).

**`owner = "program"`: patch.** Keep the program's copy and set only keys with profile values,
using `confpatch` semantics: the file's own values are recorded, keys no longer wanted go
back unless changed since.

**Vanilla and install files.** Options on install files need the overlayfs mount (the base
install is never written). A launch with install-file option values mounts an overlay even
for vanilla, with `profiles/vanilla/upper`, as "vanilla with fixes" already does.

**The record** (`mods/<product>/options-applied.json`, beside `patches.json`): per physical
file, the hash of the shipped file it was rendered from, the hash of what Quiver wrote, and the
values written. It is what makes external edits detectable.

### When the game or the mod writes the file itself

Example: the person opens ReShade's own menu, changes `Sharpness`, ReShade rewrites its
preset; or the game's own menu rewrites `PixelDensity` in `ACO.ini`.

- **Detection**: at session end Quiver compares each written file with its record. During a
  session, while any client is subscribed (the app's options page, the in-game overlay),
  quiverd stats the watched files every 2 s through the running namespace (below). SDK mods
  report changes directly.
- **Rule: last writer wins, and every external change is a revision.** A schema'd key whose
  file value differs from what Quiver wrote becomes the profile's value, recorded with
  `source = "game"` or `"mod:<id>"` ("Changed in ReShade's menu"), undoable like any other.
  If Quiver also set that key during the same session, the later write wins and both are in
  history (steer 7).
- Keys without a schema are imported as `raw` values only for `owner = "quiver"` files (a hand
  edit); for program-owned files they're the program's business and left alone (window
  positions, tutorial flags).
- A file the person edited by hand between sessions is imported the same way at the next
  launch, before rendering.

## 5. Live apply

Four ways a running program can get a new value, in order of preference:

| Channel | How | Who it serves | Verdict |
|---|---|---|---|
| **SDK** | the mod links `quiver_mod.h`; values are pushed over a pipe the bridge owns | mods that opt in (aco-dlss next) | phase 2: instant, two-way, metrics, actions |
| **File rewrite** | Quiver rewrites the file inside the running namespace; the mod re-reads it | mods that poll or watch their file (aco-dlss today, MangoHud) | **MVP**: works for aco-dlss with zero mod changes |
| Key emulation | send the mod's hotkey | toggles only | rejected: Quiver can't know the resulting state, and focus/input races with the in-game overlay |
| Memory poking | | | never |

### File rewrite (MVP)

The game's overlayfs mount exists only inside the launch's namespace. quiverd never enters it
(`ubi_core::overlay`), and writing into an upper folder underneath a live overlayfs mount is
undefined behaviour. Instead quiverd writes **through the namespace**: via
`/proc/<pid>/root/<install path>/…` of a process in the session (same uid, the namespace is a
child of ours, so the access check passes; Yama only restricts attach), or, as a fallback,
with a short-lived helper started by `Namespace::enter`. The write goes through overlayfs
(copy-up happens normally), as temp file + rename in the same folder, so the mod never reads a
half-written file. Wine's `GetPrivateProfileString` re-reads a file whose timestamp changed.

Only files with `owner = "quiver"` and a `reload` are rewritten live. A `live` option in a
file without `reload` is saved and marked "applies when the mod re-reads its file".

For aco-dlss this gives real-time tuning in game today: change `world.npc_density`, Quiver
rewrites `aco_dlss.ini`, the mod notices within a second and applies it within a minute.

One caveat aco-dlss already handles well: its F9/F10 flips are runtime state that a file
change resets. The in-game overlay therefore can't show the true FG state without the SDK
(the `fg.state` metric); the file channel shows the configured state.

### SDK (phase 2)

**Transport.** The bridge is the game's parent in every Quiver launch (and `steam-run`), and
already owns named pipes and a connection to quiverd. It creates
`\\.\pipe\quiver-mods-<session>` and puts the name in the game's environment as
`QUIVER_MODS_PIPE` (with `QUIVER_SESSION`, already planned). The SDK connects to it; the
bridge relays each mod connection to quiverd as a stream (a new `BrokerRequest::ModChannel`
that turns the connection into a duplex message stream), and quiverd scopes it to the
session's profile. Mods never see account data, a token or a TCP port (steer 10).

**Wire format.** 4-byte little-endian length, then a UTF-8 JSON object with a `t` field.
Unknown `t` values and unknown fields are ignored both ways. `proto` is an integer that only
grows; each side speaks `min(theirs, mine)`. The current protocol is **2**: it adds the metrics
messages (`metrics`, `metrics_ok`, `m`, `mark`, `ring_req`, `ring`, `perf_ctl`, and `caps` /
`perf` fields on `hello` / `welcome`), specified in [SIGHT-HUD.md](SIGHT-HUD.md) §3.6. The
table below is the options part.

| Direction | `t` | Fields | Meaning |
|---|---|---|---|
| mod → | `hello` | `proto`, `sdk`, `mod` (surface id), `pid`, `schema`? (inline TOML/JSON, optional) | first message |
| ← | `welcome` | `proto`, `session`, `profile`, `values` {id: {text, json}} | full snapshot |
| ← | `set` | `seq`, `values` {id: {text, json}} | person changed values; apply them |
| mod → | `ack` | `seq`, `rejected` {id: reason}? | applied (or not: shown in the UI) |
| mod → | `changed` | `values`, `persist` (bool) | the mod changed them itself (its own menu: `persist`; a hotkey flip: not) |
| mod → | `readout` | `values` {id: text} | protocol 1 only: display values (≤ 4/s). quiverd maps each to a `text` metric's `m` sample; protocol-2 SDKs send `metrics` + `m` instead |
| ← | `action` | `id`, `arg`? | run an action |
| mod → | `log` | `level`, `text` | goes to the session log |
| both | `bye` | | clean shutdown |

`text` is exactly what the file would contain (`"1"`, `"d3d12"`, `"F9"`), so a mod that
already parses its INI reuses that code unchanged; `json` is the typed value for mods that
want it.

**Rules.** quiverd validates every `set` before sending; values in `changed` are validated
too, but a value the mod says it runs with is recorded as fact (marked invalid if it is). A
mod channel can only touch its own surface. If the pipe is absent (not under Quiver, an old
bridge), every SDK call is a cheap no-op and the mod keeps reading its file.

**The header** (sketch; single-file C99, MinGW and MSVC, no dependencies beyond kernel32,
0BSD-licensed so any mod can vendor it):

```c
/* quiver_mod.h — Quiver mod options SDK, protocol 2 (options here; metrics: SIGHT-HUD §3.14).
 * In exactly one .c/.cpp file:  #define QUIVER_MOD_IMPLEMENTATION  before including.
 * All strings are UTF-8. No call blocks for more than a pipe write; none allocate per frame. */
#ifndef QUIVER_MOD_H
#define QUIVER_MOD_H
#ifdef __cplusplus
extern "C" {
#endif

#define QUIVER_MOD_PROTOCOL 2

typedef struct quiver_mod quiver_mod;          /* opaque */

typedef struct quiver_value {
    const char *id;     /* option id, e.g. "world.npc_density" */
    const char *text;   /* as the file would hold it, e.g. "4" */
    const char *json;   /* typed JSON, e.g. "4" or "\"d3d12\"" or "false" */
} quiver_value;

/* Called with a batch of changed values. Return 0 to accept; non-zero rejects the batch
 * (quiver_reject() can name a reason per id first). */
typedef int  (*quiver_on_values)(void *user, const quiver_value *values, int count);
typedef void (*quiver_on_action)(void *user, const char *action_id, const char *arg /* may be NULL */);

enum {
    QUIVER_POLL        = 1u << 0,  /* callbacks run inside quiver_poll() on your thread
                                      (e.g. the render thread); default: an SDK thread */
    QUIVER_NO_SNAPSHOT = 1u << 1,  /* don't deliver the welcome snapshot as a callback */
};

/* Connects if running under Quiver (QUIVER_MODS_PIPE is set); otherwise returns a handle
 * on which every call is a no-op and quiver_connected() is 0. `surface` is the schema's
 * `surface` id. `schema_utf8` may be NULL (the mod's shipped quiver-options.toml is used). */
quiver_mod *quiver_open(const char *surface, const char *schema_utf8, unsigned flags);
int  quiver_connected(const quiver_mod *m);

void quiver_on_values_cb(quiver_mod *m, quiver_on_values cb, void *user);
void quiver_on_action_cb(quiver_mod *m, quiver_on_action cb, void *user);
void quiver_reject(quiver_mod *m, const char *id, const char *reason);

/* Last known value (snapshot or later set); returns 0 if unknown. `buf` gets `text`. */
int  quiver_get(quiver_mod *m, const char *id, char *buf, int buf_len);

/* The mod changed a value itself. persist=1: its own menu (saved to the profile);
 * persist=0: runtime state such as a hotkey flip (shown, not saved). */
void quiver_changed(quiver_mod *m, const char *id, const char *text, int persist);

/* Convenience: declares (once) a `text` metric named `id` and sets it. The full metrics API
 * (quiver_metric_declare, quiver_gauge, quiver_state, quiver_frame_submit, ...) is part of this
 * header; see SIGHT-HUD §3.14. */
void quiver_readout(quiver_mod *m, const char *id, const char *text);
void quiver_log(quiver_mod *m, int level, const char *text);

void quiver_poll(quiver_mod *m);   /* QUIVER_POLL mode: deliver pending callbacks now */
void quiver_close(quiver_mod *m);

#ifdef __cplusplus
}
#endif
#endif /* QUIVER_MOD_H */
```

aco-dlss's integration would be about twenty lines: `quiver_open("aco-dlss", NULL,
QUIVER_POLL)` in `DllMain`'s preinit, `quiver_poll` where it already calls `config_reload()`
every 64 frames, a callback that feeds `text` into the same parser `load()` uses, and
`quiver_changed("fg.enabled", on ? "1" : "0", 0)` plus `quiver_state(q, fg_state, on ? "on" :
"off")` in the F9 handler (its other metrics: SIGHT-HUD §3.15).

The SDK also ships a tiny Linux-side test server (`quiver dev mod-channel GAME`) that speaks
the protocol on a pipe-equivalent so authors can test without a game.

## 6. Surfaces

### Daemon requests (`ubi_api::options`, appended to `ModsRequest`)

| Request | Reply | Notes |
|---|---|---|
| `OptionsList { product_id, profile, level? }` | `Vec<SurfaceState>` | schema (resolved), source of schema, per option: effective value, provenance, default, apply, live channel available, dormant/invalid flags; plus raw "Other keys" |
| `OptionsSet { product_id, profile, ops: Vec<OptionOp>, live: Live, message, confirmed }` | `OptionsChange` (revision, per-op outcome: `applied_live`, `saved_restart`, `saved_at(...)`, `dormant`, `rejected(reason)`) | `OptionOp`: `Set {surface, id, value}`, `Reset {surface, id}`, `Preset {surface, id}`, `Raw {surface, binding, value}`. `Live`: `IfRunning` (default), `No`, `Only` (session, not saved) |
| `OptionsDiff { product_id, profile, against }` | `Vec<OptionDiff>` | against `default`, another profile, or a revision |
| `OptionsHistory { product_id, profile }` | `Vec<OptionsRevision>` | |
| `OptionsRestore { product_id, profile, revision, confirmed }` | `OptionsChange` | agents need `confirmed` |
| `OptionsSchema { product_id, mod, version? }` | `SchemaInfo` | the resolved schema, its source, lint findings |
| `OptionsSchemaSet { product_id, mod, toml, message }` | `SchemaInfo` | writes a local (c) schema, revisioned |
| `OptionsInfer { product_id, mod, file }` | TOML text | |
| `OptionsLint { path }` | findings | a package folder before `version add` |
| `OptionsInvoke { product_id, surface, action, arg }` | ok/err | SDK actions in a running session |

Events (after `Subscribe`): `OptionsChanged { product_id, profile, revision, source, changes }`,
`OptionsLive { session, surface, values, by }` (session-level changes, including unsaved and
`changed persist=0`), `ModChannel { session, surface, connected }`. Values a running mod
reports are metrics: listed with `PerfCatalog` and watched with `PerfSubscribe`
(`ubi_api::perf`, SIGHT-HUD §3.11), which replaced the `OptionsReadout` event.

`ActiveSession` gains `session` (id), `profile`, `options_revision` and `pids`, which the
in-game overlay and the CLI need to address "the running game".

### CLI: `quiver mods <game> options …`

```
$ quiver mods origins options --profile DLSS
DLSS v4 (current) · aco_dlss.ini · schema shipped with v4
  Upscaling
    upscaler.enabled        on                          default
    upscaler.preset         K  DLSS 4 transformer       you, r5 (default M)        live
    upscaler.render_scale   0.667  Quality              you, r2                    restart
    upscaler.toggle_key     F10                         default                    live
  Latency and frame generation
    reflex.mode             on                          default                    live
    fg.mode                 d3d12                       default                    restart
    fg.enabled              off                         in game, r7 (default on)   live
    fg.toggle_key           F9                          default                    live
  World
    world.npc_density       4 ×                         in game, r7 (default 1)    live, about a minute
  12 advanced, 31 debug options hidden (--all); 2 other keys (--raw)

$ quiver mods origins options set DLSS world.npc_density=5 world.npc_distance=2 --profile DLSS \
    --reason "User wants busier cities"
r8: DLSS world.npc_density 4 → 5, world.npc_distance 1 → 2
AC Origins is running (DLSS): rewrote aco_dlss.ini; the mod applies it within about a minute.

$ quiver mods origins options set DLSS fg.mode=off --profile DLSS
r9: DLSS fg.mode d3d12 → off (restart: applies next launch)

$ quiver mods origins options preset DLSS crowds --profile DLSS
$ quiver mods origins options reset DLSS world.npc_distance --profile DLSS
$ quiver mods origins options diff --profile DLSS --against vanilla
$ quiver mods origins options history --profile DLSS
$ quiver mods origins options restore r6 --profile DLSS
$ quiver mods origins options get DLSS fg.enabled --json
$ quiver mods origins options schema DLSS [--version v3] [--lint]
$ quiver mods origins options schema DLSS --set ./quiver-options.toml
$ quiver mods origins options infer DLSS aco_dlss.ini > quiver-options.toml
$ quiver mods origins options lint ./build/mod
$ quiver mods origins options keys --profile DLSS      # every hotkey in play, with conflicts
```

`--profile` defaults to the game's default profile; `--live-only` sets for the running
session without saving; `--no-live` saves without touching the running game; `--json`
everywhere.

### App

A new **Options** section on the game's Mods tab, for the selected profile (and a deep link,
`quiver open origins --tab mods --options DLSS`):

```
┌ Profile: DLSS ──────────────────────────────────────────────────────────────────────────┐
│ [/ Search options…          ]  [Changed only ○]  [Advanced ○]      Running · live: file │
├──────────────────┬─────────────────────────────────────────────────────────────────────┤
│ DLSS        3 ●  │ Upscaling                                                            │
│  Upscaling       │  DLSS                         [■ On ]                         Live   │
│  Latency & FG  1 │  DLSS model                   [ M | L | K● ]   you · r5     ↺  Live   │
│  World         2 │  Render scale                 ──●───────── 0.667 Quality ↺  Restart │
│  Other keys      │                               [Use the game's]                       │
│ AC Origins       │  DLSS / TAA switch key        [ F10 ]                         Live   │
│  Graphics        │ World                          Preset: [Busy cities]                 │
│ DXVK             │  Crowd density                ────●────── 4 ×  in game · r7 ↺ ~1 min │
│ MangoHud         │  Crowd spawn distance  EXP    ●────────── 1 ×                 Live   │
├──────────────────┴─────────────────────────────────────────────────────────────────────┤
│ What this profile changes (5) · Compare with… · History · Copy to profiles…            │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Generated forms** from the schema; level switch; `when` hides dependent options live.
- **Provenance** on every changed row (you · CLI · agent name · in game · ReShade's menu, and
  the revision), a reset arrow (↺) that deletes the value, and the default in the tooltip.
- **Apply badge** on every row; after a change in a running game the row says "Applied" or
  "Applies next launch", and a banner collects pending restarts ("2 changes need a restart").
- **Search** over labels, ids, help and values across surfaces, also from the command palette.
- **What this profile changes**: a diff list grouped by surface (against defaults, another
  profile or a revision), each row with reset; this is the answer to MODS-V2's "show clearly
  where each profile's settings live".
- **Gamepad**: rows are focusable; A toggles a bool / opens an enum list / starts key capture;
  D-pad left/right steps numbers and cycles enums (hold to repeat; LT/RT ×10 steps); Y resets;
  X opens a number pad or text entry; LB/RB move between pages. `overlay = false` types
  (paths, long strings) are editable with a keyboard only.
- **Schema status** in the surface header: "schema shipped with v4", "Quiver catalog",
  "local, by aco-dlss agent", "guessed, Save as schema…", plus lint warnings.

### Agent workflow

The aco-dlss session (or any mod author) ships options with a version in one command:

```sh
# in the mod's repo: dist/quiver-options.toml next to dist/aco_dlss.ini, copied into build/mod
quiver mods origins options lint build/mod          # schema vs aco_dlss.ini: undescribed keys, bad defaults
quiver --source aco-dlss mods origins version add DLSS build/mod --notes "npc_distance; FG pacing"
#  v5 added (current). Options: schema shipped with v5 (41 options, 18 keys undescribed).
#  Profiles DLSS, DLSS-crowds track DLSS: their values carry over; 1 value dormant
#  (fg.present_thread=2: v5 allows 0, 1).
```

- `quiver-options.toml` at the root of a **plain folder** is taken as the version's schema and
  **not** laid over the install (like whiteouts are taken as deletions); in a package folder
  it sits next to `quiver-mod.toml`. `version add --options FILE` names one explicitly.
- A local schema for an already-installed version: `options schema DLSS --set FILE`.
- Agents may set options on profiles (the "change mod profiles" row of the policy: Allow);
  restoring an options revision, changing `danger` options and **live changes to a running
  game** are Ask (steer 13).

## 7. Keybindings

Keys are options of type `key` with a known encoding, so Quiver can normalize every binding
to one form (W3C `KeyboardEvent.code` names plus modifiers: `F9`, `Shift+Home`) and compare.

`options keys` and the app's **Keys** view list every binding in play for a profile:

| Source | Example |
|---|---|
| mod options (`key` type) | DLSS: F9 (Frame Generation), F10 (DLSS/TAA) |
| known tools (runtime surfaces) | ReShade: Home (`KeyOverlay=36,0,0,0`); MangoHud: Shift_R+F12 (HUD), Shift_L+F4 (reload); Steam: Shift+Tab (overlay), F12 (screenshot) |
| the in-game overlay | its own open/close binding (SIGHT.md); the HUD's Shift+F3 / Ctrl+Shift+F3 (SIGHT-HUD §1.3) |
| the game | catalog entry per product, where known (`[keys]` in the game surface) |
| desktop | Hyprland binds with no modifier other than Super never reach the game; listed as "taken by the desktop" if they overlap |

Conflicts are warnings in the app and `options keys`, never auto-resolved. Key capture in the
app records the chord and shows what else uses it before saving.

**Delegation (phase 2).** An SDK mod can declare an `action` with `hotkey_option` and let
Quiver own the key: the in-game overlay's input layer (which already sees input) dispatches it
as an `action` message, and the mod skips its own `GetAsyncKeyState` polling while
`quiver_connected()`. One place then knows every binding, and the overlay can show "F9: Frame
Generation" in a help strip. Mods without the SDK keep their own keys (steer 14).

## 8. Safety

- **Validate before writing**, in quiverd, for every path (app, CLI, agents, in-game overlay,
  SDK): type, range, enum, pattern, `when` consistency. The front ends validate too, for
  feedback, but the daemon is the authority.
- **`danger` options** need confirmation in the app and `confirmed` from agents; the in-game
  overlay shows them read-only unless the person enables "dangerous options in game".
- **Atomic writes** (temp + rename), never under a live mount except through the namespace.
- **Crash correlation.** Sessions record the options revision at launch and each live change
  with its time. If the game exits abnormally (non-zero exit, no clean SDK `bye`) within 60 s
  of a live change, the app notifies "AC Origins crashed 4 s after Frame Generation key was
  set. Revert this session's in-game changes?" Never automatic (steer 15).
- **Known good.** A session that ran ≥ 10 minutes and exited cleanly tags its
  (profile revision, options revision) as known good. "Restore last known good" restores both.
- **Safe launch.** `quiver play origins --profile DLSS --options defaults` (and an app menu
  item) launches once with the profile's option values ignored, without changing them.
- **Dormant, not destructive.** Values that don't fit a new schema are kept and not applied.

## 9. Interactions with what exists

- **Config patches** stay: they are a mod's fixed edits. When an option binds the same key,
  the profile's value wins and the row says "DLSS sets 0; you override". Both go through one
  adapter engine and one reconcile pass; the options record sits beside `patches.json`.
- **`profile_config`** stays the way a profile gets its own copy of a prefix folder. The
  options page shows which surfaces are per-profile ("ACO.ini: this profile's own copy") or
  shared ("shared with vanilla; values are re-applied at each launch").
- **Launch settings**: `env`/`args` options compile into launch settings after the mods'
  `[launch]` and before the profile's explicit overrides, with clashes reported as setting
  conflicts are today.
- **Mod versions**: schemas belong to versions; values belong to the profile and the mod.
- **Activity**: every options revision is an activity entry with its actor.

## Contract with the in-game overlay

This section is the interface SIGHT.md can rely on. Everything here is daemon-side, so it
holds whether the in-game overlay runs as a Linux Vulkan layer (talking to quiverd's Unix
socket directly) or as a Windows DLL in the game (talking through the bridge's pipe, which
relays the same JSON requests).

**C1. Addressing.** The in-game overlay knows its session from `QUIVER_SESSION` in its
environment. `Request::Sessions` returns `ActiveSession { session, product_id, profile,
options_revision, pids, … }`. All options requests take `product_id` + `profile` from it.

**C2. Discovery.** `OptionsList { product_id, profile, level: Some("advanced") }` returns
everything needed to render, with no file or format knowledge:

```
SurfaceState {
  key: "m7d2k1x/aco-dlss", title: "DLSS", kind: mod|game|runtime,
  schema_source: author|catalog|local|inferred,
  live: { channel: sdk|file|none, connected: bool },
  pages: [{ id, title, groups: [{ title, options: [id…] }] }],
  options: { id → OptionState {
      def: OptionDef,                       // type, label, help, ranges, marks, special, values,
                                            // level, apply, at, latency, when, danger, experimental,
                                            // overlay (false: don't show in game)
      value: json, text: string,            // effective
      default: json,
      from: default|mod_patch(mod)|profile(rev, source)|session(source),
      visible: bool,                        // `when` already evaluated
      pending: none|restart|at(text),       // a saved value the running game hasn't picked up
      dormant: Option<reason>,
  }},
  presets: [{ id, label, help }],
  metrics: [metric id…],                    // the surface's metrics; resolve with PerfCatalog
  actions: { id → { label, hotkey } },
}
```

**C3. Changing values.** `OptionsSet { …, live: IfRunning, ops }` with `source = "in-game"`.
Each op's outcome comes back individually: `applied_live` (delivered and, for SDK mods,
acked), `saved_restart`, `saved_at(text)`, `rejected(reason)`. The in-game overlay must show
the outcome, never assume "live". Values the overlay sends are validated by the daemon;
the overlay should still clamp and step locally from `def` for responsive controls.

**C4. Persistence of in-game edits.** Recommended (steer 12): saved to the profile, coalesced
into one revision per in-game editing burst (closed page, 30 s idle, or session end; message
"Changed in game 21:04–21:19"). `OptionsSet { live: Only }` exists for a "try" UX if the
overlay wants one; `OptionsRevertSession { session }` puts back the values the session
launched with (live where possible).

**C5. Watching.** After `Subscribe`, the overlay gets `OptionsLive` (values changed by anyone:
the app, CLI, an agent, the mod's own menu, a hotkey flip with `persist=false`),
`OptionsChanged` (new revisions) and `ModChannel` (SDK connected/disconnected). For the mod's
reported values it uses `PerfSubscribe { session, select: [the surface's metric ids] }`
(SIGHT-HUD §3.11) while visible. It never polls files.

**C6. Actions and presets.** `OptionsInvoke` runs an SDK action; `OptionOp::Preset` applies a
preset as one change.

**C7. Keys.** `OptionsKeys { product_id, profile }` returns every binding in play, normalized,
with conflicts. The overlay registers its own binding as a runtime surface so conflicts show
everywhere. Delegated actions (§7) are dispatched by the overlay's input layer to
`OptionsInvoke`.

**C8. Control mapping.** Every type has a gamepad behaviour (§6, App), and anything the
in-game overlay can't edit sensibly is marked `overlay = false` by the schema or by type
(`path`, multi-line `string`). Labels are ≤ 40 chars and help ≤ 2 sentences by lint rule, so
they fit a compact in-game panel.

**C9. Guarantees.** Option ids, surface keys and the shapes above are stable within
`ubi_api` (appended, never renamed, like the rest of the protocol). The daemon never writes a
value it didn't validate. The overlay doesn't need to know about files, formats, profiles'
folders, the SDK or the bridge.

**C10. Not provided.** Rendering, input, focus, text entry, screenshots; FPS/frame-time data
and every mod-reported value, which are SIGHT-HUD's metrics model (decided 2026-11-16).
MOD-CONFIG owns options, actions and keys; a surface's metrics share its id as namespace, so
the overlay can show them next to the options.

## Prior art

- **MCM Helper (Skyrim)**: declarative `config.json` (pages, `toggle`/`slider`/`stepper`/
  `enum`/`keymap` controls bound to INI keys, group conditions); defaults in the mod's
  `settings.ini`, the user's changes in a separate `MCM/Settings/<mod>.ini`. Closest analogue;
  the source of "declarative, bound to the mod's INI, values as a delta". The original SkyUI
  MCM (menus built by Papyrus scripts) is what happens without a schema: every mod writes UI
  code.
- **BepInEx ConfigurationManager / r2modman**: typed entries with descriptions and acceptable
  ranges written as comments into `.cfg` files, an in-game editor (F1) and `SettingChanged`
  events; r2modman edits the same files outside the game with a generic form and a text
  fallback. Lessons: metadata next to the value makes inference reliable; live events; always
  keep a raw fallback.
- **Reloaded-II**: config classes with attributes, generated configurator UI, JSON storage,
  `ConfigurationUpdated` event for live changes. Lesson: typed + live, but code-defined
  schemas tie it to one runtime; ours are data.
- **Vortex**: almost no per-mod configuration UI; game extensions offer "INI tweaks" as
  toggleable files. That's our config patches; options go further.
- **ReShade**: uniform annotations (`ui_type`, `ui_min`, `ui_max`, `ui_step`, `ui_label`,
  `ui_tooltip`, `ui_category`, `ui_items`) are a schema in the source; presets store only
  values. Phase 3: generate surfaces from `.fx` annotations, and a small ReShade add-on that
  speaks the SDK (the add-on API can set uniforms live).
- **MangoHud**: flat `key=value` file plus `MANGOHUD_CONFIG` env, reloaded when the file
  changes and on a hotkey. Confirms that file rewrite is a legitimate live channel.

## Phasing

**Phase 1 (MVP): aco-dlss is configurable from the app, the CLI and agents, live.**
- Schema format 1: bool, enum, int, float, string, key; pages/groups; `when`; `level`;
  `apply`; `special`/`marks`; presets; `danger`/`experimental`. Lint and infer.
- Schema sources: author (shipped with a version, carried forward) and inferred "Other keys".
- `ini` adapter on install-root files (`owner = "quiver"` render; `owner = "program"` patch) and
  prefix files (existing `confpatch` View).
- Values store with revisions; resolution; dormant values; external-edit import at session end
  and at launch.
- Live file rewrite through the running namespace for `reload` files.
- Requests `OptionsList/Set/Diff/History/Restore/Schema/Lint/Infer`, events; `ActiveSession`
  additions; `PlaySession.options_revision`.
- CLI `quiver mods <game> options …`; app Options section with search, changed-only, diff,
  reset, gamepad.
- aco-dlss ships `quiver-options.toml` (written by that session; this doc's example is a
  starting point).
- The in-game overlay can be built against C1–C9 from here (file channel only).

**Phase 2: two-way and wider.**
- SDK (`quiver_mod.h`), bridge pipe, broker mod channel, actions, `changed`; aco-dlss adopts
  it (F9 delegation). The pipe, the broker mod channel, `hello`/`welcome` and the JSON metrics
  messages are pulled forward into SIGHT-HUD's phase 1 (its HUD MVP shows aco-dlss's FG state
  and base FPS), so this phase adds the options side on a transport that already exists.
- Keys view, conflict detection, delegation.
- Catalog schemas (b) with `[match]`, starting with the AC Origins game surface (`ACO.ini`
  graphics, so `render_scale` and the game's own setting are shown together) and DXVK/
  VKD3D/MangoHud runtime surfaces.
- Local schemas (c) via `options schema --set`, from the app's "Save as schema".
- `xml`, `kv`, `json`, `toml`, `env`, `args` adapters; `color`, `path`, `list` types in the app.
- Crash correlation, known-good pairs, safe launch.

**Phase 3: the long tail.**
- ReShade (`.fx` annotation importer, preset surfaces, SDK add-on), Special K and common ASI
  plugin descriptors in the catalog, matched by hash.
- `lines` and `registry` adapters; `extends` overrides on catalog schemas; mod-wide scope if
  wanted.
- Only if a real mod needs it: WASM adapters.

## Steers wanted

**Decided 2026-11-16:** the user accepted every recommendation below as written. Each steer's
*Recommend* is the decision; the other options are kept for the record.

1. **Name of the feature.** (a) Options (`quiver mods <game> options`, `ubi_api::options`);
   (b) Config; (c) Settings. *Recommend (a): "config" and "settings" already mean patches,
   `profile_config` and launch settings.*
2. **Plugin unit.** (a) Declarative schemas + built-in Rust adapters (+ `lines` regex later);
   (b) plus WASM adapters now; (c) plus Rhai/Lua transforms. *Recommend (a); WASM only when a
   real mod needs it, catalog-only.*
3. **Schema location.** (a) Separate `quiver-options.toml`, recognized at a package or plain
   folder root; (b) inside `quiver-mod.toml`; (c) JSON only. *Recommend (a): plain-folder
   mods like aco-dlss have no manifest, and schemas get long.*
4. **Schema precedence.** (a) Whole surface: author > catalog > local > inferred, undescribed
   keys raw; (b) per-key merge of all sources; (c) whole surface now, catalog `extends`
   overrides later. *Recommend (c).*
5. **Value scopes.** (a) Per profile only (vanilla is a profile), with copy/compare;
   (b) plus a mod-wide layer; (c) plus game-wide. *Recommend (a) now, (b) if you miss it.*
6. **Where values are stored.** (a) Own per-profile history (`options/<profile>.jsonl`),
   sessions record both revisions; (b) inside `Profile` revisions. *Recommend (a): slider
   moves shouldn't flood profile history, and restores stay deliberate.*
7. **When the game or mod writes the file itself.** (a) Import as an attributed, undoable
   revision (last writer wins); (b) Quiver's value always wins next launch; (c) ask each time.
   *Recommend (a).*
8. **Mod-owned files Quiver edits.** (a) Re-render from the version's shipped file + values
   every launch; (b) patch the profile's copied-up file. *Recommend (a): otherwise new versions'
   keys and defaults never arrive.*
9. **Live channel in the MVP.** (a) File rewrite through the running namespace (aco-dlss works
   live with no changes); (b) wait for the SDK; (c) no live in the MVP. *Recommend (a).*
10. **SDK transport.** (a) Named pipe owned by ubi-bridge, relayed to quiverd; (b) mods
    connect to quiverd over TCP with a session token from the environment; (c) shared memory
    plus an event. *Recommend (a): no new port or secret in the game's environment, and every
    launch path already has the bridge.*
    *Note:* SIGHT-HUD decided (a)'s pipe for options and low-rate metrics, plus shared-memory
    rings in the session dir, created by the bridge, for per-frame data. So part of (c) exists
    either way; this steer is about the options messages.
11. **SDK form.** (a) Single-header C99, 0BSD; (b) a `quiver_mod.dll` mods load; (c) a Rust
    crate with a C ABI. *Recommend (a): vendorable into any MinGW/MSVC mod, nothing to ship
    beside it.*
12. **In-game edits.** (a) Saved to the profile, coalesced per editing burst, "Revert this
    session" available; (b) session-only until "Keep"; (c) choose per change. *Recommend (a).*
13. **Agents and the running game.** (a) Options on profiles Allow; live changes to a running
    game, restores and `danger` options Ask; (b) all Allow; (c) all Ask. *Recommend (a).*
14. **Hotkeys.** (a) Detect and show conflicts only; (b) plus SDK delegation to Quiver's input
    layer in phase 2; (c) Quiver rebinds mods' keys to avoid conflicts. *Recommend (b).*
15. **Crash after a live change.** (a) Offer to revert the session's changes; (b) revert
    automatically; (c) do nothing special. *Recommend (a).*
16. **Game-native options** (ACO.ini graphics as a surface). (a) Phase 2 from the catalog;
    (b) in the MVP alongside aco-dlss (its `render_scale` writes `PixelDensity`); (c) later.
    *Recommend (a).*
