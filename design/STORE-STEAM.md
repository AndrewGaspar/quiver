# Steam as a store in Quiver (scoping notes)

Status: scoped 2026-11-16; phase 1 (local mode) built 2026-11-17 on `build/steam-local`. Section 8
says what was built and what wasn't. Phase 2's generalised `steam-run` (settings, tweaks, HUD and
local sessions for any Steam game, the opt-in hook) is built; see section 9. Siblings, written in parallel:
`STORE-EPIC.md`, `STORE-GOG.md`, `STORE-EA.md` and `STORE-META.md` (the Meta VR store).
`STORES.md` owns the cross-store architecture. This document is Steam-specific. Section 3 lists only the
interfaces Steam needs from that architecture.

The ask: "scope out integrating Steam… I want all the same great capabilities we built for
Ubisoft Connect." Quiver already reads and writes a lot of Steam's local state for Steam
copies of Ubisoft games (`design/STEAM-COPIES.md`, `crates/quiver-core/src/exports/steam*.rs`,
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
| C1 | Accounts: password + 2FA login, remember-me refresh, one keyring entry per account, several accounts, default and per-game launch account | `quiver-core/src/auth.rs`, `store.rs`; `Request::Login/Login2fa/Logout/AccountDefault` |
| C2 | Library: ownership sync, product configs, merged across accounts; art chain (config → games API → Steam cache → Steam CDN → custom); game info, add-ons, links | `library.rs`, `gameinfo.rs`, `spaces.rs`, `quiver-api/src/art.rs` |
| C3 | Install pipeline: one queue (pause, resume, reorder, bandwidth limit, max active, auto-update), free-space check with margin, slice-level resume, update preview, verify and repair, import of existing installs (reflink), uninstall with or without prefix. **Move is designed, not built** (no `Request::Move`) | `install.rs`, `download.rs`, `verify.rs`, `quiverd/src/downloads.rs` |
| C4 | One Proton prefix per game; Proton choice; settings with revision history, restore, actor labels | `launcher.rs` (`list_protons`), `settings.rs`, `SettingsSet/History/Restore` |
| C5 | Licensed launch without Connect: `quiver-bridge` owns the SDK pipes; the broker gets Denuvo and ownership tokens | `quiver-bridge`, `broker.rs`, `launch.rs` |
| C6 | Cloud saves (pull before, push after), backups, save stores, merge, restore with approvals | `cloudsave.rs`, `savesync.rs`, `saves.rs`, `savestores.rs` |
| C7 | Achievements and progression, read-only from UbiServices. Earning is logged by the bridge but not reported (`EarnAchievementRsp` OK) | `progress.rs`, `quiver-bridge/src/win/api.rs` |
| C8 | Play time: reported to Ubisoft (heartbeat every 330 s), local `playtime.jsonl` with settings revision, `RemotePlaytime` | `playsession.rs`, `settings.rs` |
| C9 | DLC / add-ons: owned and not owned, store links | `info.rs` (`GameInfo`) |
| C10 | Mods v2: layers, profiles, versions, config patches, per-profile prefix folders, fixes on vanilla, built-in audited catalog keyed by product id; MOD-CONFIG options (designed) | `mods.rs`, `modversions.rs`, `confpatch.rs`, `overlay.rs`, `mods/<product>/…` |
| C11 | Sight overlay and the pinned HUD (designed, not built) | `design/SIGHT.md`, `design/SIGHT-HUD.md` |
| C12 | Exports: `.desktop` files and Steam shortcuts (`shortcuts.vdf`, grid art), staged while Steam runs, restart only after confirmation | `exports/*` |
| C13 | Steam copies of Ubisoft games: in the library, `steam-run`, patch and unpatch, offers, launch settings applied inside Steam's launch | `steamrun.rs`, `steamplan.rs`, `exports/steamgames.rs`, `quiverd/src/steamlib.rs` |
| C14 | CLI and agents: everything is a daemon request with a source label, activity log, `confirmed` gates for destructive acts, `--json` | `quiver-api`, `quiver-cli`, PROPOSAL "What agents may do" |
| C15 | First run: Sign in, Storage, Import, Steam, Controller, Fixes, Theme | `quiver-app/src/ui/welcome.rs` (`Step`) |

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
- `steamrun` (pure) with `quiver-cli steamrun.rs` (I/O) rewrites Steam's launch command, and
  `SteamLaunchPlan` applies settings inside Steam's launch.

### 3.2 Generalising: store-agnostic interfaces Steam needs

Names are suggestions. STORES.md decides the real shape. The Steam column says how each is
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
`quiver-api/src/lib.rs`). A multi-store Quiver needs a store-qualified reference such as
`GameRef { store: "steam", id: "292030" }`, printed `steam:292030`. Ubisoft keeps bare ids
for compatibility, so `3539` means `ubisoft:3539` (`ubi:` is accepted as input). Export keys become `quiver:steam:292030[/profile]`.
Existing Ubisoft keys and their CRC-derived shortcut app ids must not change.

**One title, several stores.** A Ubisoft game bought on Steam is already *one* library entry
with `steam: Option<SteamGame>`. In general, an entry links store records
(`ubisoft:66088` with `steam:3751950`) and picks a *play route* per entry. For Steam copies of
Ubisoft games that route is "Steam launch, `steam-run`, Ubisoft bridge" (built). The Steam
provider's records replace today's Ubisoft-only Steam scan, and owned-but-not-installed
Steam copies can appear once signed in.

**Crates.** A `store-steam` crate (or `quiver-core::steam`, if the crate rename waits) holding
the local readers (moved from `exports/` with re-exports), the `appinfo` and stats parsers,
the URL driver, and later `steam-vent` sign-in and the depot installer behind a cargo
feature. That keeps the CM stack out of builds that don't want it. `quiverd` hosts the
provider. `quiver-bridge` stays Ubisoft-only.

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
- **Agents.** Sign-in and sign-out are Deny (as for Ubisoft; PROPOSAL "What agents may do").

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
- Identity: `steam:<appid>` refs across `quiver-api` and the CLI. Ubisoft ids unchanged.
- The existing Steam copies merge into the same entries (no regression to Ubisoft-on-Steam).
- First-run Steam step: show Steam games yes or no, which user.

**Phase 2: Quiver around Steam's launch.**

- Generalised `steam-run` (opt-in per game): launch settings and revisions, local sessions,
  markers (`QUIVER_*`, scope, Hyprland tag), Sight and HUD env. **Built (section 9), except the
  scope and Hyprland tag markers.**
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
   compatibility. *Recommendation: yes; settled in STORES.md section 2.*
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

    **Decided 2026-11-17: (a).** The owned library waits for Quiver's Steam sign-in (phase 3); no Web API key, no "previously played" hints.
    (b) asks users to make profiles public and manage a key, which PROPOSAL avoided
    (SteamGridDB was dropped for the same reason).
11. **Own-Proton launches outside Steam's launch** (spike S6), with the client still
    running. *Recommendation: no* unless S6 shows play time and overlay intact. There is
    nothing to gain over `steam-run`.
12. **Agent policy for Steam.** *Recommendation:* install, verify and play through Steam:
    Allow. Patch or unpatch launch options and Proton mapping: Allow (reversible, staged).
    Uninstall: Ask. Restart Steam: Ask (as decided). Steam sign-in: Deny.
    *Revised 2026-11-18:* uninstall and other game changes are Allow for agents (they act like
    the person; PROPOSAL.md, "What agents may do"). Restart Steam and sign-in are unchanged.
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

## 8. As built: phase 1, local mode (2026-11-17)

**What it shows.** Every installed Steam game, from every library in `libraryfolders.vdf`, Proton or
native, shows in Quiver's library. Each one has:

- Steam's art: the library cache, then its public CDN.
- Install size and update state from the manifest, and installed DLC (`InstalledDepots/*/dlcappid`).
- Name, type, platforms, developer, publisher and release date from `appinfo.vdf`.
- Play time, two-week time and last played from `localconfig.vdf`.
- Achievement counts and the list with unlock times, from the stats cache.
- Steam Cloud state from `remotecache.vdf`: files, size, last upload and unsynced files.

**What Quiver does with them.** Play runs `steam://rungameid/<appid>` through the client.
"Running" is read from Steam's `reaper SteamLaunch AppId=<id>` processes.

**Safety.** No Steam login, no Steam secret, no write to any Steam file, no signal to Steam. From
`localconfig.vdf`, `steamlocal::playtime` keeps only `Playtime`, `Playtime2wks` and `LastPlayed`.

**Where the code is.**

- `crates/quiver-core/src/steamlocal/`: `appinfo` (v27–v29, string-table keys), `stats`, `playtime`,
  `remotecache`, `kv` (a binary KeyValues reader with a nesting limit), `is_tool`, and `provider`
  (`SteamLocal`, which caches a scan for 15 s; `refresh` rescans).
- `AppManifest` (`exports/steamgames.rs`) now also reads `SizeOnDisk`, `LastPlayed`, `StateFlags`,
  `TargetBuildID` and installed DLC.

**Accounts (`ClientManaged`).** Each game's play time, achievements and cloud state are read for
the Steam user who installed it (`LastOwner`), else the client's most recent user. The name comes
from `loginusers.vdf`, as `Steam::users` already did.

**Tools.** Tools and runtimes are not listed:

- appinfo type `tool` or `config`;
- Steamworks Common Redistributables, Steam Linux Runtime 1.0–4.0 and Proton 6.3–11.0,
  Experimental and Hotfix, by app id;
- when appinfo has no entry, by name.

Applications such as Wallpaper Engine stay listed.

**Steam copies of Ubisoft games stay one entry.** The Ubisoft provider's entry carries the Steam
install in `steam`, and `merge_libraries` drops the Steam provider's record for that app id
(steer 7). With Ubisoft signed out, the same app shows as a Steam game.

**Checked against this machine, read-only.** All counts are aggregates.

- `appinfo.vdf` is v29: 2.9 MB, 1223 apps, decoded in 9 ms (release build).
- 25 app manifests: 11 tools and 14 games.
- All 14 games have appinfo, play time and capsule art. 13 have achievements.
- `GameDetail` works for all 14. 10 have cloud state and 9 have DLC lists.
- A full scan takes about 120 ms in a debug build.
- Format notes from the parser work:
  - The schema's `type` appears as `"ACHIEVEMENTS"`, `"4"` or int 4.
  - Most games use the bit map's key as the bit; 9 games store an explicit `bit`.
  - The user stats file's root is `cache`.
  - `syncstate` was always 1 and `persiststate` always 0. The meanings of the other values are
    unverified.

**Not built yet.**

- Install, update, verify and uninstall through `steam://`, and progress mirrored into Downloads.
- Steam libraries in Storage.
- inotify change events. The scan is cached instead.
- The first-run Steam step.
- Flatpak Steam is found but untested.
- Description text: there is no local source for it.
- Achievement icons on the page. The detail carries CDN URLs, but the app doesn't draw them yet.
- Phase 2 onwards: `steam-run` for any game, settings and Sight are in section 9. Mods are not built yet.

**Not exercised live.** `quiver play steam:<appid>` was not run, because it would start a game.
Routing, refusals (not installed, still downloading, `--profile`) and the `rungameid` argv are
covered by tests and by the code shared with Steam copies.

## 9. As built: phase 2, any Steam game through `steam-run` (2026-11-17)

**What it does.** A Steam game whose launch options run `quiver steam-run %command%` gets
Quiver's launch plan from quiverd, the same as a Quiver launch of a Ubisoft game, as far as it
can ride on Steam's command:

- its Quiver launch settings: `env`, `wrappers` (in front of Steam's command), `args` (after
  the game's own) and DLL overrides (merged into Steam's `WINEDLLOVERRIDES`). `proton` and
  `language` are reported as not applied, because Steam picks them;
- Quiver Tweaks at the global and game scopes (Steam games have no profiles), compiled by the
  same code as a Quiver launch (`quiver_core::tweaks::plan_for`);
- the HUD (the Sight layer in every hooked launch since 2026-11-18, hidden until Shift+F3;
  not for games with anti-cheat in their folder, SIGHT-HUD §1.8): the perf
  session, its `/dev/shm` session dir and `hud.image`, `QUIVER_SIGHT=1` and the explicit
  `VK_INSTANCE_LAYERS` order (`perf/launch.rs`, ABOVE_SIGHT), from the same `PerfHub::begin`;
- a local play session: it shows in `quiver sessions` (`game: "steam:<appid>"`), keeps
  quiverd from restarting while the game runs, and goes into
  `games/steam-<appid>/playtime.jsonl` with its settings and tweak revisions and the perf
  summary when it ends.

Nothing of Steam's command is swapped. Ubisoft games' Steam copies take the path they took
before (the bridge is swapped in) and their launch plan request is byte-identical on the wire.

**How a launch goes** (`crates/quiver-cli/src/steamrun.rs`, `quiver_core::steamrun::route`):

1. Route: `--product` means a Ubisoft copy. `--app <appid>` means a plain Steam game, and no
   product is looked for. With neither, a Proton launch whose install script or prefix names a
   Ubisoft product is a Ubisoft copy, as a hand-typed `quiver steam-run %command%` always was.
   Anything else, Proton or native, is the Steam game in `SteamAppId`.
2. `Request::SteamLaunchPlan { game: "steam:<appid>", perf: true, env }`. `env` is the part of
   Steam's environment the plan needs: the keys under a fixed list of prefixes (`DXVK_`,
   `VKD3D_`, `PROTON_`, `WINE`, `VK_`, `ENABLE_`, `DISABLE_`, `LSFG`, `NVPRESENT_`, `MANGOHUD`,
   `OBS_VKCAPTURE`, `__GL_`, `NGX_`, `SL_`, `PROTONFIXES_`). Nothing else leaves the process.
   With it, tweaks yield per key to what the person typed before `%command%` (their
   `DXVK_CONFIG` is kept and Quiver's entries go after it), and the Sight layer goes below a
   vkBasalt or lsfg-vk they turned on there.
3. `Request::SteamSession { app_id, pid, perf_session, settings_revision }` with `steam-run`'s
   own pid. quiverd opens a pidfd on it.
4. `exec` of Steam's command with the plan applied. The pid stays the same, so it is now Steam's
   reaper (or whatever Steam put first in `%command%`). When it exits, the pidfd turns readable
   and quiverd ends the session and the perf session.

Steam's process tree is the one it would be with `gamemoderun %command%`: Steam's Stop, its
overlay (`LD_PRELOAD`, which a setting is appended to rather than replacing) and Steam Input
are untouched. If quiverd can't be reached, the game starts unchanged. `steam-run` never starts
quiverd itself (it would be inside Steam's tree): only through its systemd unit.

**The hook** (`quiver steam hook <game> on|off|status`; `Request::SteamHook`,
`Request::SteamHookStatus`; `quiverd/src/steamhook.rs`, `steamgames::{hook, unhook,
hook_status}`):

- `on` puts `<quiver> --source steam steam-run --app <appid>` in front of `%command%` in
  `localconfig.vdf`'s `LaunchOptions` for the Steam user who installed the game. The person's
  own options are kept around it, and the edit is byte-preserving (`textvdf::set_string`).
  This is the same staged machinery as the Connect switch: while Steam runs, the change waits
  in `steam-games.json` and is written once Steam is seen closed, by `quiver steam restart`
  (asks first) or by `quiver steam apply`.
- `off` restores exactly what was there. When the hook added the `LaunchOptions` key, or the
  app's whole section, that is taken out again (`textvdf::remove`) instead of being left
  empty. If Steam has written more into a section the hook created, only the key goes.
- A Ubisoft game's Steam copy gets the Connect switch itself (`SteamPatch` with `--product`),
  so `quiver steam hook` and `quiver steam patch` do the same thing for it.
- Refused (with the reason) for Flatpak Steam, a quoted `%command%`, no `quiver` found, or
  several Steam users with none chosen (`--user`).

**Saying it plainly.** For a Steam game that isn't hooked:

- CLI: `quiver set` and `quiver get`, `quiver hud enable`, and `quiver tweaks set` or
  `preset` print that Steam starts the game without Quiver, and give the command that hooks
  it. `quiver tweaks plan` carries the same note; hooked, the note says
  the tweaks apply to its Steam launch.
- App: the Steam game's Overview has a *Quiver launch* card. It shows the hook state and its
  switch ("Start through Quiver" or "Stop starting through Quiver"), the launch options, and
  whether the HUD is on, with its own switch. The Tweaks tab's banner says whether the tweaks
  reach the game, with the same switch. Saving a tweak, or turning the HUD on, for a game
  that isn't hooked shows a toast with the offer. `GameDetail.steam_hook` carries the state,
  and the page reloads it on `SteamGamesChanged`.

**Identity on the wire** (additive; Ubisoft JSON unchanged):

- `ActiveSession`, `PerfSessionInfo`, `HudStatus`, `PerfEvent::Session` and `SteamLaunchPlan`
  gain `game` for other stores, with `product_id` 0.
- `HudScopeRef` and the perf requests that named a product (`Catalog`, `Subscribe`,
  `HudToggle`, `HudNext`, `HudStatus`, `LaunchEnv`, `Summaries`) take a game. A Ubisoft game
  is still written as `product_id`.
- `SettingsGet`, `SettingsSet`, `SettingsHistory` and `SettingsRestore` take `GameField`.
- `hud/settings.json` and `state.json` key games by `GameRef::key` (`3539` as before,
  `steam:620`).
- The perf hub's sessions, its Steam launches and `HudSettings::for_launch` are keyed by
  `GameRef`.

**Verified here** (no real Steam file written, no Steam started or stopped, no game run):

- Unit tests: route selection, `steam_command` and `steam_env` (overlay preload kept, DLL
  overrides merged), the env allowlist, hook and unhook on fixture trees (keeps the user's
  options; byte-exact for an added key, an added section, and a section Steam grew; staged
  while running, then written; a quoted `%command%` refused), `textvdf::remove`, HUD scopes
  for Steam games with Ubisoft keys unchanged, perf sessions for Steam games with layer order
  from Steam's environment, pidfd exit, local session records, and the demo's hook.
- End to end (`crates/quiver-cli/tests/steam_run.rs`), against a scratch quiverd (own XDG
  dirs, HOME with a fixture Steam tree, `QUIVER_SHM_ROOT`, a do-nothing `systemctl` first on
  `PATH`):
  - settings, HUD and a tweak for `steam:870780`, each saying the game isn't hooked;
  - the hook's status, then on and off, back byte for byte. A Steam client was running on this
    machine, so this run took the staged branch; the written branch is covered by the unit
    tests;
  - a launch through `quiver steam-run --app 870780` of a shell script standing in for
    Steam's command. Arguments, wrapper, settings env, `QUIVER_SESSION`, `QUIVER_SIGHT`,
    `QUIVER_PERF_DIR` under the scratch root, `VK_INSTANCE_LAYERS` below the person's
    vkBasalt, the tweak's `DXVK_CONFIG` and Steam's own variables were all recorded by it;
  - `quiver sessions` and `hud status` while it ran, and the session's record, with the perf
    session and its shm dir gone, after it exited;
  - with quiverd down, the game started unchanged and no daemon was started.
- `--dry-run` for both routes. A Ubisoft copy's dry run (Black Flag Resynced, the known
  table) is unchanged.

**Steers taken** (not covered by the docs; the conservative choice each time):

- **G1.** `exec`, not spawn-and-wait. The play session follows the pid through a pidfd, so
  Steam's process tree, Stop and signal handling are exactly Steam's. Cost: the namespace
  child mode that mod overlays need (STEAM-COPIES §4) will have to spawn. That waits for the
  mods work and its S1 checks. *Decided 2026-11-17: keep `exec` until the mods work needs spawning.*
  *Built 2026-11-17 (§10, M2): spawn only when a launch has overlays; `exec` otherwise.*
- **G2.** `--app` means "a plain Steam game": nothing is looked up, so a hook written for a
  game never turns into a bridge launch later. Bare `quiver steam-run %command%` still
  detects Ubisoft products.
- **G3.** If quiverd can't be reached, the game starts as Steam would start it. quiverd is
  never started from inside Steam's launch.
- **G4.** Only allowlisted prefixes of Steam's environment go to quiverd. *Decided
  2026-11-17: Ubisoft copies send it too,* so tweaks yield to a `DXVK_CONFIG` (or other key)
  typed in Steam's launch options for them as well, and their `LD_PRELOAD` is kept after
  Steam's.
- **G5.** An exact undo removes an added key or section. *Decided 2026-11-17: Ubisoft
  patches use it too.* Patches recorded before that (no `added`) are undone as before.
- **G6.** A launch-setting `LD_PRELOAD` is appended after Steam's, so the overlay always
  loads.
- **G7.** Local sessions are local only: nothing is reported to Steam or anyone. `quiver
  playtime steam:<id>` keeps Steam's totals, because Steam counts hooked launches too, and
  lists Quiver's recorded sessions.
- **G8.** The offer is a line in the CLI, and a toast and banner in the app. Nothing hooks a
  game unasked. There is no "declined" memory, as there is for Ubisoft install offers: the
  offer only appears right after the person changed something.
- **G9.** `account` and `profile` settings are refused for Steam games. `proton` and
  `language` are stored but reported as not applied. *Lifted for `profile` (§10, M1): for a
  Steam game it sets `steam_profile`, the profile Steam's Play runs.*
- **G10.** A quiverd crash during a session loses that session's record. Restarts onto new
  builds wait for running games, local sessions included.

**Spikes that need you** (each touches the real Steam client; agents don't run them):

1. **S1 + TWEAKS T6: the first real hooked launch.** CONTROL (app 870780), with the HUD and
   `PROTON_LOG=1`:
   1. Install this build (`tools/install-user.sh` after merge). Check that `quiver daemon
      status` shows quiverd on it.
   2. `quiver list --store steam` shows CONTROL; note its ref (`steam:870780`).
   3. Set it up:
      `quiver hud enable steam:870780`,
      `quiver set steam:870780 env.PROTON_LOG=1`,
      `quiver tweaks set steam:870780 tex.anisotropy=16 dlss.sr.model=K`.
      Each prints "isn't hooked".
   4. `quiver steam hook steam:870780 on`. With Steam running it is staged: quit Steam from its
      menu, wait about a minute (quiverd writes it once Steam is seen closed), or run `quiver
      steam restart`, which asks first. `quiver steam hook steam:870780` should say "starts
      through Quiver".
   5. Start Steam. CONTROL › Properties › Launch Options should read
      `<your quiver> --source steam steam-run --app 870780 %command%`.
   6. Play CONTROL from Steam. While it runs, check:
      - `quiver sessions` lists `steam:870780 … via Steam (app 870780) … local history`;
      - Shift+F3 shows Quiver's HUD, and `quiver hud status steam:870780` names the presenter
        (in the game means the Sight layer loaded inside pressure-vessel);
      - Shift+Tab opens Steam's overlay (and its FPS counter if you use it);
      - a controller works as before (Steam Input).
   7. Press Stop in Steam's library. The game should close, Steam should stop showing
      Playing, and `quiver sessions` should drop it.
   8. Send these:
      - `~/.local/share/quiver/steam-run/870780/steam-run.log` (the plan and the exec line);
      - the top of `~/steam-870780.log`, where Proton logs its environment. Look for
        `QUIVER_SESSION`, `QUIVER_SIGHT=1`, `VK_INSTANCE_LAYERS=…VK_LAYER_QUIVER_sight`,
        `DXVK_CONFIG=…samplerAnisotropy = 16` and `DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE=on`.
        Present means they survive pressure-vessel (T6). Missing means the Steam route needs a
        passthrough;
      - `quiver perf history steam:870780` and `quiver tweaks sessions steam:870780`.
   9. Undo:
      - `quiver steam hook steam:870780 off` (staged while Steam runs; written once it
        closes). The launch options should be back as they were, empty here;
      - `quiver set steam:870780 env.PROTON_LOG=`;
      - `quiver hud disable steam:870780`;
      - `quiver tweaks reset steam:870780 tex.anisotropy dlss.sr.model`.
2. **S1, a native Linux game.** Repeat steps 4–8 with a native game, such as Stardew Valley,
   with the HUD on. Native games have no Proton, so `steam-run` takes the route from
   `SteamAppId` alone. Check the HUD, Stop and the session record.
3. **S2: do `LaunchOptions` sync?** With the hook on, start Steam on a second machine signed in
   to the same account, and see whether CONTROL's launch options show the hook there. If they
   do, the hook needs a louder note (it points at this machine's `quiver`).
4. **Flatpak Steam** stays unsupported for the hook (`hookable: false` with the reason). There
   is nothing to try.

**Not built yet.**

- Mods and overlays through the namespace (the stretch goal, G1). *Built: §10.*
- Built-in fixes keyed by store id. *The catalog takes `mods/steam-<appid>/`: §10.*
- Tweak evidence on the Steam route: there is no per-session log dir, so effects stay
  "applied" (TWEAKS "Not built in phase 1").
- Launch settings and the HUD form for Steam games in the app (the CLI has both). The app's
  Steam page has only the HUD on/off switch.
- Gamepad focus for the new card's buttons.
- `SettingsChanged` for Steam games (none is sent; the app has no Launch options tab for
  them).
- A "next launch uses profile P" one-shot for pinned shortcuts (§7).
- Proton per game (`CompatToolMapping`, steer 4).
- Achievement toasts from the stats cache.

## 10. As built: phase 3, mods for Steam games (2026-11-17)

**What it does.** Any Steam game started through the hook (`quiver steam-run --app <appid>`)
runs Quiver's mods the way a Ubisoft game Quiver installed does: mod profiles (stacks of
versioned mods plus the person's overrides), install-file overlays (add, replace, delete),
config patches to the game's files in Steam's prefix (originals recorded and put back), the
mods' launch settings, and Options (rendered into the profile's install layer and patched into
the prefix). The rest of the launch plan (settings, tweaks, HUD, local play session) is as in §9.

- **Stores per game** (STORES §2.2, §8.1): a Steam game's mods, profiles, histories, options
  values and patch record live under `games/steam-<appid>/` and `mods/steam-<appid>/`
  (`GameRef::dir_name`). Ubisoft games' folders and files are byte-identical to before
  (`games/3539/…`, `mods/3539/…`; unit-tested).
- **The Steam game as a `Game`** (`quiverd/src/steammods.rs`): `install_dir` is Steam's
  (`<library>/steamapps/common/<game>`, from the app manifest, read-only); `prefix` is
  `compatdata/<appid>/pfx` for a Proton game and none for a native one (`Game::prefix()` is
  `None`, the field empty); `Game.game = steam:<appid>`, `product_id` 0. Without a prefix,
  config patches and prefix or Documents option files don't apply (said in the plan's
  warnings and the Options problems) and `profile_config` is a problem; install files work.
- **CLI:** `quiver mods steam:<appid> …` (or the game's name when no Ubisoft game matches):
  install, uninstall, version add/use/retire, profile create/set/delete/restore, default,
  plan, options, set, undo-patches, migrate. `quiver mods` lists Steam games that have a
  store. `quiver saves steam:<appid> …` says Steam games have no save stores, and why.
- **App:** a Steam game's page has the Mods tab (Options inside it, as for Ubisoft games;
  `Tab::STEAM`), loaded when Steam has the game installed and reloaded on `ModsChanged {
  game }`. A banner at the top says whether the mods reach the game and offers "Start
  through Quiver" when it isn't hooked. Next to Play, a **"Steam runs: Vanilla ▾"** chip
  (shown once the game has profiles) sets `steam_profile`, as does "Use in Steam" on a profile
  card. When the game isn't hooked, the chip and its toast say mods apply once it starts
  through Quiver. Profile cards and the new-profile dialog say "Uses the game's real saves
  (Steam Cloud syncs them)…" and offer no save store. "Play this profile" and "Pin to
  library" are hidden for Steam games. The Steam store's capabilities report `mods: quiver`.
- **Built-in catalog by store id:** `mods/<game folder>/<mod>/quiver-mod.toml`, so
  `mods/steam-<appid>/…` next to `mods/<product>/…` (`build.rs` takes both forms). A Steam
  game's manifest names it in the new `games = ["steam:<appid>"]`; installing a manifest on
  another game is refused (`modpkg::check_game`). No Steam built-in ships yet (none audited).

**Which profile a Steam launch runs** (M1): `steam_profile=<id|vanilla>`, a per-game setting,
default vanilla. `quiver set <game> steam_profile=NAME`, `quiver mods <game> default NAME`
(for a Steam game `profile` *is* `steam_profile`: its only launch is Steam's) or the app's chip.
`quiver steam-run --profile NAME|vanilla` in the launch options overrides it for that launch.
A deleted default runs vanilla with a warning, as for Play. `steam_profile` exists for Ubisoft
games too, stored and reported as not applied until their Steam copies run mods (M7).

**How a launch goes** (`crates/quiver-cli/src/steamrun.rs`, `quiverd/src/steamlib.rs`,
`steammods.rs`, `quiver_core::steamns`):

1. `Request::SteamLaunchPlan { game: "steam:<appid>", perf: true, env, mods: true, profile }`.
   `env` also carries `STEAM_COMPAT_DATA_PATH` on this route (Steam sets it for Proton
   launches only), so the launch decides whether there's a prefix and where (M6).
2. quiverd picks the profile (`profile`, else `steam_profile`, else vanilla) and builds the
   same plan as Play (`mods::launch_plan`: mods v2 migration or built-in updates, stack,
   resolved settings, mounts, patches). With problems it answers them and logs "can't start …"
   to the activity log; `steam-run` then doesn't start the game (M3). Otherwise it renders
   options (`options_launch`, registering the options session under the launch id), applies
   the config patches to Steam's prefix or the profile's own copies (`Plan::apply_patches`,
   under the mod-store lock, before anything mounts), and creates the profile's prefix folders.
   The stack's launch settings replace the game's own in the plan, before tweaks and the HUD
   see them. The launch id is also the perf session's (one `QUIVER_SESSION`, M10). The
   activity log says "Steam launch of <name> with profile '<P>' (<id>) r<N>: <mods> (<why>;
   <mounts>)".
3. The reply's `mods` (`SteamMods`) lists the overlays. `steamns::start_for` decides:
   **exec** when nothing mounts (vanilla, config-only stacks: G1 as before), **namespace**
   when something does, **refuse** on problems (M2).
4. `Request::SteamSession { …, mods_launch }`: the local session takes the prepared launch's
   profile, mod versions and options revision (`ActiveSession.profile`).
5. Namespace: `steamns::namespace` uses the existing holder (`overlay::Namespace`: a `cat`
   holder unshares, maps uid/gid, mounts, is reaped), then `steamns::run` spawns Steam's
   command with `setns` into it in `pre_exec`, the working directory Steam gave (as seen
   inside), `PR_SET_PDEATHSIG(SIGTERM)`, and steam-run's process group. steam-run forwards
   SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1 and SIGUSR2 to it, waits, and exits with its
   status (the exit code, or the same signal re-raised) (M4). Steam's reaper, the runtime
   (pressure-vessel's bwrap binds the library recursively, so the overlay comes along), Proton
   and the game see the mods; Steam and everything else see the real install.
6. When the launch ends, quiverd records the session in `games/steam-<appid>/playtime.jsonl`
   with its profile, profile revision, mod versions, launch id and options revision, imports
   option edits made outside Quiver, then drops the session ("played <name> for N min").

**Saves and Steam Cloud** (M5). Steam Cloud syncs the game's real save folder. Every profile of
a Steam game uses it: no save stores, `saves=new` is refused with the reason, and a stack with
a mod that isn't save-compatible warns ("back them up before playing it"). The CLI (`mods
list`, `default`, `plan`) and the app say "uses the game's real saves (Steam Cloud syncs
them); profiles with saves of their own aren't built for Steam games yet". Forking saves needs
each game's save folder (per game, often not in the prefix) and an overlay of it inside the
namespace only, so Steam Cloud keeps seeing the real one; that's for later.

**Wire** (additive; Ubisoft games' requests and files unchanged):

- `ModsRequest` and `OptionsRequest` variants take `#[serde(flatten)] game: GameField`:
  `"product_id": 3539` for a Ubisoft game, as always, `"game": "steam:292030"` otherwise.
- `Game.game`, `GameSettings.steam_profile`, `ModManifest.games`, `ModsState.game`,
  `GameProfiles.game`, `SessionOptions.game`, `Event::ModsChanged.game` (Steam games: product
  0; options changes of a Steam game also send `ModsChanged` with `game`).
- `Request::SteamLaunchPlan { mods, profile }`, its reply's `mods: SteamMods { launch,
  profile, profile_name, revision, chosen_by, resolved, overlays: [SteamOverlay { target,
  lower, upper, work }], problems, lines }`, and `Request::SteamSession { mods_launch }`.

**Verified here** (no real Steam file written, no Steam started or stopped, no game run, the
person's quiverd and data untouched):

- Unit tests: Steam stores' folders, real saves, `saves=new` refused, a Steam game's patch
  applied to and undone in a fixture prefix, the mod's launch arguments, a native game (no
  prefix: install overlay only, patches warned), Ubisoft folders unchanged
  (`mods::tests::steam_games_have_stores_of_their_own`); the catalog by store id and
  `check_game` (`modpkg`); a Steam app as a `Game`, the prefix decided by the launch, the
  prepared launch claimed once (`steammods`); `steam_profile` not applied to Ubisoft copies
  (`steamlib`); spawn vs exec vs refuse (`steamns`); the wire forms (`quiver-api`).
- End to end (`crates/quiver-cli/tests/steam_mods.rs`; scratch quiverd, HOME with a fixture
  Steam tree, `QUIVER_SHM_ROOT`, a do-nothing `systemctl`): a mod installed with
  `quiver mods steam:292030 install DIR`, a profile, `mods default`, then `steam-run --app` of a
  script standing in for Steam's command, started in the install folder:
  - it saw the overlay: a replaced file, an added file, the working directory inside, its own
    mount namespace, and a nested `bwrap --unshare-user --ro-bind / / --bind <install>
    <install>` (pressure-vessel's shape) saw the replaced file too;
  - it was spawned (steam-run stayed the parent, its only child), the play session held
    steam-run's pid and the profile; the install was unchanged and the script's write went to
    the profile's upper layer; the config patch was in the fixture prefix and `patches.json`;
    the play history and activity log named the profile and mod;
  - `--profile vanilla`: exec (same pid, the host's mounts) and the patch put back;
  - SIGTERM to the launch's process group (Steam's Stop) and to steam-run alone each ended the
    command, and steam-run died of the same signal; `exit 7` came back as 7; SIGKILL of
    steam-run ended the command through the parent-death signal. Each time no process was
    left in the namespace and the session ended.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets` (no new warnings in
  touched files).

**Steers taken** (not covered by the docs; the conservative choice each time):

- **M1.** One setting, `steam_profile`, for "what Steam's Play runs"; for a Steam game
  `profile` writes it too (G9 lifted for `profile`; `account` is still refused). Stored as the
  profile id, names resolved by quiverd.
- **M2.** Spawn only when something mounts. Config-only stacks and vanilla keep `exec` (G1).
- **M3.** A profile that can't run (a mod lost its folder, a broken stack) doesn't start: steam-run
  exits with an error, the reason in `steam-run.log` and the activity log, as Play refuses. Not
  "start vanilla instead": that would run something other than what was picked.
- **M4.** The child stays in steam-run's process group (no `setpgid`), so Steam's group signal
  reaches it directly; steam-run forwards too (a group signal can reach the reaper twice,
  which it tolerates). `PR_SET_PDEATHSIG` is set only when the spawn runs on the main thread
  (the signal is tied to the forking thread).
- **M5.** Real saves for every Steam profile; nothing forks saves.
- **M6.** The prefix is the launch's (`STEAM_COMPAT_DATA_PATH`); without a launch (the Mods
  tab, `mods plan`) it's `compatdata/<appid>/pfx` when that folder exists. Native means none.
  Only the Steam route sends the variable, so a Ubisoft copy's plan request is unchanged.
- **M7.** Steam copies of Ubisoft games don't run mods yet: their route doesn't ask, `--profile`
  is logged as ignored, `steam_profile` is reported as not applied.
- **M8.** A Steam game's store is keyed by its Steam ref, so a Ubisoft game's Steam copy, if
  modded this way later, would have a store apart from the product's (STEAM-COPIES §4 planned
  to share the product's; to decide with M7).
- **M9.** A catalog manifest for a Steam game must name it in `games`; no Steam built-in ships
  until one is audited the usual way.
- **M10.** One id per launch: the options session, the perf session and `QUIVER_SESSION`.
- **M11.** Options of a Steam game apply at launch. Live changes find no addressable game process
  (quiverd knows only steam-run's pid, and Steam games have no SDK bridge), so they're saved
  for the next launch and say so.
- **M12.** Tweaks stay at the game and global scopes for Steam launches (no profile scope yet).

**Spikes that need you** (each touches the real Steam client or game; agents don't run them):

1. **S3: the first real modded launch, The Witcher 3 (`steam:292030`, Proton).** A test mod
   that changes one install file (the debug console) and one setting in Steam's prefix
   (subtitles), so both paths show in the game.
   1. Install this build (`tools/install-user.sh` after merge); `quiver daemon status` shows
      quiverd on it.
   2. Make the mod (in your home, not in Steam's folders):
      ```sh
      M=~/quiver-mods/w3-spike
      W3="$HOME/.local/share/Steam/steamapps/common/The Witcher 3"
      mkdir -p "$M/files/bin/config/base"
      cp "$W3/bin/config/base/general.ini" "$M/files/bin/config/base/general.ini"
      printf 'DBGConsoleOn=true\n' >> "$M/files/bin/config/base/general.ini"
      cat > "$M/quiver-mod.toml" <<'EOF'
      id = "w3-spike"
      name = "Spike: console, no subtitles"
      kind = "mod"
      save_compatible = true
      games = ["steam:292030"]
      description = "Quiver spike S3: the debug console (install file) and subtitles off (config patch)."
      [[patch]]
      file = "drive_c/users/steamuser/Documents/The Witcher 3/dx12user.settings"
      section = "Audio"
      set = { Subtitles = "false" }
      [[patch]]
      file = "drive_c/users/steamuser/Documents/The Witcher 3/user.settings"
      section = "Audio"
      set = { Subtitles = "false" }
      EOF
      ```
   3. `quiver mods steam:292030 install ~/quiver-mods/w3-spike`, then
      `quiver mods steam:292030 profile create Spike --layers w3-spike`, then
      `quiver mods steam:292030 default Spike` (it says whether the game is hooked, and that
      profiles use the real saves). `quiver mods steam:292030 plan` should show one mount over
      `…/common/The Witcher 3` and the two config patches.
   4. If it isn't hooked: `quiver steam hook steam:292030 on` (staged while Steam runs: quit
      Steam from its menu, or `quiver steam restart`, which asks first).
   5. Play The Witcher 3 from Steam. While it runs:
      - `quiver sessions` lists `steam:292030` with profile Spike;
      - Options › Audio shows subtitles off; the console opens (the key left of 1, or F2);
      - `pid=$(pgrep -f witcher3.exe | head -1); grep DBGConsole "/proc/$pid/root$W3/bin/config/base/general.ini"`
        prints the line (the game's view, inside pressure-vessel), and
        `grep DBGConsole "$W3/bin/config/base/general.ini"` prints nothing (the real file);
      - Shift+Tab opens Steam's overlay; a controller works (Steam Input).
   6. Press Stop in Steam's library: the game closes, Steam stops showing Playing, `quiver
      sessions` drops it, and `quiver playtime steam:292030` lists the session (profile Spike).
   7. `quiver set steam:292030 steam_profile=vanilla`, Play again: no console, subtitles back on
      (the original values are put back; a value you changed in-game since is left alone).
      Quit the game normally.
   8. Send: `~/.local/share/quiver/steam-run/292030/steam-run.log` (the plan, "mods: profile
      'Spike'", the mount line, "spawn (in the mods' namespace)", the exit), and the
      `quiver activity` lines for steam:292030.
   9. Undo: `quiver mods steam:292030 profile delete Spike`, `quiver mods steam:292030 uninstall
      w3-spike` (puts back any value still patched), and `quiver steam hook steam:292030 off` if
      you don't want it hooked. Also check: Steam's "Verify integrity of game files" finds
      nothing wrong (it runs outside the namespace and should never see the mod).
2. **S4: a native game, Cities: Skylines (`steam:255710`).** It has a compatibility folder, so
   whether it runs native or with Proton depends on its Compatibility setting. Repeat steps 3–6
   with a file-only mod (for example `files/` holding a copy of a file from its install folder
   with one byte changed, or a new file you can see from `/proc/<pid>/root`). Run it native
   (no Compatibility tool) and check `steam-run.log`: the plan says the game has no Wine prefix
   (no config patches), the overlay still mounts, the game starts and Stop ends it. Then, if you
   use Proton for it, once with Proton: the log names `compatdata/255710/pfx`.
3. **S5: `--profile` from the launch options.** With the hook on, edit The Witcher 3's launch
   options in Steam to `<your quiver> --source steam steam-run --app 292030 --profile Spike
   %command%` (Steam must be closed for Quiver to edit them; editing them in Steam's own
   Properties is fine). The profile runs whatever `steam_profile` says. Put them back with
   `quiver steam hook steam:292030 off` and `on`.

**Not built yet.**

- Mods for Steam copies of Ubisoft games (the bridge route; M7, M8).
- Save forks for Steam games (M5): each game's save folder, overlaid inside the namespace only.
- Live option changes for Steam launches (M11) and the tweaks' profile scope (M12).
- A Steam game in the built-in catalog (needs an audited recipe; M9).
- App: live options events for Steam games (`OptionsLive`, `OptionsOutcome`, `OptionsCrash`
  carry only `product_id`); gamepad focus for the Steam profile chip; Steam mods in the demo
  mode (its Steam game shows "needs an installed game"). Not yet seen on screen (spike S3
  can check the page too).
- Mods for other stores' games (`epic:…`, `gog:…`): the store keys are ready, the games aren't
  `Game`s yet.

## 11. As built: Steam's updates in Quiver (2026-11-18)

Steam installs and updates Steam games; Quiver writes none of Steam's files. What Quiver does
is show Steam's update and point at where to act on it.

- **Read** (`exports/steamgames.rs`, `AppManifest::update`): the app manifest's
  `StateFlags` (Steam's `EAppState` bits: 2 update required, 256 running, 512 paused, 1024
  started, 1048576 downloading, 2097152 staging, 4194304 committing, 131072 validating),
  `BytesToDownload`/`BytesDownloaded`, `BytesToStage`/`BytesStaged`, `TargetBuildID`,
  `UpdateResult` (non-zero: the last attempt failed) and `AutoUpdateBehavior` (1: only when
  launched). A first install still downloading (build id 0, not installed) isn't an
  update. The earlier `update_pending` treated 512 as "update started" and missed 256 and
  the download phases; it now covers them.
- **Shown:** `ClientInstall::update` (`quiver-api::stores::ClientUpdate`: queued,
  downloading, paused, installing, validating; bytes; target build; error), so the library
  entry's `update_available`/`client.update_pending`, the tile's tag ("Update", or
  "Updating" while Steam downloads), the Updates filter, the game page's pill ("Steam is
  updating it · 42% of 3.1 GB") and `quiver library` ("Update queued in Steam of 3.1
  GB") all say the same. `LibraryEntry::has_update` is the one rule front ends use.
- **Act:** Steam has no `steam://` URL that starts one game's update (there are
  `install`, `validate`, `uninstall` and `open/<page>`). So `quiver update steam:<appid>`,
  and **Open Steam downloads** on the game page, hand `steam://open/downloads` to the
  client (`steamlaunch::open`, the same route Play takes: the running client, or a newly
  started one in a scope of its own). Only when the manifest says an update is waiting;
  otherwise the answer is that Steam has none and checks for updates itself. The result is
  a finished Install job saying what Steam's manifest shows. Steam is never restarted or
  stopped. `quiver update steam:<appid> --preview` prints Steam's state without opening
  anything; `UpdatePreview` for a Steam game is refused with that explanation.
- **Not in Quiver's queue or auto-update:** Steam's own auto-update (and the per-game
  `AutoUpdateBehavior`) decides. Changing that setting means writing the manifest with
  Steam closed, which isn't built.
- **Checked:** unit tests on manifests with each state; the field names are the ones
  §2.2 verified present. The `steam://open/downloads` URL is Valve's documented
  `open/<window>` command; it hasn't been tried against this machine's client.

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
