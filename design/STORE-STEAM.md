# Steam as a store in Quiver (scoping notes)

Status: scoping only (2026-11-16). Nothing here is built. Siblings, written in parallel:
`STORE-EPIC.md`, `STORE-GOG.md`, `STORE-EA.md` and `STORE-META.md`, which owns the
cross-store architecture. This document is Steam-specific. Section 3 lists only the
interfaces Steam needs from that architecture.

The ask: "scope out integrating Steam… I want all the same great capabilities we built for
Ubisoft Connect." Quiver already reads and writes a lot of Steam's local state for Steam
copies of Ubisoft games (`design/STEAM-COPIES.md`, `crates/ubi-core/src/exports/steam*.rs`,
`textvdf.rs`, `vdf.rs`, `steamrun.rs`). Much of this plan generalises that code from
"Ubisoft games Steam installed" to "every Steam game".

Conventions: **Verified** means checked here, either in Quiver's code or by reading this
machine's Steam files read-only, with secret subtrees skipped. **Unverified** means it comes
from documentation or prior art and has not been tried. No Steam login or credentialed call
was made while writing this.

---

## 1. Summary and recommendation

**Use the real Steam client as the engine, and make Quiver the cockpit.** For Steam games,
the Steam client is the license authority and the default installer. Quiver does three
things on top of it:

1. It reads Steam's local state (libraries, app manifests, `appinfo.vdf`, art cache, play
   time, achievement cache, cloud cache) to build a first-class library with art, metadata,
   install and download state, play time and achievements. This needs no Steam login in
   Quiver.
2. It drives Steam through documented entry points: `steam://install|validate|uninstall|
   rungameid/<appid>`, `steam -shutdown`, and staged edits of `localconfig.vdf` and
   `config.vdf` while Steam is closed. This is the same pattern Quiver already uses for
   shortcuts and launch options.
3. It runs inside Steam's own launch through the existing `quiver steam-run %command%` hook,
   generalised from Ubisoft games to any Steam game. From there it applies its settings,
   revisions, mod profiles (overlayfs namespace), session tracking, launch markers and Sight
   to Steam games, while Steam's Proton, DRM, Steamworks, overlay, Steam Input, cloud sync,
   achievements and play time stay Steam's.

An **optional Steam sign-in inside Quiver** comes second. It uses QR login with the Steam
Mobile app, over the CM protocol, through a Rust stack (`steam-vent`). It adds what local
files can't give: the full owned library including uninstalled games, DLC ownership,
achievements and global rarity for games that aren't installed, and cloud file listings.

A **Quiver-native depot installer** comes third, if at all. It works like DepotDownloader
or Aurelia, with the user's own session, and writes an `appmanifest` so the Steam client
adopts the result. It is opt-in, and its ToS risk is low in practice but not zero (section 2.3).

**Out, explicitly:** Goldberg and every other Steamworks emulator, `steam_api` replacements,
DLC unlockers (CreamAPI, SmokeAPI and the like), SteamStub/CEG strippers (Steamless), and
running DRM'd Steam games without the genuine client. Also out: reading or reusing the Steam
client's own credentials (`config/config.vdf` ConnectCache, `local.vdf`, `ssfn*`, the
tickets in `localconfig.vdf`), and achievement unlockers (Steam Achievement Manager style).
Quiver never reports an achievement or a stat to Steam itself. Only the game, through the
real client, does.

Why this shape:

- It is the only shape where licensing is unquestionably legitimate. Every Steamworks call
  goes to Valve's own client.
- It matches Quiver's Ubisoft parity where it matters. The bridge gave Ubisoft games "no
  Connect window". Steam games have no equivalent problem to solve: the Steam client is
  well behaved on Linux, ships Proton, and is already running for most users. The value
  Quiver adds is in launch settings, mods, the HUD, saves visibility, agents and one
  library. All of that fits through the `steam-run` hook.
- The prior art agrees. Heroic merged native Steam support on 2026-11-14 (PR #6045). It
  drives Aurelia, a Rust CLI client built on `steam-vent`, with QR login, a depot installer
  and a "real Steam client" mode for DRM. Before that, Heroic's maintainers argued a Steam
  integration "would still delegate installation and launch … to the official client" (PR
  #5067). Lutris treats Steam as a runner and reads local files plus the Web API.

Top risks:

1. **Steam rewrites its files on exit.** Every write must be staged until Steam is closed.
   Quiver already does this for shortcuts and launch options. Adding `config.vdf` (Proton
   mapping) raises the stakes because that file also holds Steam's credentials, so edits
   must change only the target bytes.
2. **Account risk from a second CM client.** It is low in practice, given DepotDownloader,
   ArchiSteamFarm and node-steam-user have run for years, but not zero: SSA §2.G forbids
   emulating Valve's protocols without consent. There are anecdotal account flags when
   third-party QR logins pose as the *mobile app*. Mitigation: sign-in is optional, honest
   client identity, never "playing" from Quiver's session, never concurrent with a game.
3. **Launch-option hooks are global state.** Patched `LaunchOptions` break the game if
   Quiver is uninstalled or moved. They may also sync to other machines (unverified).
   Every hook must be reversible (it already is) and opt-in per game.
4. **Undocumented binary formats** (`appinfo.vdf` v29 with its string table, the
   `appcache/stats/*.bin` files) change without notice. Treat them as caches with fallbacks,
   never as truth for anything destructive.

---

## 2. Capability matrix

### 2.1 What "the same capabilities" means (checked against the code)

| # | Capability as built for Ubisoft | Where |
|---|---|---|
| C1 | Accounts: password + 2FA login, remember-me refresh, one keyring entry per account, several accounts, default and per-game launch account | `ubi-core/src/auth.rs`, `store.rs`; `Request::Login/Login2fa/Logout/AccountDefault` |
| C2 | Library: ownership sync, product configs, merged across accounts; art chain (config → games API → Steam cache → Steam CDN → custom); game info, add-ons, links | `library.rs`, `gameinfo.rs`, `spaces.rs`, `ubi-api/src/art.rs` |
| C3 | Install pipeline: one queue (pause, resume, reorder, bandwidth limit, max active, auto-update), free-space check with margin, slice-level resume, update preview, verify and repair, import of existing installs (reflink), uninstall with or without prefix. **Move is designed, not built** (no `Request::Move`) | `install.rs`, `download.rs`, `verify.rs`, `ubid/src/downloads.rs` |
| C4 | One Proton prefix per game; Proton choice; settings with revision history, restore, actor labels | `launcher.rs` (`list_protons`), `settings.rs`, `SettingsSet/History/Restore` |
| C5 | Licensed launch without Connect: `ubi-bridge` owns the SDK pipes; the broker gets Denuvo and ownership tokens | `ubi-bridge`, `broker.rs`, `launch.rs` |
| C6 | Cloud saves (pull before, push after), backups, save stores, merge, restore with approvals | `cloudsave.rs`, `savesync.rs`, `saves.rs`, `savestores.rs` |
| C7 | Achievements and progression, read-only from UbiServices. Earning is logged by the bridge but not reported (`EarnAchievementRsp` OK) | `progress.rs`, `ubi-bridge/src/win/api.rs` |
| C8 | Play time: reported to Ubisoft (heartbeat every 330 s), local `playtime.jsonl` with settings revision, `RemotePlaytime` | `playsession.rs`, `settings.rs` |
| C9 | DLC / add-ons: owned and not owned, store links | `info.rs` (`GameInfo`) |
| C10 | Mods v2: layers, profiles, versions, config patches, per-profile prefix folders, fixes on vanilla, built-in audited catalog keyed by product id; MOD-CONFIG options (designed) | `mods.rs`, `modversions.rs`, `confpatch.rs`, `overlay.rs`, `mods/<product>/…` |
| C11 | Sight overlay and the pinned HUD (designed, not built) | `design/SIGHT.md`, `design/SIGHT-HUD.md` |
| C12 | Exports: `.desktop` files and Steam shortcuts (`shortcuts.vdf`, grid art), staged while Steam runs, restart only after confirmation | `exports/*` |
| C13 | Steam copies of Ubisoft games: in the library, `steam-run`, patch and unpatch, offers, launch settings applied inside Steam's launch | `steamrun.rs`, `steamplan.rs`, `exports/steamgames.rs`, `ubid/src/steamlib.rs` |
| C14 | CLI and agents: everything is a daemon request with a source label, activity log, `confirmed` gates for destructive acts, `--json` | `ubi-api`, `ubi-cli`, PROPOSAL "What agents may do" |
| C15 | First run: Sign in, Storage, Import, Steam, Controller, Fixes, Theme | `ubi-app/src/ui/welcome.rs` (`Step`) |

### 2.2 Steam, per capability

Effort: S is days, M is one to two weeks, L is longer or open-ended. Risk covers technical,
ToS and data-loss risk.
"Client" means the genuine Steam client must be running or must do the work.

| Capability | Approach for Steam | Needs client? | Prior art | Effort | Risk |
|---|---|---|---|---|---|
| **Account (local)** | Steam users from `userdata/<accountid>` plus `loginusers.vdf` names (built: `Steam::users`). Follow the client's signed-in user. Switching user means restarting Steam after confirmation | yes (it is the client's account) | Lutris `get_active_steamid64` | S | Low |
| **Account (Quiver sign-in, optional)** | QR login (`IAuthenticationService.BeginAuthSessionViaQR`), then poll, then a refresh token in the keyring, then CM logon with `steam-vent`. Password and Steam Guard code as a fallback. Section 4 | no | SteamKit2, DepotDownloader `-qr`, Aurelia `login --qr`, node-steam-session | M | Med (ToS letter, account flags) |
| **Library: installed** | Walk `libraryfolders.vdf` and `appmanifest_*.acf` (built for Ubisoft games in `steamgames::library_folders/app_manifests`), for all apps. Skip tools (Proton, Steam Linux Runtime, Steamworks Redist 228980) by `appinfo` type | no | Lutris `add_installed_games`, Heroic PR #5067 | S | Low |
| **Library: owned, not installed** | (a) After Quiver sign-in: the CM `ClientLicenseList` licenses, mapped package → apps through PICS. (b) Without sign-in: Web API `IPlayerService/GetOwnedGames` needs a Web API key and a public profile (Lutris does this). Whether a user access token works instead is **unverified**. (c) Local hints only: `localconfig.vdf` apps with `Playtime` (played, not "owned") | (a) no | SteamKit2, Aurelia, Lutris | M (a) | Med (a), Low (b) |
| **Metadata** | `appcache/appinfo.vdf`: binary, magic `0x07564429` = v29 here (**verified**), with a string table. It holds name, type, DLC list (`listofdlc`), launch configs, cloud `ufs` roots, supported OS and Deck rating. PICS after sign-in. Store `appdetails` (public, unauthenticated) for descriptions | no | SteamDatabase's appinfo parsers, Heroic (appdetails) | M | Med (format churn) |
| **Art** | Built: `exports/steamart.rs` (local `librarycache`, hashed layout) and the Steam CDN, plus custom art. Now the primary source for Steam games | no | built | S | Low |
| **Install** | Default: `steam://install/<appid>`, which opens Steam's install dialog (library choice and size shown by Steam). Quiver's Downloads view mirrors progress from the app manifest (`StateFlags`, `BytesToDownload/BytesDownloaded`, `BytesToStage/BytesStaged`, all **verified** present) with inotify | yes | Valve `steam://` protocol; Lutris | S | Low |
| **Install (Quiver-native, later)** | Depot download with the user's session (manifest → depot key → chunks from CDN), written into a Steam library with an `appmanifest`, so the client adopts it. Rust: `steamroom`/`steamroom-client`, Aurelia's `steam-cdn` fork | no (session only) | DepotDownloader, steamcmd (official), Aurelia, steamroom | L | Med |
| **Update** | Steam's: auto-update or Steam's queue. Quiver shows "update pending" (`StateFlags`, `TargetBuildID != buildid`). The per-game auto-update setting is `AutoUpdateBehavior` in the app manifest (write it staged, Steam closed) | yes | — | S | Low |
| **Verify** | `steam://validate/<appid>`. Quiver shows the result through the manifest's state | yes | — | S | Low |
| **Uninstall** | `steam://uninstall/<appid>` (Steam asks for confirmation). Quiver offers to clean its own state (profiles, exports). The prefix lives in `compatdata/<appid>` and is Steam's to remove | yes | — | S | Low |
| **Move / storage** | Show every Steam library in Quiver's Storage view (`statvfs`, `SizeOnDisk` per app). Moving is delegated to Steam's storage manager (opened via a `steam://` URL, **unverified** which). A manual move with Steam closed (folder + manifest + `libraryfolders.vdf` `apps`) is possible but not proposed | yes | Aurelia keeps `libraryfolders.vdf` in sync | S (show) / M (move) | Med (manual move) |
| **Prefix / Proton** | The prefix is Steam's `compatdata/<appid>/pfx` (built for Steam copies). Proton per game is `config/config.vdf` `InstallConfigStore/Software/Valve/Steam/CompatToolMapping/<appid>` (key **verified**; values not read). Write it staged with Steam closed, with a byte-preserving edit (`textvdf::set_string`). Quiver's Proton list already scans `compatibilitytools.d` | write: client closed | SteamTinkerLaunch, ProtonUp-Qt (installs tools) | M | Med (file holds secrets) |
| **Launch + licensing** | `steam://rungameid/<appid>` (built: `exports/steamlaunch.rs`, with its own systemd scope). With the hook, `quiver steam-run %command%` wraps Steam's command (built for Ubisoft). Steam stays the license authority: DRM, Steamworks and CEG all go to the real client | **yes** | SteamTinkerLaunch (same launch-option wrapper idea) | S / M (generalise `steam-run`) | Low |
| **Launch without Steam's launch** | Quiver's own Proton with Steam running, so the game's `steam_api` reaches the client through Proton's `lsteamclient`. **Unverified**, and play time and overlay behaviour are unknown. Not proposed for DRM'd games. A spike decides whether it is ever worth it | yes (running) | umu-launcher (no Steam needed for non-Steam games) | M | Med |
| **Cloud saves** | Sync is Steam's (before launch and after exit, in the client). Quiver shows per-file cloud state from `userdata/<accountid>/<appid>/remotecache.vdf` (`root`, `size`, `sha`, `time`, `remotetime`, `syncstate`, `persiststate`, **verified** present) and the local files (`remote/` or Auto-Cloud roots in the prefix). It also takes local backups before launches, as for Ubisoft | yes (sync) | Steamworks Cloud docs; SteamCloudFileManagerLite (uses the real client) | S–M | Low |
| **Cloud (Quiver-driven, no)** | `ICloudService` Web API is documented for *non-Steam versions* with partner OAuth scopes (`read_cloud`/`write_cloud`). The CM `Cloud.Client*` calls are client-only protocol. Aurelia does `cloud sync` over CM. Proposal: no upload from Quiver. At most a read-only listing after sign-in | — | Aurelia | M | High (data loss, ToS) |
| **Save stores** | Default = the game's real save folder (Steam Cloud syncs it, from the client's view outside Quiver's namespace). A profile with its own store overlays that folder inside the launch namespace only, so Steam Cloud never sees it. That is the same "one cloud store per game" rule as for Ubisoft. Needs the `steam-run` namespace (STEAM-COPIES §4) | — | built design | M | Med (game paths vary) |
| **Achievements (read)** | Local: `appcache/stats/UserGameStatsSchema_<appid>.bin` (names, per-language display strings, icons) and `UserGameStats_<accountid>_<appid>.bin` (bit fields and `AchievementTimes`). Both are binary KeyValues, the format `exports/vdf.rs` already parses (**verified** by hex dump). Only for apps the client has cached. After sign-in: CM user stats, plus `ISteamUserStats/GetGlobalAchievementPercentagesForApp` (public) for rarity | no | xan105 node-vdf, Achievement Watcher; Aurelia `achievements` | S–M | Low |
| **Achievements (unlock reporting)** | The game reports them to the real Steam client through Steamworks. Quiver never reports them. Sight can toast an unlock by watching the stats file (inotify) | yes | — | S (toast) | Low |
| **Play time** | Steam records it. Quiver reads `localconfig.vdf` `apps/<appid>/Playtime` (minutes), `Playtime2wks`, `LastPlayed` (**verified** keys), and the app manifest's `LastPlayed`. Local session history (with settings revision) comes from `steam-run`, as for Steam copies | yes (records) | Heroic PR (play time from localconfig) | S | Low |
| **Progress / stats** | Steam has no Ubisoft-style "progression" page. Stats are in the same user-stats cache, and global rarity is public | no | — | S | Low |
| **DLC** | List from `appinfo` `listofdlc`. Owned comes from licenses (after sign-in) or is inferred from `InstalledDepots` `dlcappid` (**verified** field). Not-owned DLC links to `https://store.steampowered.com/app/<dlcid>`. Install and enable are Steam's | install: yes | — | M | Low |
| **Workshop mods** | Subscriptions and downloads are Steam's (`steamapps/workshop/content/<appid>/<id>`). Quiver can show them and later wrap a Workshop item as a read-only Quiver mod layer. DepotDownloader `-pubfile` exists, but is not proposed | yes | DepotDownloader | L (later) | Med |
| **Mods and profiles, Options** | Everything in Mods v2 works through the `steam-run` namespace (designed for Steam copies). Built-in mods need store-qualified ids (`mods/steam-292030/…`). PCGamingWiki is keyed by Steam app id, which makes recipes easier. MOD-CONFIG is store-agnostic | — | STEAM-COPIES §4 | M | Med |
| **Sight and the HUD** | The Sight layer loads inside pressure-vessel (as SIGHT already requires for Steam copies). It coexists with the Steam overlay (Shift+Tab). Session data comes from `steam-run`. Achievement toasts come from the local stats cache (de-duplicated against Steam's own toast; steer 9) | — | SIGHT §8 | S (on top of Sight) | Low |
| **Exports** | Steam games are already in Steam, so no shortcut is needed. Pinned *profiles* of Steam games still need one: a non-Steam shortcut that runs `steam://rungameid/<appid>` with `QUIVER_PROFILE` passed to `steam-run`, which needs a design for passing the profile (steer 3). `.desktop` files: `steam steam://rungameid/<appid>` | — | BoilR, Steam ROM Manager | S–M | Low |
| **CLI and agents** | Same verbs on store-qualified ids (`quiver install steam:292030`, which delegates). Agent policy: installing through Steam is Allow, Steam sign-in is Deny, patching launch options is Allow (reversible), Steam restart is Ask | — | Aurelia (`--json` everywhere) | S–M | Low |
| **First run** | The existing Steam step grows: "Show your Steam games in Quiver", which user, "start Steam games through Quiver for settings and mods (per game)", optional "Sign in to Steam for your whole library" | — | — | S | Low |

### 2.3 What strictly needs the genuine client, and the ToS picture

| Needs the Steam client running (or doing it) | Can be independent of the client |
|---|---|
| Launching any game that calls Steamworks or has Steam DRM (SteamStub, CEG); achievements, stats, rich presence, multiplayer, overlay, Steam Input, Remote Play | Reading local state (libraries, manifests, `appinfo`, art, play time, stats cache, cloud cache) |
| Steam Cloud sync | Owned-library listing, PICS metadata, achievements read (with Quiver's own session) |
| Play time recorded on the account | Depot downloads with the user's own licenses (DepotDownloader / steamcmd style) |
| Install, update, verify and uninstall *as Steam knows them* (unless Quiver writes manifests itself) | Global achievement rarity, store metadata (public) |

**ToS reading (Steam Subscriber Agreement):**

- §2.G(ii) forbids, without Valve's consent, emulating "the communication protocols used by
  Valve" through "protocol emulation, tunneling, … use of a utility program …". A
  SteamKit-style client arguably falls under the letter of this.
- §4.B forbids "unauthorized third-party software" that tampers with Steam. Driving Steam
  through `steam://` URLs, its command line and its documented launch options is not
  tampering. Injecting into Steam's CEF UI (what Decky Loader does through
  `.cef-enable-remote-debugging`) is closer to the line. Not proposed.
- §1.C: credentials stay the user's own. Quiver's own sign-in is the user logging in. Reading
  Steam's stored tokens would be credential extraction and is out.
- In practice: DepotDownloader (SteamRE), ArchiSteamFarm and node-steam-user have operated
  openly for about a decade. I found no reports of bans for *downloading owned content*. The
  reported flags are about behaviour: a third-party QR login posing as the Steam *Mobile
  App* got accounts marked compromised (doctormckay forum, June 2025). Logins posing as
  `WebBrowser` reportedly did not. **steamcmd is Valve's own** and is the ToS-safe way to
  download depots independently, at the cost of a 32-bit binary that is awkward to embed.
- A second CM logon on the same account can disturb the running client's session (friends
  state oddities, reported 2023–2024). Starting a game from another session kicks the other
  one (`LoggedInElsewhere`). Quiver's session must never set a "playing" state, and should
  disconnect when idle.

---

## 3. Architecture

### 3.1 What exists that is already Steam-shaped

- `exports::steam::Steam` finds the install (native or Flatpak), its users, `is_running`,
  `game_running`, and handles `restart_steam` (only after confirmation) and staged writes.
- `exports::textvdf` (read, plus a byte-preserving `set_string`) and `exports::vdf` (binary
  KeyValues, round-trips byte for byte). These are the two formats behind every local Steam
  file this plan reads, except `appinfo.vdf`'s v29 container, which wraps binary KeyValues
  with a string table.
- `exports::steamgames` walks libraries and manifests and does the launch-option patch and
  unpatch with staging. `exports::steamlaunch` handles `steam://rungameid` with a systemd
  scope, and `exports::steamart` handles the local art cache.
- `steamrun` (pure) with `ubi-cli steamrun.rs` (I/O) rewrites Steam's launch command, and
  `SteamLaunchPlan` applies settings inside Steam's launch.

### 3.2 Generalising: store-agnostic interfaces Steam needs

Names are suggestions. STORE-META decides the real shape. The Steam column says how each is
filled.

| Interface | Responsibility | Steam implementation |
|---|---|---|
| `StoreAccount` | sign in or out, identity, secrets in the keyring, "signed in elsewhere" | Local mode: the client's user (no secret). Sign-in mode: QR or password plus Guard, refresh token in the keyring |
| `LibraryProvider` | owned titles, metadata, art candidates, DLC graph, change events | local scan plus `appinfo.vdf`, then PICS or licenses after sign-in. Events from inotify on `steamapps/` and `appcache/` |
| `Installer` | preview, install, update, verify, uninstall, move, progress events, storage | `SteamClientInstaller` (delegates via `steam://`, progress from manifests). Later `SteamDepotInstaller` (native) |
| `Launcher` | build and run a launch, session lease, markers, mod namespace, settings applied | `SteamLauncher`: `rungameid`, and when hooked, `steam-run` hands the command to quiverd's launch plan |
| `LicenseProvider` | what must be true for a licensed launch | "Steam client running and signed in as an owner". For Ubisoft it is the bridge plus broker |
| `SaveProvider` | slots, cloud state, backups, stores | local files plus `remotecache.vdf`, sync delegated, backups by Quiver |
| `AchievementProvider` | definitions, unlocks, rarity, unlock events | stats cache (inotify), with CM after sign-in |
| `PlaytimeProvider` | remote total, local sessions | `localconfig.vdf`, plus `steam-run` sessions |
| `RuntimeProvider` | Proton choice, prefix location | `CompatToolMapping` (staged), `compatdata/<appid>` |

**Identity.** Today every request carries `product_id: u32` (41 places in
`ubi-api/src/lib.rs`). A multi-store Quiver needs a store-qualified reference such as
`GameRef { store: "steam", id: "292030" }`, printed `steam:292030`. Ubisoft keeps bare ids
for compatibility, so `3539` means `ubi:3539`. Export keys become `quiver:steam:292030[/profile]`.
Existing Ubisoft keys and their CRC-derived shortcut app ids must not change.

**One title, several stores.** A Ubisoft game bought on Steam is already *one* library entry
with `steam: Option<SteamGame>`. In general, an entry links store records
(`ubi:66088` with `steam:3751950`) and picks a *play route* per entry. For Steam copies of
Ubisoft games that route is "Steam launch, `steam-run`, Ubisoft bridge" (built). The Steam
provider's records replace today's Ubisoft-only Steam scan, and owned-but-not-installed
Steam copies can appear once signed in.

**Crates.** A `store-steam` crate (or `ubi-core::steam`, if the crate rename waits) holding
the local readers (moved from `exports/` with re-exports), the `appinfo` and stats parsers,
the URL driver, and later `steam-vent` sign-in and the depot installer behind a cargo
feature. That keeps the CM stack out of builds that don't want it. `quiverd` hosts the
provider. `ubi-bridge` stays Ubisoft-only.

### 3.3 Generalised `steam-run`

Today `steam-run` is the hook for Ubisoft products (`--product <id>`, the bridge swapped in
for the game executable). Generalised:

```
<quiver> --source steam steam-run [--product <ubi id>] [--app <appid>] %command%
```

- Without a Ubisoft product, it does **not** touch the executable. It only asks quiverd for
  the launch plan (`env`, `dll`, `args`, `wrappers`, mod namespace, save store, markers,
  Sight layer env), holds a play-session lease with quiverd for local history, and execs or
  spawns Steam's command. The app id comes from `SteamAppId` in Steam's environment.
- Mod overlays use the namespace design from STEAM-COPIES §4: `steam-run` spawns Steam's
  command as a child that joins the namespace.
- Patching stays opt-in per game (`quiver steam patch <game>`, an offer when the user first
  changes a setting or adds a mod for a Steam game), staged, and exactly reversible.

---

## 4. Auth and account details

**Local mode (MVP): no Steam secret in Quiver at all.** The client's signed-in user comes
from `userdata/` plus `loginusers.vdf` names and recency, as built. Quiver never reads
`config/config.vdf` (ConnectCache, account tokens), `local.vdf`, `ssfn*`, `steam.token`, or
the token-bearing keys in `localconfig.vdf`: `apptickets`, `Offline/Ticket`, `Offline/
EncryptedTicket`, `CloudKey`, `depots/*/CDN/*/token`, `nettickets`, `streaming/Devices/*/Key`.
All of these are present in this machine's `localconfig.vdf` (key names checked, values
not read).

Consequences for code that already exists:

- `steamgames::localconfig` reads that file and rewrites one key. It must never log or copy
  values beyond `LaunchOptions`.
- The `localconfig.vdf.quiver-backup` Quiver writes before a change contains those tickets
  too. It should be written `0600` and ideally rotated. Today it gets the default umask
  (`0644` here), which is no worse than Steam's own files (`0755` here), but it is a second
  copy of the same tickets.

**Sign-in mode (phase 2), over CM:**

- **Transport.** `steam-vent` (Rust, CM protocol, async). Its README lists password,
  Steam Guard (device or email), device confirmation and saved machine token. QR is listed
  as *not done* in 0.5.0. Aurelia, built on `steam-vent`, has `login --qr` working, so
  either upstream it or carry the call: `Authentication.BeginAuthSessionViaQR` and
  `PollAuthSessionStatus` are plain unified-service RPCs.
- **QR flow (recommended).** `BeginAuthSessionViaQR{device_details}` returns
  `challenge_url` (shown as a QR code in the app, as text in the CLI), `client_id`,
  `request_id` and an interval. Poll `PollAuthSessionStatus` until the user approves in the
  Steam Mobile app. The result is `refresh_token`, `access_token` and `account_name`. No
  password ever reaches Quiver.
- **Password flow (fallback).** `GetPasswordRSAPublicKey`, then
  `BeginAuthSessionViaCredentials` (password RSA-encrypted), then
  `UpdateAuthSessionWithSteamGuardCode` (TOTP or email code) or device confirmation, then
  poll. Keep the returned guard data (the "machine auth token") so email codes are not
  asked for again.
- **Client identity.** `platform_type = SteamClient` (needed for CM logon with the token),
  with honest `device_friendly_name` such as "Quiver on <hostname>" and `os_type` Linux.
  Never pose as `MobileApp`: that is what the anecdotal account flags were tied to.
- **Tokens.** Refresh tokens are JWTs, bound to their platform type, and live about 200 days
  (forum reports, unverified). CM logon uses the refresh token. Web access tokens come from
  `GenerateAccessTokenForApp` (the CM route is reported to be flaky). When a token nears
  expiry, show "sign in again". Renewal is possible but undocumented.
- **Where secrets live.** The Secret Service keyring, like Ubisoft sessions (`store.rs`
  uses `keyring::Entry::new(SERVICE, "session:<userId>")`). Steam would use
  `steam:<steamid64>` holding `{refresh_token, guard_data}`. Non-secret index entries go in
  `accounts.json` with a `store` field. Aurelia keeps `session.json` in plain files, which
  Quiver should not copy.
- **Session hygiene.** Connect on demand (library refresh, install, achievements), stay
  connected while downloads run, disconnect when idle. Never send games-played. Handle
  `LoggedInElsewhere` by pausing, never by reconnect loops, which mirrors PROPOSAL's "signed
  in elsewhere (decided)". The sign-in account must match the client's user, or Quiver shows
  both and says which one launches.
- **Multiple Steam accounts.** Several local users are supported read-only. One is active in
  the client at a time. "Play" for a game owned only by the other user offers to switch
  (restart Steam, after confirmation). Steam Families sharing is the client's business.
- **Agents.** Sign-in is Deny (as for Ubisoft). Sign-out is Ask.

---

## 5. Phased plan

**Phase 0: spikes (each a day or two; the ones that need the real client need the user's OK).**

- S1 Generalised `steam-run` on a non-Ubisoft game: env, wrappers, Steam's "Stop", overlay
  and Steam Input unaffected. Then the namespace child mode with one overlay.
- S2 Do `LaunchOptions` sync between machines (Steam Cloud of `localconfig`)? Decides how
  loudly to warn.
- S3 Parse `appinfo.vdf` v29 and the two stats `.bin` files over this machine's caches,
  read-only. Measure the cost: `appinfo.vdf` is about 3 MB here.
- S4 `CompatToolMapping` edit with Steam closed on a test entry. Check that Steam keeps it,
  and that the byte-preserving edit leaves the rest of `config.vdf` identical.
- S5 (only if phase 3 is wanted) `steam-vent` QR login on a throwaway account. This needs
  the user's consent and is not run by agents.
- S6 (optional) Own-Proton launch with Steam running, for a Steamworks game, through
  `lsteamclient`: does it work, is play time recorded, does the overlay load?

**Phase 1 (MVP): Steam in the library, client mode, no Steam login.**

- Steam provider: installed games from every library, metadata from `appinfo.vdf` (fallback:
  the manifest name), art (built), play time and last played, achievements from the stats
  cache, cloud file state from `remotecache.vdf`, DLC list (installed DLC marked).
- Play through `rungameid` (built). Install, verify and uninstall through `steam://`.
  Downloads view mirrors Steam's progress. The Storage view includes Steam libraries.
- Identity: `steam:<appid>` refs across `ubi-api` and the CLI. Ubisoft ids unchanged.
- The existing Steam copies merge into the same entries (no regression to Ubisoft-on-Steam).
- First-run Steam step: show Steam games yes or no, which user.

**Phase 2: Quiver around Steam's launch.**

- Generalised `steam-run` (opt-in per game): launch settings and revisions, local sessions,
  markers (`QUIVER_*`, scope, Hyprland tag), Sight and HUD env.
- Mod profiles and save stores through the namespace. Built-in mods keyed by store id.
- Proton per game via staged `CompatToolMapping`. Auto-update per game via the manifest.
- Sight: session page, achievement toasts from the stats cache, Steam overlay coexistence.

**Phase 3: optional Steam sign-in.**

- QR login, keyring, CM logon on demand. Owned library (licenses to PICS), DLC ownership,
  achievements and rarity for uninstalled games, read-only cloud listing.

**Phase 4: only if the steers want it.**

- Quiver-native depot installer into Steam libraries (manifest-adopted), with Quiver's queue,
  bandwidth rules and verify. Or steamcmd as the engine.
- Workshop items as mod layers. Moving installs between libraries.

---

## 6. Open questions and steers

1. **Engine for installs.**
   - (a) Steam client only.
   - (b) Client by default, with a Quiver-native depot installer opt-in later.
   - (c) Native by default.

   *Recommendation: (b), with (a) until phase 4.* Native buys a unified queue and headless
   installs, but costs an L-sized protocol surface and ToS exposure, for little gain while
   the client is running anyway.
2. **Steam sign-in inside Quiver.**
   - (a) Never.
   - (b) Optional, QR only.
   - (c) Optional, QR or password plus Guard.

   *Recommendation: (b) in phase 3,* honest `SteamClient` identity. QR keeps passwords out
   of Quiver entirely.
3. **Which Steam games start through Quiver (`steam-run`).**
   - (a) None, launch only.
   - (b) Opt-in per game, offered when you change a setting or add a mod.
   - (c) All games Quiver shows.

   *Recommendation: (b).* It is global, persistent state in Steam's files, and (c) makes
   uninstalling Quiver break every game.
4. **Proton for Steam games.**
   - (a) Leave it to Steam and only show it.
   - (b) Write `CompatToolMapping`, staged until Steam closes.
   - (c) Swap the Proton path inside `steam-run`.

   *Recommendation: (b),* with the same staging and confirm-to-restart rules as launch
   options. (c) desynchronises Steam's runtime choice.
5. **Where native installs would go (if 1b).**
   - (a) Into a Steam library with an `appmanifest`, so Steam adopts and updates it.
   - (b) Quiver's own library, invisible to Steam.

   *Recommendation: (a);* (b) can't launch DRM'd games through Steam.
6. **Identity scheme.** Store-qualified `steam:<appid>`, with Ubisoft ids kept bare for
   compatibility. *Recommendation: yes; align with STORE-META.*
7. **Ubisoft games bought on Steam.** One entry with two store records (today's behaviour),
   or two entries? *Recommendation: one entry,* play route "Steam, then `steam-run`, then
   bridge"; the Steam provider replaces the Ubisoft-only scan.
8. **Steam Cloud.**
   - (a) Display plus local backups only.
   - (b) Also a read-only cloud listing after sign-in.
   - (c) Quiver-driven upload and download.

   *Recommendation: (a) now and (b) in phase 3. Never (c).* Steam's client already syncs
   around every launch.
9. **Achievement toasts.**
   - (a) Steam overlay only.
   - (b) Sight too, always.
   - (c) Sight only when the Steam overlay is off for that game.

   *Recommendation: (c),* from the local stats cache.
10. **Library before sign-in.**
    - (a) Installed only.
    - (b) Installed plus a Web API key the user pastes (public profile, as Lutris does).
    - (c) Installed plus "played" from `localconfig`.

    *Recommendation: (a),* with (c) as a greyed "previously played" collection if wanted.
    (b) asks users to make profiles public and manage a key, which PROPOSAL avoided
    (SteamGridDB was dropped for the same reason).
11. **Own-Proton launches outside Steam's launch** (spike S6), with the client still
    running. *Recommendation: no* unless S6 shows play time and overlay intact. There is
    nothing to gain over `steam-run`.
12. **Agent policy for Steam.** *Recommendation:* install, verify and play through Steam:
    Allow. Patch or unpatch launch options and Proton mapping: Allow (reversible, staged).
    Uninstall: Ask. Restart Steam: Ask (as decided). Steam sign-in: Deny.
13. **Flatpak Steam.** It is unsupported for `steam-run` today (its sandbox can't run
    `quiver`). Should read-only library and launch support Flatpak? *Recommendation: yes*
    for reading and `rungameid` (paths under `~/.var/app/com.valvesoftware.Steam`), no hook.

---

## 7. Interplay with existing work

- **Steam copies of Ubisoft games.** They stay as built. Their library rows become Steam
  provider records linked to Ubisoft products. The Ubisoft-only `find_games` detection becomes
  a classifier over the Steam provider's installed list. Offers, patch and unpatch are
  unchanged.
- **Proton choice.** For Quiver-installed Ubisoft games, Quiver's own `proton` setting. For
  Steam games, Steam's mapping, edited through steer 4. The app labels which applies.
  Prefix snapshots on Proton change (PROPOSAL) apply to `compatdata/<appid>/pfx` too,
  taken with Steam closed or the game not running.
- **Steam overlay and Sight.** Both are in the game: Steam's is a Vulkan layer plus a
  `gameoverlayrenderer.so` preload, and Sight is a compositor panel plus the
  `VK_LAYER_QUIVER_sight` HUD. Different hotkeys; never summon Sight on Shift+Tab. Steam's
  FPS counter (`InGameOverlayShowFPS*` in `localconfig`) and Quiver's HUD can both be on;
  the HUD settings should note it.
- **Steam Input and hyprpad.** Unchanged from PROPOSAL and SIGHT: hyprpad feeds a virtual
  pad, strips Guide and replays a Guide tap to Steam, and Steam Input reads the virtual pad
  for Steam games. While Sight is open, hyprpad's mode neutralises the game's pad. Per-game
  Steam Input settings (`UserLocalConfigStore/apps/<appid>/UseSteamControllerConfig`) stay
  Steam's. Quiver only shows them.
- **Exports.** No shortcut for Steam games themselves. Pinned profiles of Steam games need a
  shortcut that passes the profile to `steam-run`: an env var in the shortcut's launch
  options is not available for `rungameid`, so this needs design (a quiverd "next launch of
  app X uses profile P" one-shot is the simplest).

---

## Sources

- SteamKit2 and DepotDownloader: https://github.com/SteamRE/SteamKit, https://github.com/SteamRE/DepotDownloader (`-qr`, `-remember-password`, `-validate`, `-pubfile`, `-os`)
- `steam-vent` (Rust CM client): https://docs.rs/crate/steam-vent/latest (README: auth methods, QR not yet)
- `steamroom` / `steamroom-client` (Rust depot and CDN): https://docs.rs/crate/steamroom/0.3.0, https://docs.rs/steamroom-client
- Aurelia (Rust CLI Steam client on `steam-vent`; QR login, installs, cloud, Wine Steam runtime): https://github.com/Drackrath/Aurelia, https://raw.githubusercontent.com/Drackrath/Aurelia/v0.1.35/USAGE.md
- Heroic Steam support: https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/pull/5067 (local files, delegate to client; maintainer discussion), https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/pull/6045 (merged 2026-11-14, via Aurelia)
- Lutris Steam service: https://github.com/lutris/lutris/blob/master/lutris/services/steam.py, https://github.com/lutris/lutris/blob/master/lutris/util/steam/config.py (`GetOwnedGames` with key, `loginusers.vdf` recency)
- Steam Subscriber Agreement: https://store.steampowered.com/subscriber_agreement/ (§1.C, §2.A, §2.G, §4.B)
- `IAuthenticationService` methods: https://steamapi.xpaw.me/IAuthenticationService; token notes: https://dev.doctormckay.com/topic/4607-access-tokens-and-refreshtoken-lifetime
- QR login account flags: https://dev.doctormckay.com/topic/5758-i-get-my-account-banned-when-i-log-in-with-a-qr-code/; concurrent session disturbance: https://dev.doctormckay.com/topic/4539-logging-into-an-account-without-interrupting-existing-steam-network-sessions/
- `ICloudService` (partner OAuth, non-Steam versions): https://partner.steamgames.com/doc/webapi/icloudservice; Steam Cloud: https://partner.steamgames.com/doc/features/cloud
- `IPlayerService/GetOwnedGames`: https://partner.steamgames.com/doc/webapi/IPlayerService
- `steam://` protocol: https://developer.valvesoftware.com/wiki/Steam_browser_protocol
- Binary KeyValues for stats schema files: https://github.com/xan105/node-vdf
- SteamTinkerLaunch (launch-option wrapper prior art): https://github.com/sonic2kk/steamtinkerlaunch
- umu-launcher: https://github.com/Open-Wine-Components/umu-launcher
- BoilR (Rust, shortcuts from other launchers): https://github.com/PhilipK/BoilR; Steam ROM Manager: https://github.com/SteamGridDB/steam-rom-manager (not re-checked for this note)
- Decky Loader CEF debugging (why UI injection is not proposed): https://wiki.deckbrew.xyz/plugin-dev/cef-debugging
- SteamCMD (Valve's own CLI): https://developer.valvesoftware.com/wiki/SteamCMD (not re-checked: whether it supports QR or mobile confirmation is unverified)
- SteamCloudFileManagerLite (cloud via the real client's Steamworks API): https://github.com/GMMan/SteamCloudFileManagerLite (URL from memory, not fetched)
