# Quiver: design proposal

Quiver is a third-party launcher for your Ubisoft library on Linux. It is not affiliated with or endorsed by Ubisoft, and it uses no Ubisoft logos or wordmarks. The app is a front end for the `quiverd` daemon, and the `quiver` command drives the same daemon from a terminal or an agent. Internal crate names (`ubi-core`, `ubi-bridge`) are not changed by this document.

This document goes with the mock-ups (the "Mock-ups" tab of the same page). Game art in the mock-ups is the real Ubisoft art for this library. Play times, sizes, sessions, achievements, mods, the second account and agent activity are illustrative.

## Principles

1. **The daemon is the product.** The app, the `quiver` CLI and agents are peers. Every button in the app is one daemon command, and the app can show that command. If the app can do it, a script can do it.
2. **Art when browsing, density when configuring.** The library is image-first. Launch options, mods and saves are dense tools you can drive from the keyboard or a controller.
3. **Every change can be undone.** Launch options are versioned, Proton changes snapshot the prefix, saves are backed up before every sync, and mods never touch the base install.
4. **Show where data comes from.** Show platform data where it exists, and mark what is not wired yet instead of inventing it. Developer, release date and description are left out until there is a source for them.
5. **Know who did what.** Every change records whether it came from the app, the CLI, an agent token or the system, and why.
6. **Fit the desktop.** Quiver follows the Omarchy theme, works with a controller next to hyprpad, and puts games and mod profiles into Steam and the app launcher instead of building its own big-picture mode.

## Information architecture

```
Top bar      Library · Downloads · Activity   [Search / Ctrl K / Start]   quiverd status · Notifications · Settings · Accounts
Library
  Home       Continue playing hero · Needs attention · Collections · Grid or list
             Games and pinned mod profiles are both tiles
  Game       List on the left (pinned profiles indented under their game), detail on the right
             Overview · Launch options · Mods · Saves & progress · Add-ons · Files · Logs
Downloads    Active job (with “when it finishes” exports) · Up next · Recent · Storage · Download rules
Activity     Event log · Pending approvals · Connected clients
Settings     Accounts · Library and storage · Downloads · Compatibility · Launch defaults · Cloud saves ·
             Mods · Appearance · Notifications · Keyboard and controller · Automation and CLI · Diagnostics · About
Flows        First run: sign in → 2FA → import existing games → library folder → Proton check
             Add account · Sign in again (session expired) · Import · Verify / Move / Uninstall dialogs
             Delete profile (with export clean-up)
Overlays     Command palette (Ctrl K or Start) · Profile menu · Now playing card · Desktop notifications
Exports      Steam shortcuts and .desktop entries for games and pinned profiles
```

- **Deep links.** Each view has an address, for example `quiver open origins --tab launch`. Notifications and CLI output can point into the app.
- **Keyboard.** Ctrl K palette, `/` filter, `j`/`k` or arrows to move, Enter to open, `P` to play, Esc to go back, `g l` / `g d` / `g a` to jump to Library, Downloads and Activity, Ctrl , for Settings.
- **Controller.** The same app, with spatial focus and button hints. See [Controller](#controller-and-hyprpad).
- **One game, one place.** Everything about a game lives under its tabs. Downloads and Activity link back to the game.

## Feature priorities

### MVP: the new app on the daemon

| Area | What ships |
|---|---|
| Library | Grid with Ubisoft box art, list view, list + detail layout, install-state badges, filter, sort (recent, name, series order, size), smart collections (Installed, Favorites) |
| Accounts | Several Ubisoft accounts, merged ownership with the owning account shown per game, one live session per account, account switcher |
| Install | Install, update, verify, move, uninstall; queue with pause, resume, reorder; storage view per library folder |
| Exports | Steam shortcut and .desktop entry for each game, offered when an install finishes and available later from the game page |
| Sign in | 2FA (app code, email code, backup code), trusted device, keyring, session-expired flow per account |
| Play | Split Play button, sessions with exit status, local play time, now-playing card, stop game |
| Launch options | Proton build, sync primitive, wrappers (gamescope, MangoHud, GameMode), environment, extra arguments, resolved command, global and per-game scopes with inherited values |
| History | Revisions with actor and reason, diff, restore, prefix snapshot before a Proton change (automatic on btrfs/xfs) |
| Saves | Slot table with sync state, conflict resolution, backups list and restore |
| Add-ons | Owned and not-owned add-ons; “View in Ubisoft Store” link for ones you don’t own |
| Automation | Activity log with actors, copy-as-CLI everywhere, `--json` on every CLI command |
| Look and input | Omarchy theme with live switching, full keyboard navigation, controller navigation with button hints, command palette |
| Import | Lutris and Ubisoft Connect prefixes, plain folders, matched against manifests |

### Next

| Area | What ships |
|---|---|
| Mods | Overlay layers, profiles, conflict view, own saves per modded profile, profile launch overrides |
| Pinned profiles | Profile tiles in the library and list, their own .desktop entry and Steam shortcut, stable ids, clean-up on delete |
| Art | SteamGridDB and your own images per game or pinned profile, also copied into Steam’s grid folder for exported shortcuts |
| History | Test launch, “launch once with…”, automatic known-good tags, “restore last known good” |
| Agents | Scoped tokens, the approval policy, approvals in Activity and as notifications, MCP server (`quiver mcp`) |
| Progress | Ubisoft play time as the headline, achievements and player stats from UbiServices |
| Library | User collections, franchise collections from `sort_string`, favorites |
| System | Desktop notifications, update window, GE-Proton downloader, log viewer, diagnostics bundle export |

### Later

| Area | What ships |
|---|---|
| Platform | Ubisoft Connect rewards and Club actions, friends |
| Mods | Nexus Mods integration and update checks |
| Tools | Bisect launch options: try revisions automatically to find the change that broke a game |

There is no separate big-picture mode. The normal app works with a controller, and anything you want on a TV goes into Steam as a shortcut.

## Where each screen's data comes from

“Platform” means a Ubisoft service the core already talks to or can talk to. “Local” means `quiverd` state on this machine. “Not yet” means a platform source exists but is not wired. “Third party” is a service that is neither Ubisoft nor Quiver.

| Screen | Data | Source | Notes |
|---|---|---|---|
| Library | Owned games, product ids, add-on associations | Platform | demux ownership, per account, merged; 23 installable games, many with 30 to 66 associated SKUs |
| Library | Box art, hero, logo | Platform | product config `thumb_image`, `background_image`, `logo_image`, sometimes a localization key such as `l2` |
| Library | Custom art | Third party | SteamGridDB, or image files you choose |
| Library | Franchise grouping and series order | Platform | product config `sort_string`, for example “Assassin's Creed 16” |
| Library | Install state, size, update available | Platform | manifests compared with local install records |
| Library | Pinned profiles, collections, favorites | Local | `quiverd` |
| Game | Executables, arguments, working directory, Denuvo flag, cloud-save flag, game options tool, links | Platform | product config |
| Game | Installed version, manifest, size | Local | install record and manifest |
| Game | Add-on names | Platform | each add-on’s own product config; not fetched in this dump |
| Game | Store links for add-ons you don’t own | Platform | Ubisoft web store; no per-product URL in configs yet, so a search link for now |
| Game | Developer, release date, description | Not yet | not in product configs; left out |
| Game | Sessions and crash codes | Local | `quiverd` |
| Game | Steam shortcut and .desktop state | Local | `shortcuts.vdf`, `~/.local/share/applications` |
| Launch options | Proton builds | Local | Steam `compatibilitytools.d` plus builds Quiver downloads |
| Launch options | Revisions, outcomes, prefix snapshots | Local | `quiverd` config store |
| Mods | Layers, profiles, conflicts, exports | Local | `quiverd` mod store |
| Mods | Base version, owned add-ons | Platform | manifest and ownership |
| Saves | Slots, names, sizes, cloud times | Platform | cloudsave_service V2, per account |
| Saves | Backups | Local | `~/.local/share/ubisoft-linux` |
| Saves | Play time total | Not yet | Ubisoft play-session API; the headline once wired |
| Saves | Weekly chart, sessions | Local | `quiverd` |
| Saves | Achievements | Not yet | UbiServices; product config names the achievements set |
| Saves | Player stats | Not yet | stat ids are in product configs (`TimePlayed`, `TotalProgression`, `PlayerLevel`, …); values from UbiServices |
| Downloads | Job progress, phases, speeds | Local | installer events |
| Downloads | Disk usage | Local | statvfs and install records |
| Settings | All settings | Local | `quiverd` config store |
| Settings | Accounts and their sessions | Platform | UbiServices and demux, one live session per account |
| Settings | Theme | Local | Omarchy `colors.toml` |
| Settings | GE-Proton releases | Third party | GitHub releases |
| Sign in | Login, 2FA, trusted device | Platform | UbiServices sessions |
| Sign in | Session storage | Local | Secret Service keyring |
| Activity | Events, actors, diffs, approvals | Local | append-only event log |
| Import | Product match, version check | Platform | executable names in product configs, manifests |
| Import | Found installs, prefixes, saves | Local | filesystem scan |
| Now playing | Bridge, Denuvo token, ticket refresh | Local | `ubi-bridge` IPC and the broker |

## Accounts

- **One library, several accounts.** Each signed-in account contributes its ownership. A game owned by more than one account appears once, with an “Owned by” line naming every owner.
- **Launch account.** A game launches with the default account unless you pick another for it (Settings › Accounts › Who owns what, or `quiver config set <game> account=family`). Cloud saves belong to an account, so switching a game’s launch account shows that account’s slots, and Quiver warns before the first launch on the new account.
- **One live session per account.** Ubisoft allows one live demux login per account, so `quiverd` holds at most one per account. Accounts whose games are not running stay signed in (ticket in the keyring) and connect when needed.
- **Signed in elsewhere.** If Ubisoft Connect on another machine logs in with the same account, Ubisoft ends Quiver’s session. Quiver marks the account “signed in elsewhere”, pauses that account’s downloads and sends a notification. It does not reconnect on its own, because two clients reconnecting would keep logging each other out.
- **Badges.** In the grid, a game owned only by a non-default account carries that account’s tag. The library has an account filter.
- **CLI.** `quiver account list | add | remove | default NAME`, and `--account NAME` on `play`, `install` and `saves`.

## Pinned profiles, Steam and .desktop exports

### Pinning

- A mod profile can be **pinned**. A pinned profile gets its own tile in the library grid (“AC Odyssey · Visual overhaul”), its own row under its game in the side list, its own art, its own .desktop entry and its own Steam shortcut.
- Pinning, the .desktop entry and the Steam shortcut are three separate switches in the profile menu, each with its status (Pinned, Created, “Restart Steam”).
- Games themselves can be exported the same way, from the game page at any time or from the “when it finishes” options of an install.

### Stable ids

- Every profile gets an id when it is created, for example `p7f3k2q`. It is random, never derived from the name, and never changes. Vanilla has the fixed id `vanilla`.
- Every export is keyed by `quiver:<product id>/<profile id>`, for example `quiver:5059/p7f3k2q`, or `quiver:5059` for the game itself.
- **.desktop entry:** `~/.local/share/applications/quiver-5059-p7f3k2q.desktop`, with `Exec=quiver play 5059 --profile-id p7f3k2q`, `Name=AC Odyssey · Visual overhaul`, the game’s `icon_image` as the icon, and `X-Quiver-Id=quiver:5059/p7f3k2q`.
- **Steam shortcut:** an entry in `shortcuts.vdf` for the most recent Steam user, launching `quiver play … --profile-id …` with no compatibility tool (Quiver runs Proton itself). Quiver writes an explicit `appid` derived from the export key, `crc32("quiver:5059/p7f3k2q") | 0x80000000`, instead of letting it follow the name. Art is copied into Steam’s `grid/` folder under that appid.
- **Renames** rewrite the name in both exports. File names, appids and Steam artwork stay the same.
- Quiver records each export (target, path, appid, time written, content hash). If one is deleted or edited outside Quiver, the profile menu shows it as missing or changed and offers to recreate it.

### Steam needs a restart

Steam reads `shortcuts.vdf` when it starts and writes it back when it exits. Changes made while Steam is running would be lost. So Quiver writes the file only when Steam is not running. If Steam is running, the change waits and the UI says “Restart Steam to show it”. Restarting Steam from that message closes Steam cleanly, writes the file and starts Steam again.

### Deleting

Deleting a pinned profile opens a dialog listing what goes with it, all ticked except saves:

- the library pin
- the .desktop entry
- the Steam shortcut (waits for a Steam restart if Steam is running)
- the profile’s own saves, which are backed up first if you tick them

Uninstalling a game offers the same clean-up for the game’s exports and those of its profiles.

## Controller and hyprpad

- **Spatial focus.** D-pad and left stick move focus to the nearest focusable control in that direction. Lists and grids scroll to keep focus visible. Focus is drawn with the `focus` token: a 2 px ring with a 3 px gap and a soft halo.
- **Buttons.** A selects or plays. B goes back. X opens play options (profile, launch once with…). Y opens the menu or details. LB and RB switch between Library, Downloads and Activity. LT and RT switch tabs. Start opens the command palette. The right stick scrolls.
- **Hints.** When the last input came from a gamepad, controls show their button glyph and a hint bar appears at the bottom. The next mouse or keyboard input hides them. Glyphs follow the device (A/B/X/Y letters for Steam Controller and Xbox layouts, shapes for PlayStation pads). Settings › Appearance can force hints on or off.
- **Never the Guide button.** hyprpad (`~/code/hyprsc`) owns guide-button chords for the desktop and keeps Steam Input working, so Quiver ignores Guide entirely and never grabs input devices exclusively.
- **Two input paths.** If hyprpad has a mode for Quiver’s window that forwards the pad, Quiver reads it as a gamepad. If not, hyprpad’s desktop bindings already turn the D-pad into arrow keys and A into Enter, and Quiver’s keyboard navigation handles them. Both paths have to work; which one is the default is an open question.
- **Games launched by Quiver.** hyprpad’s `game` mode recognises Steam games by window class. Wine windows started by Quiver have the executable’s name as their class (for example `acorigins.exe`). Quiver can generate a hyprpad rule listing the executables of installed games, taken from the product configs, so guide chords and forwarding behave the same as for Steam games.
- **No big-picture mode.** The app itself is controller-friendly, and for a TV session games and profiles go into Steam.

## Design system for GPUI

The mock-ups use only things GPUI can draw: flex layout, rounded rectangles, borders, linear gradients, shadows, images with cover fit, text, and simple opacity animations. Two effects need a different approach in GPUI. Both are noted below.

### Color tokens

The palette below is the built-in fallback, “Quiver dark”. Normally the tokens come from the Omarchy theme (next section). Neutrals lean slightly blue so they sit well under warm and cool art. Semantic colors are separate from the accent. The four actor colors appear wherever a change is attributed and never follow the theme.

| Token | Value | Use |
|---|---|---|
| `ink-0` | `#0a0c10` | Top bar, side list, deepest surfaces |
| `ink-1` | `#0f1218` | App background |
| `ink-2` | `#141821` | Cards and panels |
| `ink-3` | `#1a1f2a` | Raised rows, selected inspector |
| `ink-4` | `#222836` | Buttons, inputs, badges |
| `ink-5` | `#2b3243` | Hover, segmented “on” |
| `line-1` / `line-2` | `#1f2531` / `#2c3343` | Dividers / control borders |
| `tx-1` … `tx-4` | `#eceff5` `#a8afbf` `#727a8d` `#4f5667` | Primary, secondary, muted, disabled text |
| `acc` / `acc-hi` | `#7b93ff` / `#a6b5ff` | Accent, accent text on dark |
| `sel` | `#222c4d` | Selected rows |
| `focus` | `#a6b5ff` | Focus ring for keyboard and controller |
| `ok` / `warn` / `bad` | `#4fc48e` / `#f0b44c` / `#f06272` | In sync, needs attention, failed |
| `who-gui` / `who-cli` / `who-agent` / `who-sys` | `#7b93ff` / `#4cc9c0` / `#c38bff` / `#9aa3b5` | App, CLI, agent, system |

Soft fills are the same hue at 12 to 16% alpha. Text on the accent is chosen by contrast.

### Omarchy theme

- **Source.** `~/.local/state/omarchy/current/theme/colors.toml`. Quiver watches `~/.local/state/omarchy/current/` with inotify and re-reads the palette whenever `theme.generation` changes, which happens on every theme switch. The new theme applies to every window at once, with a short crossfade. Settings › Appearance can switch to the built-in palette instead.
- **Two schemas.** Current themes have named keys (`accent`, `selection`, `muted`, `background`, `dark_background`, `darker_background`, `lighter_background`, `foreground`, `dark_foreground`, `light_foreground`, `bright_foreground`, the ANSI names and `mode`). Older ones, such as black-flag-resynced, have `color0`…`color15`, `selection_background` and no `mode`. Quiver maps the old names onto the new ones, infers `mode` from the background’s luminance, and derives any missing background steps by mixing.
- **Mapping.**

| Token | Dark themes | Light themes |
|---|---|---|
| `ink-0` | `dark_background` | `darker_background` |
| `ink-1` | `background` | `dark_background` |
| `ink-2` | 35% from `background` toward `lighter_background` | `background` |
| `ink-3` | 65% toward `lighter_background` | halfway to `darker_background` |
| `ink-4` | `lighter_background` | `darker_background` |
| `ink-5` | `lighter_background` + 10% `foreground` | `lighter_background` |
| `line-1` / `line-2` | `ink-2` + 8% / 15% `foreground` | `ink-2` + 10% / 20% `foreground` |
| `tx-1` | `foreground` | `foreground` |
| `tx-2` | `foreground` 30% toward `ink-1`, at least 7:1 | same |
| `tx-3` | `muted`, at least 4.5:1 | same |
| `tx-4` | `foreground` 62% toward `ink-1` (disabled, no minimum) | same |
| `acc` | `accent` | `accent` |
| `acc-hi` | `accent` + 20% white, at least 4.5:1 | `accent`, darkened to 4.5:1 if needed |
| `acc-ink` | white or `darker_background`, whichever contrasts more with `accent` | same |
| `sel` | `selection`, or a 30% tint of it if text on it would fall below 4.5:1 | same |
| `focus` | `accent` + 25% white | `accent` |
| `ok` / `warn` / `bad` | `green` / `yellow` / `red`, at least 3:1 | same |
| actor colors | fixed set, lightened to 4.5:1 if needed | darker fixed set, darkened to 4.5:1 if needed |

- **Contrast.** “At least N:1” means Quiver moves the color toward white (dark themes) or black (light themes) until it reaches that contrast against `ink-2`. The mock-ups run this exact mapping on the real jade-horse, black-flag-resynced and meta-braille-light files. Black-flag’s brass selection fails against its text, so its selected rows become a 30% brass tint.
- **Light themes.** App background is `dark_background` and cards are `background`, so cards stay visible on a white theme. Shadows are lighter. Text and controls over game art keep light text on a dark scrim in every theme (an “on-art” sub-theme), because the art is dark. Heroes fade into `ink-1` at the bottom. The diff view and status pills use the status tokens rather than fixed pastels.

### Type scale

- **UI:** Geist, 13 px base, like Zed's UI. **Data and commands:** Geist Mono. **Display:** Archivo SemiExpanded ExtraBold, only for page titles and the hero game title.
- GPUI does not expose variable-font axes, so ship static TTF instances (for example Archivo SemiExpanded 700/800) and register them with `cx.text_system().add_fonts`.
- Use tabular numbers (`tnum`) for sizes, times and percentages.

| Role | Size / weight | Example |
|---|---|---|
| Display | 40 / 800, Archivo | Hero game title |
| Page title | 22 / 700, Archivo | “Downloads”, “Activity” |
| Section | 20 / 700 | Settings section titles |
| Card title | 13.5 / 600–650 | “Sessions”, “History” |
| Body | 13 / 400–550 | Rows, controls |
| Small | 12 / 400 | Secondary lines |
| Caption | 11.5 / 400 | Meta under rows |
| Label | 10.5 / 600, uppercase, +0.09em | “NEEDS ATTENTION”, table headers |
| Mono | 12 and 11.5 / 400–600 | Paths, keys, commands, revision ids |

### Spacing, radius and layout

- 4 px grid. Common steps: 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40.
- Radius: 4 (checkbox, kbd), 6 (small buttons, tags), 8 (buttons, inputs), 10 (art tiles, list rows), 12 (cards, menus), 14–16 (floating panels), full (chips, pills, glyphs).
- Fixed sizes: top bar 52, side list 288, history panel 380, full hero 312, slim hero 112, Play button 46 tall (36 in the slim hero), art tile 240:280 (Ubisoft's thumb ratio), hint bar 44, page gutters 28 inside the detail pane and 40 on full-width pages.
- Shadows only on floating things: Play button glow, menus, palette, now-playing card, pending-changes bar.

### Effects in GPUI

- **No backdrop blur.** The mock-ups' frosted panels sit over art. In GPUI, make them a translucent fill over a pre-blurred copy of the art. The art cache produces a 96 px blurred “ambient” image per game when it downloads art, and the detail pane tints itself with it.
- **No CSS transforms.** Tile hover is a 2 px accent ring and a stronger shadow instead of a lift. Controller focus uses the `focus` ring.
- **Color mixing** happens once, when the theme loads, into a plain `Theme` struct of `Hsla` values. Components only read tokens.
- **Lists** use `uniform_list` (game list, activity log, file lists). The grid is a wrapped flex of fixed-size tiles, virtualized by row once a library passes a few hundred games.
- **Motion:** 120 ms for hover and press, 200 ms for panels, 320 ms crossfade when the hero changes, 160 ms for a theme change. Honor the system reduced-motion setting.

### Components

| Component | Notes |
|---|---|
| TopBar, NavTab | Active tab gets a 2 px accent underline. Downloads shows overall progress, Activity shows pending approvals. Account avatars stack on the right |
| PlayButton | Split. The main part plays the default profile. The arrow lists profiles, “launch once with…”, and “Vanilla” |
| Button | Primary, secondary, ghost, danger; 26 / 32 / 44 px |
| Chip, Pill, Tag | Chip is a filter or setting summary. Pill is a status (ok, warn, bad, accent, muted, not-yet dashed). Tag sits on art with a dark translucent fill |
| ActorBadge | Icon and name in the actor color: “You · app”, “You · CLI”, agent token name, “System” |
| ArtTile, ProfileTile, GameRow | Tile: art, state tags, title, one meta line. Profile tile: its own art, the game logo and the profile name. Row: 22×26 art, name, right-edge status; profile rows indented under their game |
| Tabs, Segmented, Toggle, Checkbox, Field, Select | Standard controls. Every focusable control draws the `focus` ring |
| Glyph, HintBar | Round glyph for face buttons, rounded square for bumpers, triggers and Start. Shown only after gamepad input |
| Menu | Profile menu and other popovers; items can carry a status pill |
| SettingRow | Label, help text, control, inherited-from marker, changed dot. Hover shows the config key and “Copy as CLI” |
| Card, KeyValue, Table | Cards only where grouping helps. Tables use 10.5 px uppercase headers and no zebra stripes |
| Timeline | Revision history and sessions. Node color shows the outcome |
| DiffView | Key-level diff with status-colored rows, header naming the two revisions |
| CommandLine | Mono line with `$`, the exact `quiver` command and a copy button |
| Charts | Bar (weekly play time), area (throughput), ring (achievements). Drawn with GPUI paths |
| Banner, Toast, Palette, Dialog | Banner for approvals and warnings in place, toast for results, palette for commands |

### Art

- **Default:** Ubisoft art from the product config.
  - `thumb_image` (240×280): grid tiles and list icons.
  - `background_image` (1200×700 to 1920×1080): heroes. Some have the logo painted in (Origins, Trials Rising). Keep a small override list so Quiver does not draw the logo on top.
  - `logo_image` (transparent PNG): drawn over heroes and slim banners. Trim transparent edges once in the cache.
  - `splash_image` (700×330): optional wide cards later.
  - `icon_image` (.ico): desktop entries.
  - Values can be localization keys (`l2`, `THUMBIMAGE`), resolved through `localizations.default`.
- **Custom:** per game or pinned profile, each slot (tile, hero, logo) can come from SteamGridDB (with your API key) or from an image file. A reset puts the Ubisoft art back. Exported Steam shortcuts get the same art.
- **Cache:** `~/.cache/quiver/art`, with the derived ambient image and dominant color.

## Settings history and restore

### How it works

- **Scopes resolve in order:** global defaults, then the game, then the mod profile, then one-off `--with` values at launch. The app shows which scope each value comes from.
- **Every save of a scope creates a revision** (`r1`, `r2`, …) per scope. Revisions are immutable and record actor, time, optional message and optional reason.
- **Restore creates a new revision** with the old content (r15 = content of r13). History is never rewritten, so a restore can itself be undone.
- **Every launch records the revisions it used** and the hash of the resolved config, plus duration and exit status. That links a crash to the change that preceded it, and lets Quiver tag a revision “known good” after a clean session of 10 minutes or more.
- **Proton changes take a prefix snapshot first,** because a new Proton build upgrades the prefix and an older build may not run in it again. On btrfs and xfs the snapshot is a reflink copy that costs almost nothing, so it is automatic. On other filesystems (ext4) it is a real 1–3 GB copy, so it is off until you turn it on in Settings › Compatibility.
- **Edits are staged in the app.** Save writes a revision. Test launch runs the staged config once without writing one. The CLI writes immediately, or previews with `--dry-run`.

### Data model

```rust
/// Flat dotted keys keep diffs and the CLI simple:
/// runtime.proton, runtime.sync, wrappers.gamescope.enabled, wrappers.gamescope.args,
/// env.DXVK_HDR, game.executable, game.extra_args, account, mods.layers, saves.namespace
struct ConfigDoc { scope: Scope, values: BTreeMap<Key, Value> }

enum Scope { Global, Game(ProductId), Profile(ProductId, ProfileId) }

struct Revision {
    scope: Scope,
    id: u32,                       // r14
    parent: Option<u32>,
    at: Timestamp,
    actor: Actor,
    message: Option<String>,       // "Turn on gamescope with HDR"
    reason: Option<String>,        // agent-supplied --reason
    changes: Vec<Change>,          // key, old, new
    restored_from: Option<u32>,
    prefix_snapshot: Option<SnapshotId>,
    tags: Vec<String>,             // "known-good", user tags
}

enum Actor {
    App { user: String },
    Cli { user: String, pid: u32, terminal: Option<String> },
    Agent { token: TokenId, name: String },
    System { why: SystemCause },   // auto-update, import, scheduled sync
}

struct Session {
    id: SessionId, game: ProductId, profile: Option<ProfileId>, account: AccountId,
    revisions: Vec<(Scope, u32)>, resolved_hash: Hash,
    started: Timestamp, ended: Option<Timestamp>, exit: ExitStatus,
}

struct PrefixSnapshot {
    id: SnapshotId, game: ProductId, at: Timestamp,
    method: Reflink | Copy, path: PathBuf, bytes_on_disk: u64,
    proton_before: String, proton_after: String,
}
```

State lives in one SQLite database next to today's `config.json` and `games.json`, which it replaces after a one-time migration. The daemon validates keys against a typed schema (enum, bool, string, path, list), so the app, the CLI and agents get the same errors.

### Play time

Once Ubisoft’s play-session data is wired, its total is the headline number everywhere (library, game page, saves tab). Local sessions stay as the detail: the weekly chart, per-session outcomes and the link to config revisions. Until then the local total is shown.

## Mod overlays

### How it works

- **A profile is a named, ordered stack of layers** on top of the base install. Vanilla always exists, runs the base install directly, and is never mounted.
- **Layers** are archives or folders that Quiver unpacks into its own store (`~/.local/share/ubisoft-linux/mods/<game>/<layer>`), with a manifest of every file's path, size and hash. Conflicts are computed from manifests before launch.
- **Mounting.** User namespaces are enabled and unprivileged overlayfs works on this machine (verified). At launch, quiverd’s launcher process calls `unshare(CLONE_NEWUSER | CLONE_NEWNS)` and writes a uid map that maps your uid to itself, not to root. While it still holds the namespace’s capabilities it calls `mount(2)` directly for the overlay over the install path: base (lower, read-only), layers bottom to top, and a per-profile writable upper directory. Then it execs Proton. Calling the syscall from the launcher, rather than using `unshare -r` or a shell wrapper, keeps the real uid, and Wine refuses a prefix that is not owned by the current uid. Because the overlay is mounted over the install path inside the namespace, registry paths and the working directory do not change. Each game has its own prefix, so its wineserver starts inside the namespace too. fuse-overlayfs remains a fallback for machines without unprivileged overlayfs.
- **Layers can target the prefix** as well as the game folder, for configs under `Documents` or `AppData`.
- **Profiles are a config scope,** so layer order and toggles are revisioned with the same history and restore as launch options. A profile can also override launch options. ReShade's layer declares `WINEDLLOVERRIDES=dxgi=n,b`, and only that profile gets it.
- **Saves.** Every modded profile has its own saves, kept apart from Vanilla’s by overlaying the prefix’s save directory with a per-profile one. Cloud sync for a modded profile is off by default, so a broken mod cannot overwrite your cloud saves, and each profile has a toggle to turn it on.
- **Pinning and exports** are covered in [Pinned profiles](#pinned-profiles-steam-and-desktop-exports).
- **Updates** apply to the base install as before. Layers that declare a `built_for` version older than the base get a warning. The upper layer can be inspected and reset.
- **Verify** checks only the base install, so it never reports mod files as corruption.

### Data model

```rust
struct Layer {
    id: LayerId, game: ProductId, name: String, version: Option<String>,
    source: LayerSource,          // Archive { path, sha256 } | Folder { path } | Url (later)
    target: LayerTarget,          // GameDir | Prefix(PathBuf)
    manifest: Vec<FileEntry>,     // path, size, hash
    built_for: Option<String>,    // game version
    requires: Vec<(Key, Value)>,  // config the layer needs, e.g. env.WINEDLLOVERRIDES
    added: (Timestamp, Actor),
}

struct Profile {
    id: ProfileId,                // stable, random, e.g. "p7f3k2q"; "vanilla" is fixed
    game: ProductId, name: String,
    layers: Vec<(LayerId, bool)>, // bottom → top, enabled
    saves: SaveNamespace,         // Shared (Vanilla only) | Own
    cloud_sync: bool,             // false by default when saves = Own
    pinned: bool,
    art: ArtOverrides,            // per-slot: Ubisoft | SteamGridDb(id) | File(path)
    upper_dir: PathBuf,
}

struct Export {
    key: String,                  // "quiver:5059/p7f3k2q"
    target: ExportTarget,         // Desktop | Steam { user_id }
    path: PathBuf, steam_appid: Option<u32>,
    written_at: Option<Timestamp>, pending_steam_restart: bool, hash: Hash,
}
```

## CLI that mirrors the app

Rules for every command:

- `quiver <noun> <verb>`. Games can be named by product id or fuzzy name (`3539`, `origins`). Profiles by name or id (`--profile visual-overhaul`, `--profile-id p7f3k2q`).
- `--json` on everything. Stable exit codes. `--dry-run` wherever something changes.
- `--reason "…"` is recorded in Activity. Agents should always pass it.
- If an action needs approval, the command returns `pending` with an event id. `quiver wait <event>` blocks until you decide in the app.

| In the app | Command |
|---|---|
| Library grid, filters | `quiver library [--installed] [--collection NAME] [--account NAME] [--sort recent]` |
| Game overview | `quiver show origins` |
| Play, Play with profile, launch once | `quiver play origins [--profile NAME \| --vanilla] [--with runtime.proton=GE-Proton10-26] [--account NAME]` |
| Stop game | `quiver stop origins` |
| Install, update, verify, move, uninstall | `quiver install [--steam] [--desktop]`, `quiver update`, `quiver verify`, `quiver move <game> <dir>`, `quiver uninstall` |
| Downloads queue | `quiver jobs [watch \| pause \| resume \| cancel \| move JOB --to 1]` |
| Launch options | `quiver config get origins [KEY] [--resolved]`, `quiver config set origins KEY=VALUE… [--message] [--reason]`, `quiver config unset origins KEY` |
| History, diff, restore, tag | `quiver config history origins`, `quiver config diff origins r12 r14`, `quiver config restore origins r13 [--with-prefix]`, `quiver config tag origins r13 known-good` |
| Prefix snapshots | `quiver prefix snapshot origins`, `quiver prefix snapshots origins`, `quiver prefix restore origins SNAP` |
| Mods | `quiver mods list odyssey`, `quiver mods add odyssey ./reshade.zip --name ReShade`, `quiver mods enable/disable/move/remove …`, `quiver mods conflicts odyssey --profile visual-overhaul` |
| Mod profiles | `quiver mods profile list/create/rename/duplicate/delete odyssey …`, `quiver mods profile pin odyssey visual-overhaul` |
| Steam and .desktop | `quiver export steam odyssey[/visual-overhaul]`, `quiver export desktop …`, `quiver export list`, `quiver export remove …` |
| Art | `quiver art set odyssey/visual-overhaul tile --steamgriddb 4512` or `--file ~/Pictures/odyssey.png`, `quiver art reset odyssey` |
| Saves | `quiver saves list origins`, `quiver saves sync origins`, `quiver saves pull/push`, `quiver saves backup origins --name "before Siwa"`, `quiver saves restore origins BACKUP` |
| Proton builds | `quiver runtime list`, `quiver runtime install GE-Proton10-26`, `quiver runtime remove …` |
| Activity, undo, approvals | `quiver activity [--actor agent] [--game origins] [--since 1d]`, `quiver undo EVENT`, `quiver approve EVENT`, `quiver deny EVENT` |
| Accounts | `quiver login [--account NAME]`, `quiver logout NAME`, `quiver account list \| add \| remove \| default NAME` |
| Tokens, MCP | `quiver token create --name claude-code --scopes …`, `quiver token revoke …`, `quiver mcp` |
| Import | `quiver import scan [--from lutris \| steam \| DIR]`, `quiver import apply PLAN` |
| Theme | `quiver theme show` (prints the resolved tokens and their contrast) |
| Daemon | `quiver daemon status \| start \| stop` |

An agent handling “reconfigure AC Origins to use Proton X”:

```sh
quiver runtime list --json                    # is X installed?
quiver config set origins runtime.proton=GE-Proton10-26 \
  --reason "User asked to switch AC Origins to GE-Proton10-26"
# quiverd snapshots the prefix (automatic on btrfs/xfs), writes r15 and logs the event
# under the agent's token. The app shows it in Activity with an Undo button.
```

### What agents may do

Confirmed policy, editable in Settings › Automation and CLI:

| Action | CLI (you) | Agents |
|---|---|---|
| Read library and status | Allow | Allow |
| Change launch options, mod profiles, exports | Allow | Allow |
| Launch and stop games | Allow | Allow |
| Install and update | Allow | Allow |
| Restore saves or settings | Allow | Ask |
| Uninstall, delete prefixes, profiles or saves | Allow | Ask |
| Sign in, sign out, add or remove accounts | Ask | Deny |

The command palette, `quiver --help` and the MCP tool list all come from one command registry in the daemon, so they cannot drift apart.

## Added beyond the brief

- **Needs attention** on the home page: updates, installs, save conflicts and agent requests in one place.
- **Sessions tied to revisions,** so “it worked yesterday” has an answer.
- **Test launch** and **launch once with…** to try a change without saving it.
- **Automatic known-good tags** and a “restore last known good” button.
- **Prefix snapshots** before Proton changes and updates.
- **Export records with drift detection,** so a deleted .desktop file or Steam shortcut shows up as missing instead of silently breaking.
- **A generated hyprpad rule** for the games Quiver launches.
- **`quiver theme show`** to debug a palette’s contrast from the terminal.
- **Copy as CLI** on settings, the palette and every Activity entry.
- **Import that checks against manifests,** so adopted installs are verified, not trusted.

## Decisions (from review, 2026-11-16)

Each decision is written into the sections above; this list is the record.

- **Name:** pick an original product name with no Ubisoft trademarks (no Ubi/Uplay/Connect, and no
  Assassin's Creed terms such as Animus, Abstergo or Helix). Decided: **Quiver** (the `ubi`
  CLI becomes `quiver`).
- **Agent permissions:** as proposed. See [What agents may do](#what-agents-may-do).
- **Mod overlays:** unprivileged overlayfs in a user namespace, verified on this machine, mounted with the syscall to keep the real uid. See [Mod overlays](#mod-overlays).
- **Mod profiles are first-class launch targets:** pinned tiles, their own .desktop entry and Steam shortcut. See [Pinned profiles](#pinned-profiles-steam-and-desktop-exports).
- **Modded saves:** own saves per modded profile, cloud sync off by default, per-profile toggle.
- **Store:** add-ons you don't own are listed with a “View in Ubisoft Store” link.
- **Steam shortcuts:** offered at install time and from the game page, for games and pinned profiles. Steam needs a restart to show them. No separate big-picture mode.
- **Controller-friendly:** everywhere, without the Guide button, alongside hyprpad. See [Controller and hyprpad](#controller-and-hyprpad).
- **Custom art:** SteamGridDB and your own images per game or profile; Ubisoft art stays the default.
- **Icons:** unchanged from the original proposal (box-art thumbnails in lists; the game's
  `icon_image` for desktop entries).
- **Multiple Ubisoft accounts:** supported now. See [Accounts](#accounts).
- **Theme:** inherit Omarchy's theme and restyle on `theme.generation`. See [Omarchy theme](#omarchy-theme).
- **Prefix snapshots:** automatic on btrfs/xfs, opt-in elsewhere.
- **Play time:** Ubisoft's total is the headline once wired; local sessions are the detail.

## Open questions

1. **Restarting Steam.** Shortcuts appear only after Steam restarts. May Quiver restart Steam itself when no game is running, or only tell you?
2. **Controller input path.** Should hyprpad forward the pad to Quiver’s window (Quiver reads a gamepad), or should Quiver rely on hyprpad’s desktop bindings (arrow keys, Enter)? And should Quiver generate a hyprpad `game` rule from the executables of installed games?
3. **Which account launches a game both accounts own.** Always the default account, or the account whose cloud saves are newest? Saves are per account, so this decides which progress you continue.
4. **Signed in elsewhere.** Quiver will not reconnect an account that was logged in from another machine, to avoid two clients logging each other out. Is a notification and a manual “Sign in again” enough?
5. **SteamGridDB.** It needs a personal API key. Is asking for one in Settings › Appearance acceptable, and should custom art also be pushed into Steam for exported shortcuts by default?
6. **Store links.** Product configs have no store URL, so the first version opens a store search for the add-on’s name. Is that good enough until a better source turns up?
7. **Steam user.** On a machine with several Steam users, Quiver writes shortcuts for the most recent one. Should it ask instead?
