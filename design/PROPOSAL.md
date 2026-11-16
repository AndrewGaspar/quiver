# ubi launcher: design proposal

A front end for the `ubid` daemon that replaces Ubisoft Connect on Linux. This document goes with the mock-ups (the "Mock-ups" tab of the same page). Game art in the mock-ups is the real Ubisoft art for this library. Play times, sizes, sessions, achievements, mods and agent activity are illustrative.

## Principles

1. **The daemon is the product.** The app, the `ubi` CLI and agents are peers. Every button in the app is one daemon command, and the app can show that command. If the app can do it, a script can do it.
2. **Art when browsing, density when configuring.** The library is image-first. Launch options, mods and saves are dense, keyboard-friendly tools.
3. **Every change can be undone.** Launch options are versioned, Proton changes snapshot the prefix, saves are backed up before every sync, and mods never touch the base install.
4. **Show where data comes from.** Show platform data where it exists, and mark what is not wired yet instead of inventing it. Developer, release date and description are left out until there is a source for them.
5. **Know who did what.** Every change records whether it came from the app, the CLI, an agent token or the system, and why.

## Information architecture

```
Top bar      Library · Downloads · Activity   [Search / Ctrl K]   ubid status · Notifications · Settings · Account
Library
  Home       Continue playing hero · Needs attention · Collections · Grid or list
  Game       List on the left, detail on the right
             Overview · Launch options · Mods · Saves & progress · Add-ons · Files · Logs
Downloads    Active job · Up next (reorderable) · Recent · Storage · Download rules
Activity     Event log · Pending approvals · Connected clients
Settings     Account · Library and storage · Downloads · Compatibility · Launch defaults · Cloud saves ·
             Mods · Appearance · Notifications · Keyboard and controller · Automation and CLI · Diagnostics · About
Flows        First run: sign in → 2FA → import existing games → library folder → Proton check
             Sign in again (session expired) · Import · Verify / Move / Uninstall dialogs
Overlays     Command palette (Ctrl K) · Now playing card · Desktop notifications
Later        Big picture mode for controller and Steam Deck
```

- **Deep links.** Each view has an address, for example `ubi open origins --tab launch`. Notifications and CLI output can point into the app.
- **Keyboard.** Ctrl K palette, `/` filter, `j`/`k` or arrows to move, Enter to open, `P` to play, Esc to go back, `g l` / `g d` / `g a` to jump to Library, Downloads and Activity, Ctrl , for Settings.
- **One game, one place.** Everything about a game lives under its tabs. Downloads and Activity link back to the game.

## Feature priorities

### MVP: the new app on the daemon

| Area | What ships |
|---|---|
| Library | Grid with Ubisoft box art, list view, list + detail layout, install-state badges, filter, sort (recent, name, series order, size), smart collections (Installed, Favorites) |
| Install | Install, update, verify, move, uninstall; queue with pause, resume, reorder; storage view per library folder |
| Account | Sign in with 2FA (app code, email code, backup code), trusted device, keyring, session-expired flow |
| Play | Split Play button, sessions with exit status, local play time, now-playing card, stop game |
| Launch options | Proton build, sync primitive, wrappers (gamescope, MangoHud, GameMode), environment, extra arguments, resolved command, global and per-game scopes with inherited values |
| History | Revisions with actor and reason, diff, restore, prefix snapshot before a Proton change |
| Saves | Slot table with sync state, conflict resolution, backups list and restore |
| Automation | Activity log with actors, copy-as-CLI everywhere, `--json` on every CLI command |
| Navigation | Command palette, full keyboard navigation |
| Import | Lutris and Ubisoft Connect prefixes, plain folders, matched against manifests |

### Next

| Area | What ships |
|---|---|
| Mods | Overlay layers, profiles, conflict view, per-profile saves, profile launch overrides |
| History | Test launch (run staged changes once), “launch once with…”, automatic known-good tags, “restore last known good” |
| Agents | Scoped tokens, approval policy, approvals in Activity and as notifications, MCP server (`ubi mcp`) |
| Progress | Achievements and player stats from UbiServices, Ubisoft play time |
| Library | User collections, franchise collections from `sort_string`, favorites |
| System | Desktop notifications, update window, GE-Proton downloader, log viewer, diagnostics bundle export |

### Later

| Area | What ships |
|---|---|
| Big picture | Controller-first layout for Steam Deck and TV, or export games as Steam shortcuts |
| Platform | Ubisoft Connect rewards and Club actions, friends |
| Store | Links for add-ons you do not own, opening the web store. No in-app store |
| Mods | Nexus Mods integration and update checks |
| Accounts | More than one Ubisoft account |
| Tools | Bisect launch options: try revisions automatically to find the change that broke a game |

## Where each screen's data comes from

“Platform” means a Ubisoft service the core already talks to or can talk to. “Local” means `ubid` state on this machine. “Not yet” means a platform source exists but is not wired.

| Screen | Data | Source | Notes |
|---|---|---|---|
| Library | Owned games, product ids, add-on associations | Platform | demux ownership; 23 installable games, many with 30 to 66 associated SKUs |
| Library | Box art, hero, logo | Platform | product config `thumb_image`, `background_image`, `logo_image`, sometimes a localization key such as `l2` |
| Library | Franchise grouping and series order | Platform | product config `sort_string`, for example “Assassin's Creed 16” |
| Library | Install state, size, update available | Platform | manifests compared with local install records |
| Library | Last played, play time | Local | session tracking in `ubid` |
| Library | Ubisoft play history | Not yet | play-session API |
| Game | Executables, arguments, working directory, Denuvo flag, cloud-save flag, game options tool, links | Platform | product config |
| Game | Installed version, manifest, size | Local | install record and manifest |
| Game | Add-on names, descriptions, prices | Not yet | needs store metadata; only ids today |
| Game | Developer, release date, description | Not yet | not in product configs; left out |
| Game | Sessions and crash codes | Local | `ubid` |
| Launch options | Proton builds | Local | Steam `compatibilitytools.d` plus builds ubi downloads |
| Launch options | Revisions, outcomes, prefix snapshots | Local | `ubid` config store |
| Mods | Layers, profiles, conflicts | Local | `ubid` mod store |
| Mods | Base version, owned add-ons | Platform | manifest and ownership |
| Saves | Slots, names, sizes, cloud times | Platform | cloudsave_service V2 |
| Saves | Backups | Local | `~/.local/share/ubisoft-linux` |
| Saves | Achievements | Not yet | UbiServices; product config names the achievements set |
| Saves | Player stats | Not yet | stat ids are in product configs (`TimePlayed`, `TotalProgression`, `PlayerLevel`, …); values from UbiServices |
| Downloads | Job progress, phases, speeds | Local | installer events |
| Downloads | Disk usage | Local | statvfs and install records |
| Settings | All settings | Local | `ubid` config store |
| Settings | GE-Proton releases | Third party | GitHub releases |
| Sign in | Login, 2FA, trusted device | Platform | UbiServices sessions |
| Sign in | Session storage | Local | Secret Service keyring |
| Activity | Events, actors, diffs, approvals | Local | append-only event log |
| Import | Product match, version check | Platform | executable names in product configs, manifests |
| Import | Found installs, prefixes, saves | Local | filesystem scan |
| Now playing | Bridge, Denuvo token, ticket refresh | Local | `ubi-bridge` IPC and the broker |

## Design system for GPUI

The mock-ups use only things GPUI can draw: flex layout, rounded rectangles, borders, linear gradients, shadows, images with cover fit, text, and simple opacity animations. Two effects need a different approach in GPUI. Both are noted below.

### Color tokens

One dark theme to start. Neutrals lean slightly blue so they sit well under warm and cool art. The accent is a soft periwinkle, used for selection, focus, progress and the Play button. Semantic colors are separate from the accent. The four actor colors appear wherever a change is attributed.

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
| `ok` / `warn` / `bad` | `#4fc48e` / `#f0b44c` / `#f06272` | In sync, needs attention, failed |
| `who-gui` / `who-cli` / `who-agent` / `who-sys` | `#7b93ff` / `#4cc9c0` / `#c38bff` / `#9aa3b5` | App, CLI, agent, system |

Soft fills are the same hue at 12 to 14% alpha. Text on the accent uses `#0a0e22`.

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
- Radius: 4 (checkbox, kbd), 6 (small buttons, tags), 8 (buttons, inputs), 10 (art tiles, list rows), 12 (cards), 14–16 (floating panels), full (chips, pills).
- Fixed sizes: top bar 52, side list 288, history panel 380, full hero 312, slim hero 112, Play button 46 tall (36 in the slim hero), art tile 240:280 (Ubisoft's thumb ratio), page gutters 28 inside the detail pane and 40 on full-width pages.
- Shadows only on floating things: Play button glow, popovers, palette, now-playing card, pending-changes bar.

### Effects in GPUI

- **No backdrop blur.** The mock-ups' frosted panels sit over art. In GPUI, make them a translucent fill over a pre-blurred copy of the art. The art cache produces a 96 px blurred “ambient” image per game when it downloads art, and the detail pane tints itself with it.
- **No CSS transforms.** Tile hover is a 2 px accent ring and a stronger shadow instead of a lift.
- **Lists** use `uniform_list` (game list, activity log, file lists). The grid is a wrapped flex of fixed-size tiles, virtualized by row once a library passes a few hundred games.
- **Motion:** 120 ms for hover and press, 200 ms for panels, 320 ms crossfade when the hero changes. Honor the system reduced-motion setting.

### Components

| Component | Notes |
|---|---|
| TopBar, NavTab | Active tab gets a 2 px accent underline. Downloads shows overall progress, Activity shows pending approvals |
| PlayButton | Split. The main part plays the default profile. The arrow lists profiles, “launch once with…”, and “Vanilla” |
| Button | Primary, secondary, ghost, danger; 26 / 32 / 44 px |
| Chip, Pill, Tag | Chip is a filter or setting summary. Pill is a status (ok, warn, bad, accent, muted, not-yet dashed). Tag sits on art with a dark translucent fill |
| ActorBadge | Icon and name in the actor color: “You · app”, “You · CLI”, agent token name, “System” |
| ArtTile, GameRow | Tile: art, state tags, title, one meta line. Row: 22×26 art, name, right-edge status |
| Tabs, Segmented, Toggle, Checkbox, Field, Select | Standard controls. Every control has a visible 2 px accent focus ring |
| SettingRow | Label, help text, control, inherited-from marker, changed dot. Hover shows the config key and “Copy as CLI” |
| Card, KeyValue, Table | Cards only where grouping helps. Tables use 10.5 px uppercase headers and no zebra stripes |
| Timeline | Revision history and sessions. Node color shows the outcome |
| DiffView | Key-level diff with green and red rows, header naming the two revisions |
| CommandLine | Mono line with `$`, the exact `ubi` command and a copy button |
| Charts | Bar (weekly play time), area (throughput), ring (achievements). Drawn with GPUI paths |
| Banner, Toast, Palette, Dialog | Banner for approvals and warnings in place, toast for results, palette for commands |

### Art pipeline

- `thumb_image` (240×280): grid tiles and list icons.
- `background_image` (1200×700 to 1920×1080): heroes. Some have the logo painted in (Origins, Trials Rising). Keep a small override list so ubi does not draw the logo on top.
- `logo_image` (transparent PNG): drawn over heroes and slim banners. Trim transparent edges once in the cache.
- `splash_image` (700×330): optional wide cards later.
- `icon_image` (.ico): desktop entries.
- Values can be localization keys (`l2`, `THUMBIMAGE`), resolved through `localizations.default`.
- Files come from the Ubisoft static asset host and are cached under `~/.cache/ubi/art` with the derived ambient image and dominant color.

## Settings history and restore

### How it works

- **Scopes resolve in order:** global defaults, then the game, then the mod profile, then one-off `--with` values at launch. The app shows which scope each value comes from.
- **Every save of a scope creates a revision** (`r1`, `r2`, …) per scope. Revisions are immutable and record actor, time, optional message and optional reason.
- **Restore creates a new revision** with the old content (r15 = content of r13). History is never rewritten, so a restore can itself be undone.
- **Every launch records the revisions it used** and the hash of the resolved config, plus duration and exit status. That links a crash to the change that preceded it, and lets ubi tag a revision “known good” after a clean session of 10 minutes or more.
- **Proton changes take a prefix snapshot first,** because a new Proton build upgrades the prefix and an older build may not run in it again. On btrfs or xfs this is a reflink copy and costs almost nothing. On ext4 it is a real copy (1–3 GB), so the app says so and lets you turn it off.
- **Edits are staged in the app.** Save writes a revision. Test launch runs the staged config once without writing one. The CLI writes immediately, or stages with `--dry-run` to preview the resolved command.

### Data model

```rust
/// Flat dotted keys keep diffs and the CLI simple:
/// runtime.proton, runtime.sync, wrappers.gamescope.enabled, wrappers.gamescope.args,
/// env.DXVK_HDR, game.executable, game.extra_args, mods.layers, saves.namespace
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
    id: SessionId, game: ProductId, profile: Option<ProfileId>,
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

## Mod overlays

### How it works

- **A profile is a named, ordered stack of layers** on top of the base install. Vanilla always exists, runs the base install directly, and is never mounted.
- **Layers** are archives or folders that ubi unpacks into its own store (`~/.local/share/ubisoft-linux/mods/<game>/<layer>`), with a manifest of every file's path, size and hash. Conflicts are computed from manifests before launch.
- **At launch** ubid starts the game in a private mount namespace and mounts an overlay over the install path: base (lower, read-only), then layers bottom to top, then a per-profile writable upper directory. Unprivileged overlayfs in a user namespace (kernel 5.11+) is preferred, with fuse-overlayfs as the fallback. Mounting over the install path inside the namespace means registry paths and the working directory do not change. Each game already has its own prefix, so its wineserver starts inside the namespace too.
- **Layers can target the prefix** as well as the game folder, for configs under `Documents` or `AppData`.
- **Profiles are a config scope,** so layer order and toggles are revisioned with the same history and restore as launch options. A profile can also override launch options. ReShade's layer declares `WINEDLLOVERRIDES=dxgi=n,b`, and only that profile gets it.
- **Saves:** a profile uses shared saves or its own. Modded profiles get their own by default, and cloud sync is off for them, so a broken mod cannot overwrite your cloud saves.
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
    id: ProfileId, game: ProductId, name: String,
    layers: Vec<(LayerId, bool)>, // bottom → top, enabled
    saves: SaveNamespace,         // Shared | Own
    upper_dir: PathBuf,
}
```

## CLI that mirrors the app

Rules for every command:

- `ubi <noun> <verb>`. Games can be named by product id or fuzzy name (`3539`, `origins`).
- `--json` on everything. Stable exit codes. `--dry-run` wherever something changes.
- `--reason "…"` is recorded in Activity. Agents should always pass it.
- If an action needs approval, the command returns `pending` with an event id. `ubi wait <event>` blocks until you decide in the app.

| In the app | Command |
|---|---|
| Library grid, filters | `ubi library [--installed] [--collection NAME] [--sort recent]` |
| Game overview | `ubi show origins` |
| Play, Play with profile, launch once | `ubi play origins [--profile NAME \| --vanilla] [--with runtime.proton=GE-Proton10-26]` |
| Stop game | `ubi stop origins` |
| Install, update, verify, move, uninstall | `ubi install`, `ubi update`, `ubi verify`, `ubi move <game> <dir>`, `ubi uninstall` |
| Downloads queue | `ubi jobs [watch \| pause \| resume \| cancel \| move JOB --to 1]` |
| Launch options | `ubi config get origins [KEY] [--resolved]`, `ubi config set origins KEY=VALUE… [--message] [--reason]`, `ubi config unset origins KEY` |
| History, diff, restore, tag | `ubi config history origins`, `ubi config diff origins r12 r14`, `ubi config restore origins r13 [--with-prefix]`, `ubi config tag origins r13 known-good` |
| Prefix snapshots | `ubi prefix snapshot origins`, `ubi prefix snapshots origins`, `ubi prefix restore origins SNAP` |
| Mods | `ubi mods list odyssey`, `ubi mods add odyssey ./reshade.zip --name ReShade`, `ubi mods enable/disable/move/remove …`, `ubi mods conflicts odyssey --profile visual-overhaul` |
| Mod profiles | `ubi mods profile list/create/edit/delete/default odyssey …` |
| Saves | `ubi saves list origins`, `ubi saves sync origins`, `ubi saves pull/push`, `ubi saves backup origins --name "before Siwa"`, `ubi saves restore origins BACKUP` |
| Proton builds | `ubi runtime list`, `ubi runtime install GE-Proton10-26`, `ubi runtime remove …` |
| Activity, undo, approvals | `ubi activity [--actor agent] [--game origins] [--since 1d]`, `ubi undo EVENT`, `ubi approve EVENT`, `ubi deny EVENT` |
| Account | `ubi login`, `ubi logout`, `ubi whoami` |
| Tokens, MCP | `ubi token create --name claude-code --scopes …`, `ubi token revoke …`, `ubi mcp` |
| Import | `ubi import scan [--from lutris \| steam \| DIR]`, `ubi import apply PLAN` |
| Daemon | `ubi daemon status \| start \| stop` |

An agent handling “reconfigure AC Origins to use Proton X”:

```sh
ubi runtime list --json                       # is X installed?
ubi config set origins runtime.proton=GE-Proton10-26 \
  --reason "User asked to switch AC Origins to GE-Proton10-26"
# ubid snapshots the prefix, writes r15, logs the event under the agent's token.
# The app shows it in Activity with an Undo button.
```

The command palette, `ubi --help` and the MCP tool list all come from one command registry in the daemon, so they cannot drift apart.

## Added beyond the brief

- **Needs attention** on the home page: updates, installs, save conflicts and agent requests in one place.
- **Sessions tied to revisions,** so “it worked yesterday” has an answer.
- **Test launch** and **launch once with…** to try a change without saving it.
- **Automatic known-good tags** and a “restore last known good” button.
- **Prefix snapshots** before Proton changes and updates.
- **Separate saves for modded profiles,** kept out of the cloud by default.
- **Agent approvals** with a per-action policy, plus an MCP server.
- **Copy as CLI** on settings, the palette and every Activity entry.
- **Import that checks against manifests,** so adopted installs are verified, not trusted.
- **Storage view** and an update window, so big patches do not land mid-session.

## Open questions

1. **Name.** The mock-ups say “ubi” to match the CLI. Keep it, or pick a product name?
2. **What agents may do without asking.** Proposed: agents may change launch options, install and launch freely; restoring saves, uninstalling and deleting need approval; account changes are denied. Is that right?
3. **Overlay mechanism.** Unprivileged overlayfs in a user namespace, mounted over the install path, with fuse-overlayfs as fallback. Is a user namespace acceptable on your systems, or should ubi avoid namespaces and use a merged directory elsewhere?
4. **Prefix snapshots on ext4.** They are full copies (1–3 GB each). Automatic with a retention limit, opt-in, or only when the library is on btrfs or xfs?
5. **Cloud saves for modded profiles.** Off by default, with their own saves. Agree, or should modded play also sync?
6. **Store.** Hide add-ons you do not own, or list them with a link to the web store?
7. **Play time.** When Ubisoft's play history is wired, should it replace local tracking, or be shown next to it?
8. **Big picture.** A controller layout in ubi, or export games as Steam shortcuts and let Steam's big picture mode do it?
9. **Custom art.** Ubisoft art only, or allow SteamGridDB and your own images per game?
10. **More than one Ubisoft account,** now or never?
