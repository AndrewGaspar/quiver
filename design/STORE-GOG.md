# GOG.com as a Quiver store (scoping notes)

Status: scoping and design only (2026-11-16). Nothing here is built. This is one of five
sibling documents. STORE-STEAM, STORE-EPIC, STORE-EA and STORE-META (the Meta VR store) cover
the other stores, and STORES.md reconciles the store-agnostic core. This document proposes only
the interfaces GOG needs (section 3.2); where STORES.md differs, it wins.

The user's ask: "scope out integrating GOG.com… I want all the same great capabilities we
built for Ubisoft Connect."

How this was researched:

- Quiver's code and design docs, read on this branch.
- Heroic's GOG store manager, read only: `/home/ajg/code/HeroicGamesLauncher`, commit
  `7e7d7883` from 2026-08-15, `src/backend/storeManagers/gog` and its helpers.
- The source and READMEs of comet and heroic-gogdl.
- gogapidocs, the lgogdownloader and Minigalaxy READMEs, and news coverage.

Rules followed:

- Nothing logged in to GOG or to any other service.
- No credential files were read. Heroic's `gog_store/auth.json` doesn't exist on this machine
  anyway; only `installed.json`'s structure was read, and it lists no GOG installs.
- Lutris' `pga.db` was opened read-only and has no GOG games.

So **nothing below was exercised against a live GOG account.** Behaviour of the live
endpoints comes from the open-source clients that use them every day, and claims marked
**(unverified)** need a spike.

---

## 1. Summary and recommendation

GOG is the easiest store Quiver could add, and in places it can do *more* than Quiver does
for Ubisoft today:

- **The games are DRM-free.** Launching needs no ticket, no Denuvo token and no ownership
  proof: just the exe, arguments and working directory from `goggame-<id>.info`. A
  launch-time license broker isn't needed.
- **The Galaxy SDK talks to a host-side TCP service, not to pipes inside the prefix.** Games
  that use Galaxy features (achievements, stats, leaderboards, play time) load the
  `Galaxy64.dll` they ship with and connect to the communication service on
  `127.0.0.1:9977`. Wine sockets are host sockets, so a *native Linux* process can answer them.
  That is the same reason Quiver's broker works on `127.0.0.1:47810`.
- **comet already is that service, in Rust.** It reports achievements, stats and leaderboards
  to the real GOG backend with the user's own token. Its author is imLinguin, a Heroic
  maintainer. It is Apache-2.0, at v0.4.1 (2026-10-28), with active development and Heroic
  using it by default since 2.15.0. It is Quiver's `ubi-bridge` analogue, already written.
- **Content system v2 is a clean, documented, chunked format.** It uses zlib JSON manifests
  and MD5-checked chunks under signed CDN links, and maps well onto Quiver's slice-based
  queue, verify and repair.
- **The platform APIs fill gaps that Ubisoft leaves open.** gamesdb has developer, release
  date, description and genres, and `gameplay.gog.com` has per-user achievement *unlock
  state* and play time. Today Quiver shows Ubisoft achievement definitions only
  (`Achievements.unlocked_known = false`), and the bridge logs earned achievements without
  reporting them (`ubi-bridge/src/win/api.rs`).

**Recommendation:**

1. **Native Rust for the store client.** That covers sign-in, library, metadata and art, the
   v2 and v1 downloader, verify, repair, updates and cloud saves, built in a new `gog-core`
   crate on Quiver's existing queue, keyring, prefix and launcher machinery. Use gogdl and
   Heroic as behavioural references and test oracles only. They are GPL-3.0 and Python or
   TypeScript, and their process model (a CLI per operation, stdout progress) doesn't fit
   quiverd's per-slice queue.
2. **Reuse comet for the Galaxy SDK service, as a supervised sidecar first.** Build it from a
   pinned tag and ship it as `lib/quiver/quiver-comet`; quiverd starts it when a GOG game
   starts and stops it afterwards. Hand it tokens without putting them on its command line
   (section 4.4). Contribute a library entry point and a token-provider hook upstream, then
   move it **in-process** when that lands, so quiverd stays the only holder of GOG tokens and
   can route games of different accounts. Don't port it: it would be a large job for no gain.
3. **Add a tiny Windows "GalaxyCommunication" dummy service**, registered in each GOG prefix.
   Some SDK builds want it (5 of the 36 titles in comet's compatibility table). Build it next
   to `ubi-bridge` for `x86_64-pc-windows-gnu`.
4. **Sight replaces Galaxy's overlay** and doesn't inject GOG's proprietary one. Achievement
   unlocks reach Sight as toasts through comet's `achievement_unlocked` custom event, and
   later through an in-process hook.
5. **MVP scope:**
   - sign-in (paste the redirect URL)
   - library with art
   - install, verify, update and uninstall of Windows v2 builds
   - a per-game prefix with GOG redistributables
   - launch from `goggame-*.info`
   - achievements, stats and play time via comet
   - play time posted to GOG
   - CLI parity

   Cloud saves, DLC management, imports, v1 builds and Sight integration follow in phase 2.

The biggest risks:

- GOG's private APIs can change without notice. One change in mid-2026 broke Minigalaxy's
  library.
- There is no loopback redirect for login, so sign-in is a copy-paste or a web view.
- Cloud-save semantics (folder trees, conflicts) differ from Ubisoft's slot files, so
  `savestores` must generalise.
- GOG is "actively exploring" an official Galaxy for Linux, which could later overlap with
  all of this.

---

## 2. Capability matrix

### 2.1 What "the same capabilities" means (checked against the code)

These are the capabilities Quiver has for Ubisoft today. The status column shows whether each
is built, design only, or not built.

| # | Capability | Ubisoft status | Where (verified) |
|---|---|---|---|
| C1 | Accounts: several accounts, a default, a per-game launch account, keyring sessions, password + 2FA, "signed in elsewhere" | built | `ubi-core/src/{auth,store,online}.rs`, `Request::{Login,Login2fa,Logout,AccountDefault}` |
| C2 | Library sync: ownership, product configs, art (tile, hero, logo, icon), game info, add-ons owned and not owned with store links | built | `library.rs`, `gameinfo.rs`, `spaces.rs`, `exports/art.rs`, `Request::{Library,GameInfo,Ownership}` |
| C3 | Install, update and repair queue: preview, free-space check, pause, resume, reorder, bandwidth limit, auto-update, per-slice resume, retries | built | `builds.rs`, `install.rs`, `ubid/src/downloads.rs`, `Request::{Install,InstallPreview,Queue*,Update,UpdatePreview,DownloadSettings}` |
| C4 | Verify and repair (hash every slice, re-fetch only bad slices, never touch non-manifest files) | built | `verify.rs`, `Request::{Verify,VerifyResult}` |
| C5 | Uninstall (optionally the prefix), import of existing installs (Lutris, Connect, reflink) | built | `launcher.rs::{uninstall,import_game}`, `Request::{Uninstall,Import,ScanInstalls}` |
| C6 | Move an install | **not built** (PROPOSAL only) | no `Move` request exists |
| C7 | Per-game Proton prefix, Proton choice, launch settings with revisioned history | built | `launcher.rs::ensure_prefix`, `settings.rs`, `Request::Settings*` |
| C8 | Launch with licensing: bridge in the prefix (SDK pipes, `InitReq` with ticket, owned ids, CD keys), Denuvo broker, play-session lease, sessions, stop | built | `ubi-bridge`, `broker.rs`, `launch.rs`, `ubi-ipc` |
| C9 | Cloud saves: pull before and push after, backups, restore, save stores (several per game, one synced), merge | built | `cloudsave.rs`, `savesync.rs`, `saves.rs`, `savestores.rs` |
| C10 | Achievements and stats: *read* definitions, stats, challenges and rewards. Unlock state unknown; bridge earns logged, not reported | partly built | `progress.rs`, `ubi-bridge/src/win/api.rs:403` |
| C11 | Play time: report to the platform (start, heartbeat, end), read the platform total, local sessions | built | `playsession.rs`, `Request::{Playtime,RemotePlaytime}` |
| C12 | DLC: owned add-on ids served to the game; add-on listing | built | `LaunchContext.owned_uplay_ids`, `info.rs` |
| C13 | Mods v2: packages, versions, profiles, overlayfs, config patches, built-in catalog, fixes on vanilla | built | `mods.rs`, `modversions.rs`, `modpkg.rs`, `overlay.rs`, `confpatch.rs` |
| C14 | Mod Options (MOD-CONFIG) | design only | `design/MOD-CONFIG.md` |
| C15 | Sight overlay and HUD | design only | `design/SIGHT.md`, `design/SIGHT-HUD.md` |
| C16 | Exports: `.desktop` entries and Steam shortcuts, keyed `quiver:<product>[/<profile>]` | built | `exports/*` |
| C17 | Steam copies (`steam-run`) | built | `steamrun.rs`, `exports/steamgames.rs` (no GOG analogue; see 2.2) |
| C18 | CLI and agent automation: every action is a daemon request, `--source`, activity log, `confirmed` for agent-sensitive actions | built | `ubi-api`, `ubi-cli` |
| C19 | First-run UX: Sign in, Storage, Import, Steam, Controller, Fixes, Theme | built | `ubi-app/src/ui/welcome.rs` (`Step::ALL`) |

### 2.2 GOG approach per capability

Effort: **S** is days, **M** is one to three weeks, **L** is longer. Risk is technical and
policy risk combined.

| # | Capability | GOG approach | Prior art | Effort | Risk |
|---|---|---|---|---|---|
| C1 | Account and auth | OAuth authorization code with the Galaxy client id. Login runs in GOG's web page (it may show a CAPTCHA), which redirects to `embed.gog.com/on_login_success?…&code=`. The user pastes that URL. Quiver exchanges the code at `auth.gog.com/token`, keeps the refresh token in the keyring, refreshes the roughly one-hour access token, and derives game-scoped tokens per `clientId` (section 4) | gogdl `auth.py`, Heroic `user.ts` + WebView, lgogdownloader `--browser-login`, GogDownloader | M | Med: no loopback redirect, and the paste step is clunky; multiple accounts and session semantics (unverified) |
| C2 | Library sync, art, metadata | Owned entitlements from `galaxy-library.gog.com/users/{uid}/releases` (paged; keep `platform_id == "gog"`), cross-checked with `embed.gog.com/user/data/games` (owned ids, including DLC). Metadata and art from `gamesdb.gog.com/platforms/gog/external_releases/{id}`: title, summary, developers, genres, vertical cover, background, logo, square icon, release date, supported OSes. Product facts and DLC lists from `api.gog.com/products/{id}?expand=…` and `api.gog.com/v2/games/{id}` | Heroic `library.ts::refresh,getGamesdbData,gogToUnifiedInfo` | M | Med: GOG changed an API in mid-2026 and broke Minigalaxy's library ([GamingOnLinux](https://www.gamingonlinux.com/2026/07/simple-gog-client-for-linux-minigalaxy-version-1-4-2-is-out-now/)). The tile art aspect ratio differs from Ubisoft's 240:280 |
| C3 | Install and update | Content system **v2**: list builds at `content-system.gog.com/products/{id}/os/windows/builds?generation=2`. The build's `link` is a zlib JSON *meta* manifest with depots per language and product, `dependencies`, `products[]` (base and DLC), `installDirectory`, `clientId`, `clientSecret` and `scriptInterpreter`. Depot manifests list files as chunks (`compressedMd5`, `md5`, sizes), and chunks live at `…/store/xx/yy/<hash>` behind `…/secure_link?_version=2&generation=2&path=/`. **v1** (older builds): `repository.json`, files as byte ranges of a `main.bin`, whole-file MD5, no compression. Updates by chunk diff (reuse chunks already present); xdelta3 patches (`/patches/store`) later. Redistributables come from `dependencies/repository?generation=2` and `open_link…/dependencies/store/`. Free-space check, pause, resume, priorities and bandwidth limit reuse quiverd's queue | gogdl `dl/managers/{v1,v2,dependencies}.py`, `dl_utils.py`; gogapidocs; lgogdownloader `--galaxy-install` | L | Med: v1 and odd builds; per-product DLC depots; support files; the post-install script interpreter (C7) |
| C4 | Verify and repair | Per-chunk MD5 for v2, whole-file MD5 for v1, against the stored manifest; repair fetches only the bad chunks. Same rule as Ubisoft: only manifest files are read or written | gogdl `repair` | S–M | Low |
| C5 | Uninstall and import | Uninstall: files plus an optional prefix, as today. Import: find `goggame-<id>.info` (and `.id` for `buildId`) in Heroic installs, Lutris GOG installs, a Galaxy install inside a Wine prefix (`…/GOG Galaxy/Games/…`, `HKLM\Software\WOW6432Node\GOG.com\Games\<id>`) and plain folders; match to the owned id; verify against the build's manifest before adopting it (reflink as today). Offline installers (`setup_*.exe`, Inno Setup) can be unpacked natively with `innoextract` (installed on this machine) | Heroic `importGame`, `readInfoFile` | M | Low–Med: matching a folder to the right build |
| C6 | Move | Store-agnostic: rename or reflink, then fix the paths in records. Galaxy's registry keys in the prefix, if written (C7), need rewriting. Build it once for all stores | Heroic `moveInstall` | S (generic) | Low |
| C7 | Per-game prefix and setup | Same `ensure_prefix`, but with GOG's setup instead of Ubisoft's SDK DLLs: (a) install the depot's redistributables (VC++, DirectX, and so on) once per prefix; (b) for v2 builds with `scriptInterpreter: true`, run GOG's `__redist/ISI/scriptinterpreter.exe /VERYSILENT /DIR=… /ProductId=… /galaxyclient …` per installed product (it writes the registry keys and files some games need); otherwise run any `temp_executable`; (c) register the GalaxyCommunication dummy service (`sc create GalaxyCommunication binpath=…`) | Heroic `setup.ts`, `launcher.ts` (dummy service), comet `dummy-service/` | M | Med: running GOG's own setup exes under Proton is the usual source of odd failures |
| C8 | Launch with licensing | DRM-free: read `goggame-<id>.info` `playTasks` (the `isPrimary` task: `path`, `arguments`, `workingDir`; `category` game, launcher or tool), run it under Proton in the game's prefix, with no ticket. **Galaxy-SDK features** go through the user's real account via comet on `127.0.0.1:9977` (section 3.3). Sessions, stop, systemd scope, Hyprland tags and `QUIVER_*` environment are unchanged | gogdl `launch.py`, Heroic `games.ts::launch` | S (DRM-free) + M (comet supervision) | Low for launch; Med for SDK coverage (comet's table: 27 working, 7 partial, 1 broken of 36) |
| C9 | Cloud saves | Locations from `remote-config.gog.com/components/galaxy_client/clients/{clientId}?component_version=2.0.45` (`content.Windows.cloudStorage.{enabled,locations[]}`, each a `name` plus a path with `<?INSTALL?>`, `<?SAVED_GAMES?>`, `<?DOCUMENTS?>`, `<?APPLICATION_DATA_LOCAL?>`, `_LOCAL_LOW`, `_ROAMING`). With no locations the default is `%LocalAppData%/GOG.com/Galaxy/Applications/{clientId}/Storage/Shared/Files`, the SDK's storage folder. Sync against `cloudstorage.gog.com/v1/{uid}/{clientId}/{location}/{path}`: GET lists (name, hash, last_modified), PUT uploads gzip with `Etag` = MD5 of the gzipped bytes and `X-Object-Meta-LocalLastModified`, DELETE removes. Uses the game-scoped token. Pull before and push after each session, with backups first, as for Ubisoft | gogdl `saves.py`, Heroic `save_sync.ts` | L | Med: folder trees rather than slot files, so `savestores` must generalise; conflict rules; per-profile stores need folder overlays |
| C10 | Achievements and stats | **Read:** `gameplay.gog.com/clients/{clientId}/users/{uid}/achievements` (definitions, unlock state and times, icons; `X-Gog-Lc` locale), plus stats, with a game-scoped token. **Unlock reporting:** done by comet when the game calls the SDK (`POST …/achievements/{key}` with `date_unlocked`), cached in SQLite for offline use. Leaderboards too. **Exceeds the Ubisoft state** (C10 there is definitions only) | comet `api/gog/{achievements,stats,leaderboards}.rs`; Heroic `getAchievements` | S (read) + reuse | Low–Med: the endpoints are private; Heroic calls this endpoint with the product id rather than a `clientId` (unverified which is right) |
| C11 | Play time | **Read:** `gameplay.gog.com/games/{id}/users/{uid}/sessions` → `time_sum`. **Report:** after the session, `POST` there with `{session_date, time}` in minutes, queued while offline. There is no heartbeat like Ubisoft's playsession API. Optional presence at `presence.gog.com/users/{uid}/status` (POST every 5 min while playing, DELETE at the end) | Heroic `postPlaytimeSession`, `syncQueuedPlaytime`, `presence.ts`; comet `GET_USER_TIME_PLAYED` | S | Low: a crash loses nothing, because the local session record posts on the next start |
| C12 | DLC | DLC are products in the v2 meta `products[]` with their own depots; ownership from `user/data/games`. Install and uninstall DLC means adding or removing its depots (`--dlcs` semantics in gogdl). The DLC list with store links comes from `api.gog.com/products/{id}?expand=expanded_dlcs`. The game sees DLC through files and the SDK (comet's `LIBRARY_INFO_REQUEST`) | gogdl v2 `get_dlcs_user_owns`, Heroic | M | Low |
| C13 | Mods and mod profiles | Store-agnostic once ids are namespaced: overlayfs over the install, profile prefix folders, config patches, versions. The built-in catalog moves to `mods/<store>/<id>/…`. GOG specials: REDmod deploy for Cyberpunk 2077 (Heroic special-cases it) can be a recipe | Heroic `getCyberpunkMods` | S (generic) | Low |
| C14 | Mod Options | Store-agnostic (MOD-CONFIG) | n/a | 0 | n/a |
| C15 | Sight overlay and HUD | Store-agnostic (Vulkan layer, `quiver-sight`). GOG parity: the SDK's overlay calls (open a web page, invite dialogs, visibility) are answered by comet's overlay service. Quiver should *not* inject GOG's own overlay (it is proprietary; comet downloads it and uses `galaxy-helper` to inject it). Unlock toasts come from comet's `[[custom_event]] achievement_unlocked` POST to a quiverd URL | comet `api/handlers/overlay_*.rs`, wiki "Overlay" | S–M | Med: how games behave when the overlay is reported unavailable (unverified per game) |
| C16 | Exports | Store-agnostic; the key becomes `quiver:gog:<id>[/<profile>]` (section 3.2). Art from gamesdb | n/a | S | Low |
| C17 | Steam copies | No GOG analogue: a game bought on Steam is not on the GOG account. Out of scope | n/a | n/a | n/a |
| C18 | CLI and agents | The same requests with a store-qualified game reference (`quiver install gog:witcher3`, or by fuzzy name across stores). The agent policy is unchanged: GOG sign-in and sign-out are "Deny" for agents | n/a | S | Low |
| C19 | First run | A "GOG" step: sign in (browser plus paste) or skip, then import (Heroic, Lutris, a Galaxy-in-Wine prefix, folders, offline installers). Storage, Fixes and Theme are shared | Heroic login WebView | S–M | Low |
| extra | Offline installers and extras (DRM-free archival) | `api.gog.com/products/{id}?expand=downloads` lists installers, patches and bonus content (manuals, soundtracks); `…/downlink/…` gives signed links. A GOG-only nicety, not parity | lgogdownloader | M | Low |
| extra | Native Linux builds | Not in the content system: Linux builds are MojoSetup `.sh` installers, which gogdl's Linux manager extracts from the zip by ranges. They wouldn't use the prefix or Proton | gogdl `dl/managers/linux.py`, Minigalaxy | M | Med: a different launch path and no Galaxy SDK in some Linux builds (comet's table notes several) |

---

## 3. Architecture

### 3.1 Where GOG sits

```
 quiver-app   quiver CLI   agents
        \        |        /
         ubi-api (JSON lines) — requests carry a GameRef { store, id }
                 |
              quiverd
   ├─ accounts (keyring)          ubisoft sessions | gog refresh tokens
   ├─ library (merged)            ubi-core::library | gog-core::library (galaxy-library + gamesdb)
   ├─ download queue              one queue; per-store ContentSource (Ubisoft slices | GOG chunks)
   ├─ launcher (prefix, Proton)   per-store LaunchPlan + RuntimeService
   │     ├─ Ubisoft: ubi-bridge.exe in the prefix (SDK pipes) + broker on :47810
   │     └─ GOG:     comet on the host :9977 (Galaxy SDK TCP) + dummy service in the prefix
   ├─ cloud saves                 per-store CloudSaves (Ubisoft slots | GOG folder trees)
   ├─ progress / play time        per-store Progress (UbiServices | gameplay.gog.com)
   └─ mods, settings, exports, Sight — store-agnostic once keyed by GameRef
```

New crates:

- **`gog-core`**: auth, the API client, library, content system v1/v2, install, verify,
  cloud saves, play time, achievements read, `goggame-*.info` and setup plans. It sits next
  to `ubi-core` and shares no Ubisoft code. Shared mechanics, such as the queue, prefix
  creation, Proton runner, overlay namespace, save backups and keyring, should move into a
  `quiver-core` (STORES.md section 5).
- **`gog-galaxy`** (thin): supervises comet, first as a child process (sidecar), later as an
  in-process library call. It also receives comet's custom events.
- **`gog-dummy-service`** (Windows, `x86_64-pc-windows-gnu`, like `ubi-bridge`): a
  service binary that registers with the SCM and idles. It can be a port of comet's
  `dummy-service/communication.c` (Apache-2.0) or a few lines of Rust with
  `windows-service`.

### 3.2 Store-agnostic interfaces GOG needs (brief; reconciled in STORES.md)

- **`GameRef { store: StoreId, id: String }`.** GOG ids are numeric (for example
  `1423049311`); Epic's are strings. Today everything is `product_id: u32`. Keep Ubisoft's
  on-disk paths and export keys as they are (`games/<id>/`, `quiver:<id>`), so existing
  Steam appids and records don't change. Namespace new stores (`games/gog-<id>/`,
  `mods/gog-<id>/`, `quiver:gog:<id>`; the export key uses `:` because `/` already
  separates the profile, see STORES.md section 2.2).
- **`AuthFlow`**: `Password { email, password } → 2FA` (Ubisoft) or `BrowserCode { url } →
  paste(redirect_url)` (GOG, Epic). The app and CLI render the flow from the store's kind.
- **`Library`**: `owned() → Vec<Entitlement { id, kind: Game|Dlc, parent }>`;
  `metadata(id) → GameMeta { title, art: {tile, hero, logo, icon}, developer, release,
  description, links, store_url }`.
- **`ContentSource`**:
  - `builds(id)`
  - `plan(id, build, langs, dlcs) → InstallPlan { files: [{path, size, flags, pieces:
    [{hash, compressed_hash, size, offset}]}], redists, support_files }`
  - `fetch(piece) → bytes`
  - `verify_piece`

  Quiver's queue, resume records, retries, free-space check and verify run on the plan
  rather than on Ubisoft manifests.
- **`LaunchPlan`**: exe, args, workdir, env, `prefix_setup: [steps]` (redists, script
  interpreter, services, registry), and `runtime: RuntimeService` (`InPrefixBridge` for
  Ubisoft, `HostService` for GOG via comet, `None`). Sessions, leases and markers stay
  generic.
- **`CloudSaves`**: `locations(id) → [SaveLocation { name, path_in_prefix }]`, `list`,
  `get`, `put`, `delete` with mtime and hash. Ubisoft is a single location whose entries
  are slot files. `savestores` stores become "a set of folders per location".
- **`Progress`**: `achievements`, `stats`, `playtime_total`, `report_session(start, end)`.
  Ubisoft reports with heartbeats; GOG reports after the session.

### 3.3 Galaxy SDK runtime: comet

What a Galaxy-SDK game does:

1. It loads its own `Galaxy64.dll` (shipped in the install; comet's debugging uses
   `GalaxyPeer.ini` next to the exe).
2. Some SDK builds then try to wake the `GalaxyCommunication` Windows service.
3. It connects to `127.0.0.1:9977`. comet binds exactly that address
   (`TcpListener::bind("127.0.0.1:9977")`, `src/main.rs`).
4. It sends protobuf messages (definitions reverse-engineered in
   [Yepoleb/gog_protocols](https://github.com/Yepoleb/gog_protocols)).
5. `AUTH_INFO_REQUEST` carries the game's `client_id` and `client_secret`. comet trades
   the user's Galaxy refresh token for a game-scoped token
   (`auth.gog.com/token?grant_type=refresh_token&without_new_session=1`).
6. It then serves the requests in the table below. The data lives on `gameplay.gog.com`,
   with a SQLite cache for offline use.

| Area | Requests comet handles |
|---|---|
| Library | `LIBRARY_INFO` |
| Achievements | `GET_USER_ACHIEVEMENTS`, `UNLOCK_USER_ACHIEVEMENT`, `CLEAR_USER_ACHIEVEMENT`, `DELETE_USER_ACHIEVEMENTS` |
| Stats | `GET_USER_STATS`, `UPDATE_USER_STAT`, `DELETE_USER_STATS` |
| Leaderboards | `GET_LEADERBOARDS` (also by key), entries global, around the user and for users; `SET_LEADERBOARD_SCORE`, `CREATE_LEADERBOARD` |
| Sessions and play time | `START_GAME_SESSION`, `GET_USER_TIME_PLAYED` |
| Overlay | the overlay service, client and peer handlers |

The table lists 18 request types; anything else gets `not_implemented`
(`src/api/handlers/communication_service.rs`). Not covered: global stats, and nothing
shown for lobbies, matchmaking or networking (P2P multiplayer).

How this differs from `ubi-bridge`:

- comet runs on the host. Nothing Quiver-owned has to run in the prefix except the dummy
  service.
- It serves every running game at once (one task per connection).
- The port is fixed, so there can be only one comet per host. One GOG account at a time can
  have a running game until in-process routing by `game_pid` exists (`AUTH_INFO_REQUEST`
  carries it).

**Reuse options for comet:**

| Option | How | For | Against |
|---|---|---|---|
| **Sidecar (MVP)** | Build comet at a pinned tag and install it as `lib/quiver/quiver-comet`. quiverd starts it when the first GOG game starts and stops it when the last one ends, in quiverd's cgroup; its logs go to the session log. Point `[[custom_event]]` at a quiverd endpoint | No code coupling; tracks upstream; deps stay separate (comet uses reqwest 0.13, rust-protobuf 3, sqlx/SQLite; Quiver uses reqwest 0.12 and prost 0.14); Apache-2.0 in its own binary | Token handoff (section 4.4); two token holders; no per-account routing; comet's own `config.toml` and data dirs need redirecting under Quiver's |
| **In-process crate (target)** | A git dependency on a pinned tag, or vendored under `third_party/comet` with its LICENSE. quiverd calls its handler entry point on its own listener with a token provider backed by Quiver's store | One token authority; per-connection account routing; unlock events as direct callbacks to Sight and the activity log; no extra binary | comet is a binary crate whose `lib.rs` isn't a stable API; a second HTTP and protobuf stack in quiverd; upstream churn |
| Port to `gog-core` | Reimplement the protocol | Full control | L effort, duplicates an actively maintained project, loses its compatibility work |

The recommendation is the sidecar now and in-process later, with an upstream PR adding
`comet::serve(listener, TokenProvider, Config)` and a token-from-fd or stdin option. This is
**steer 1**.

### 3.4 Data layout (proposal)

```
<data>/gog/manifests/<id>/<build>.json        v2 meta + depot manifests (for diff, verify, setup)
<data>/gog/cache/{library,gamesdb,products}/  API caches with timestamps
<data>/games/gog-<id>/history.jsonl           launch settings (as today, namespaced)
<data>/games/gog-<id>/saves.jsonl             save stores
<data>/mods/gog-<id>/…                        mods (as today, namespaced)
<install>/<installDirectory>/.quiver-install/ per-file / per-chunk resume records
<prefix>/…/GOG.com/…                          what GOG's setup and scriptinterpreter write
$XDG_RUNTIME_DIR/quiver/comet/               comet's runtime dir (sidecar), mode 0700
```

---

## 4. Auth details

### 4.1 Flow

1. Quiver opens the system browser at:

   ```
   https://auth.gog.com/auth?client_id=46899977096215655
     &redirect_uri=https%3A%2F%2Fembed.gog.com%2Fon_login_success%3Forigin%3Dclient
     &response_type=code&layout=galaxy
   ```

   Heroic uses `layout=galaxy`; gogapidocs documents `layout=client2`. The client id is
   the Galaxy client's, which Heroic, gogdl, Lutris and comet all use. Its secret is a
   published constant in the same sources.
2. The person signs in on GOG's own page. GOG's 2FA and CAPTCHA are handled there, and
   Quiver never sees the password.
3. GOG redirects to `embed.gog.com/on_login_success?origin=client&code=…`. The redirect URI
   is fixed for this client id, so **there is no loopback capture**. The person pastes the
   final URL, or just the code, into the app or into `quiver login --store gog`. This is
   what lgogdownloader's `--browser-login` and GogDownloader do.
4. Quiver exchanges the code: `GET https://auth.gog.com/token?client_id=…&client_secret=…
   &grant_type=authorization_code&code=…&redirect_uri=…`. The reply has `access_token`,
   `refresh_token`, `expires_in` (3600 in the docs), `user_id` and `session_id`.
5. Quiver reads `https://users.gog.com/users/{user_id}` (username, avatar) for the account
   list.

### 4.2 Storage and refresh

- The refresh token, plus the cached access token and its expiry, go in the Secret Service
  keyring as `gog:<user_id>` (STORES.md section 3.2), the same mechanism as `store.rs`'s `session:<userId>`.
  `accounts.json` gains a `store` field and the non-secret index.
- The access token is refreshed when it is within a few minutes of expiry, with
  `grant_type=refresh_token`. gogdl stores whatever new refresh token comes back. Quiver
  should do the same, under a lock, so only one refresh is ever in flight per account.
  Whether GOG rotates refresh tokens and invalidates the old one is **(unverified)**. The
  answer matters for the sidecar (section 4.4).
- **Game-scoped tokens:** for `clientId` and `clientSecret` (from the v2 build meta, or
  `clientId` from `goggame-<id>.info`), refresh against the main refresh token with that
  client's credentials and `without_new_session=1`. Cache them in memory only. They are
  used for cloud storage, achievements and stats reads, and by comet.
- **Sign out:** delete the keyring entry and caches. GOG has no documented revoke endpoint
  **(unverified)**.

### 4.3 Accounts

- Several GOG accounts fit the existing model (C1): merged ownership, a per-game launch
  account, and cloud saves following the launch account.
- Constraint: one comet on `:9977` per host. With the sidecar, games from two GOG accounts
  can't run at once. quiverd refuses the second with a clear reason. The in-process version
  can route by `game_pid` → session → account.
- Does a Quiver sign-in log out GOG Galaxy elsewhere? Heroic users run Galaxy on Windows and
  Heroic on Linux against the same account, so it appears not to (the Ubisoft
  "signed in elsewhere" problem doesn't seem to exist). **(unverified)**

### 4.4 Handing tokens to a comet sidecar

comet takes tokens as `--access-token`, `--refresh-token` and `--user-id` on argv, or
imports them from Heroic, Lutris or wyvern files. **Don't use argv:**
`/proc/<pid>/cmdline` is readable by every local user unless `hidepid` is set. In order of
preference:

1. **Upstream a `--tokens-fd N` or stdin option.** It's small; the maintainer is
   responsive and Heroic-adjacent.
2. **Interim:** comet's Heroic import reads
   `${XDG_CONFIG_PATH:-$HOME/.config}/heroic/gog_store/auth.json`. Note the non-standard
   variable name `XDG_CONFIG_PATH` (`src/import_parsers/heroic.rs`). quiverd can write a
   Heroic-format file (keyed by the Galaxy client id) with mode 0600 under
   `$XDG_RUNTIME_DIR/quiver/comet/` and start comet with `XDG_CONFIG_PATH` pointing there
   and `--from-heroic --username <name> --quit`. Then delete the file once comet has read
   it. comet never writes back. This must be checked in a spike.
3. **In-process** (section 3.3) removes the problem.

Notes:

- comet logs the game's `client_id` and `client_secret` at info level. These are per-game
  app credentials, not the user's, but Quiver should still run it at a log level that keeps
  them out of `quiverd.log`.
- `GalaxyPeer.log`, if a user enables it, can contain sensitive data.
- comet's long-session handling of the main access token after an hour is **(unverified)**.
  Game-scoped tokens are refreshed per game.

---

## 5. Phased plan

### Phase 0: spikes

Each spike needs the person's GOG sign-in, so ask first.

| Spike | Question | Done when |
|---|---|---|
| G1 | Login paste, token exchange, refresh, keyring; does a refresh rotate the refresh token? | `quiver login --store gog` round trip; a refresh after expiry works twice in a row |
| G2 | Library + gamesdb for the person's real library: counts, DLC, art present, ids beyond u32? | `quiver library --store gog` lists every owned game with art |
| G3 | v2 download of one small game, byte-identical to a gogdl install of the same build (gogdl as the oracle) | identical `md5` tree; verify passes; repair of a corrupted file fetches only its chunks |
| G4 | Launch the G3 game in a Quiver prefix with redists + scriptinterpreter + dummy service | reaches the menu; registry keys match a gogdl/Heroic setup |
| G5 | comet as a sidecar with the token handoff from section 4.4, on a Galaxy-SDK title from comet's "working" table | an achievement unlock appears on gog.com; Quiver receives the `achievement_unlocked` event |
| G6 | Cloud-save round trip on a game with `cloudStorage.enabled` and two locations | download, play, upload; the second machine (or a fresh prefix) sees the change |
| G7 | Play-time POST and read-back; presence on and off | `time_sum` grows by the session's minutes |

### Phase 1: MVP (GOG library you can play)

- `gog-core` auth, library, gamesdb art and metadata; GOG games in the merged library with a
  store badge.
- v2 install, verify, repair and update (chunk diff) on the existing queue: pause, resume,
  limits, free space, previews.
- Per-game prefix with redists, scriptinterpreter, `temp_executable` and the dummy service;
  launch from `playTasks`; sessions, stop, markers; launch settings and history (namespaced).
- comet sidecar for Galaxy-SDK games; play time posted after each session (with an offline
  queue) and read for the headline.
- Achievements and stats read on the game page (definitions, unlock state, icons).
- Uninstall; CLI parity (`--store gog` or `gog:` prefixes, `--json`), activity log, agent
  policy.

### Phase 2: parity

- Cloud saves with backups, restore and save stores generalised to folder locations;
  per-profile stores through prefix-folder overlays (MODS-V2's `profile_config` mechanism).
- DLC install and remove; DLC list with "View on GOG" links.
- v1 builds; imports from Heroic, Lutris, Galaxy-in-Wine, folders and offline installers
  (`innoextract`), each verified against the manifest.
- Exports (`quiver:gog:<id>`), mods with a namespaced catalog, a welcome "GOG" step, Move
  (generic, for all stores).
- Sight: unlock toasts and session info for GOG games; answer the SDK's overlay calls
  sensibly.

### Phase 3: beyond parity

- comet in-process (after upstream lands a library entry point), per-account routing,
  direct unlock callbacks.
- xdelta3 patch updates; offline-installer and extras downloads (archival); optional
  presence; native Linux builds as an opt-in per game; private branches (beta builds with a
  password, as Heroic supports).

---

## 6. Open questions and steers

1. **How to reuse comet.**
   - (a) Sidecar with an upstreamed token-fd option, and the `XDG_CONFIG_PATH` trick in the
     meantime.
   - (b) In-process crate now, with a fork if needed.
   - (c) Port.

   *Recommend (a), then (b) once upstream exposes `serve(listener, TokenProvider, Config)`,
   and offer that PR ourselves.*
2. **gogdl.**
   - (a) Native Rust downloader in `gog-core`.
   - (b) gogdl as a sidecar (GPL-3.0 and Python, spawned per operation).
   - (c) A clean-room port.

   *Recommend (a): Quiver's queue, resume, verify and repair need per-chunk control; keep
   gogdl as a test oracle in G3.*
3. **Login UX.**
   - (a) Browser plus pasting the redirect URL (app and CLI).
   - (b) A small web-view helper process (WebKitGTK or `wry`) that watches for
     `on_login_success` and captures the code.
   - (c) Both.

   *Recommend (a) for the MVP and (b) later as a one-click option. GPUI has no web view.*
4. **Which builds.**
   - (a) Windows builds via Proton only.
   - (b) Also native Linux installers.
   - (c) Native when available.

   *Recommend (a) by default, with (b) as a per-game opt-in in phase 3: one launch path, the
   Galaxy SDK works (some Linux builds lack it), and mods and prefixes behave the same.*
5. **Several GOG accounts.**
   - (a) Full model now, but one running GOG account at a time (sidecar limit).
   - (b) A single GOG account until in-process comet.

   *Recommend (a): the model is shared with Ubisoft, and only the concurrent-play limit is
   GOG-specific.*
6. **Reporting to GOG.** Play time and presence each have two options: on by default, or
   off by default.

   *Recommend play time on, as for Ubisoft's `report_playtime`, and presence ("playing X",
   seen by GOG friends) off by default, behind a setting.*
7. **Galaxy overlay.**
   - (a) Sight only.
   - (b) Also offer GOG's own overlay via comet and galaxy-helper injection.

   *Recommend (a): no proprietary injector, and it needs umu and online use; revisit only if
   a game requires it.*
8. **Cloud-save conflicts.**
   - (a) Quiver's Ubisoft model: back up, then pull before and push after, newer file wins
     per file, and ask on a true two-sided conflict.
   - (b) gogdl's model: refuse to sync on conflict.

   *Recommend (a), with the conflict prompt in the app and `confirmed` required for agents.*
9. **Store namespacing of ids, paths and export keys.**
   - (a) Ubisoft stays bare; other stores are prefixed (`gog-<id>`, `quiver:gog:<id>`).
   - (b) Migrate everything to prefixed keys.

   *Recommend (a) (no Steam appid churn); decided in STORES.md (steer X1).*
10. **Achievements read before the game reports.** Show GOG's unlock state on the game page
    and in Sight, even though Ubisoft only shows definitions?

    *Recommend yes: show what each store has, per PROPOSAL principle 4.*
11. **DRM-free extras.** Add an "Archive" action (offline installers, patches, goodies) for
    owned GOG games?

    *Recommend phase 3, opt-in, with its own storage folder.*
12. **User-Agent and identity.** gogdl sends a Galaxy User-Agent; Heroic identifies as
    Heroic.
    - (a) Identify as Quiver.
    - (b) Mimic Galaxy, as Quiver mimics Connect's headers for UbiServices.

    *Recommend (a) where it works and (b) only for endpoints that need it, found in G1–G7.*
13. **Test account and game.** Which GOG games do you own with Galaxy-SDK achievements or
    cloud saves, and may the spikes sign in with your account (per the real-prefix rule:
    ask, then revert)? *Needed before G1.*
14. **An official Galaxy for Linux.** GOG said in July 2026 that it is "actively exploring"
    Linux support for Galaxy, with no timeline
    ([GamingOnLinux](https://www.gamingonlinux.com/2026/07/gog-confirm-they-are-working-towards-gog-galaxy-on-linux/)).
    - (a) Proceed.
    - (b) Wait.

    *Recommend (a): Quiver's value is the shared daemon, mods, Sight and exports across
    stores, and an official client would at most replace the comet piece.*

---

## 7. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Private GOG APIs change (a mid-2026 change broke Minigalaxy's library) | Med | High | Isolate each endpoint behind `gog-core` with fixtures; follow Heroic, gogdl and comet releases; degrade per feature (library from `user/data/games` if galaxy-library fails) |
| Login paste UX feels rough | High | Low–Med | Clear copy-the-URL instructions and validation; web-view helper later (steer 3) |
| Token rotation conflicts between quiverd and a comet sidecar | Low–Med (unverified) | Med | comet uses `without_new_session` game tokens and never writes back; in-process ends it |
| SDK coverage gaps (multiplayer and lobbies, global stats, some SDK builds needing the dummy service) | Med | Med | Dummy service in every GOG prefix; comet's compatibility table; per-game notes; multiplayer out of scope |
| GOG setup exes (scriptinterpreter, `temp_executable`) misbehave under Proton | Med | Med | Run them as an install phase with logs; offer re-run setup; record what each wrote |
| Cloud-save folder semantics break `savestores` assumptions | Med | Med | Generalise to locations in phase 2; spike G6 before designing UI |
| One comet port per host blocks two accounts playing at once | Low | Low | Refuse with a reason; in-process routing later |
| License mixing | Low | Med | comet is Apache-2.0 (compatible with MIT; ship its LICENSE); gogdl and Heroic are GPL-3.0, so read them for behaviour and copy no code |
| An official Galaxy for Linux | Med (no timeline) | Low–Med | Architecture keeps comet swappable behind `RuntimeService` |

---

## Sources

Quiver code (this branch):

- `crates/ubi-api/src/lib.rs` (requests)
- `crates/ubi-core/src/*` (module docs cited per row)
- `crates/ubi-bridge/src/win/api.rs`
- `crates/ubi-app/src/ui/welcome.rs`
- `design/*.md`

Heroic (read only, `7e7d7883`):

- `src/backend/storeManagers/gog/{user,library,games,setup,presence}.ts`
- `src/backend/{launcher,save_sync}.ts`
- `src/common/types/gog.ts`
- `src/frontend/screens/WebView/index.tsx` (login URL)

Other sources:

- comet: [repo](https://github.com/imLinguin/comet), [README](https://raw.githubusercontent.com/imLinguin/comet/main/README.md),
  [main.rs](https://raw.githubusercontent.com/imLinguin/comet/main/src/main.rs),
  [communication_service.rs](https://raw.githubusercontent.com/imLinguin/comet/main/src/api/handlers/communication_service.rs),
  [achievements.rs](https://raw.githubusercontent.com/imLinguin/comet/main/src/api/gog/achievements.rs),
  [users.rs](https://raw.githubusercontent.com/imLinguin/comet/main/src/api/gog/users.rs),
  [heroic import](https://raw.githubusercontent.com/imLinguin/comet/main/src/import_parsers/heroic.rs),
  [dummy service](https://raw.githubusercontent.com/imLinguin/comet/main/dummy-service/README.md),
  [Overlay wiki](https://raw.githubusercontent.com/imLinguin/comet/main/docs/wiki/Overlay.md),
  [Workarounds](https://raw.githubusercontent.com/imLinguin/comet/main/docs/wiki/Workarounds.md),
  [Game compatibility](https://raw.githubusercontent.com/imLinguin/comet/main/docs/wiki/Game-Compatibility.md),
  [releases](https://api.github.com/repos/imLinguin/comet/releases) (v0.4.1, 2026-10-28; Apache-2.0)
- heroic-gogdl: [repo](https://github.com/Heroic-Games-Launcher/heroic-gogdl) (GPL-3.0),
  [auth.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/auth.py),
  [saves.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/saves.py),
  [constants.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/constants.py),
  [api.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/api.py),
  [dl_utils.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/dl/dl_utils.py),
  [v1.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/dl/managers/v1.py),
  [v2.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/dl/managers/v2.py),
  [launch.py](https://raw.githubusercontent.com/Heroic-Games-Launcher/heroic-gogdl/main/gogdl/launch.py)
- GOG API docs (community): [Galaxy / content system](https://gogapidocs.readthedocs.io/en/latest/galaxy.html),
  [Auth](https://gogapidocs.readthedocs.io/en/latest/auth.html)
- [Yepoleb/gog_protocols](https://github.com/Yepoleb/gog_protocols) (Galaxy protobufs)
- [lgogdownloader](https://github.com/Sude-/lgogdownloader) (WTFPL; `--galaxy-install`, `--browser-login`)
- [Minigalaxy](https://github.com/sharkwouter/minigalaxy) (GPL-3.0)
- [GamingOnLinux: Minigalaxy 1.4.2 and GOG's API change](https://www.gamingonlinux.com/2026/07/simple-gog-client-for-linux-minigalaxy-version-1-4-2-is-out-now/)
- [GamingOnLinux: GOG working towards Galaxy on Linux](https://www.gamingonlinux.com/2026/07/gog-confirm-they-are-working-towards-gog-galaxy-on-linux/)
- [GamingOnLinux: comet's first release](https://www.gamingonlinux.com/2024/07/open-source-implementation-of-gog-galaxys-communication-service-comet-sees-a-first-release)
