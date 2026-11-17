# Epic Games Store in Quiver (scoping and design notes)

Status: research and design only (2026-11-16). Nothing here is built. This is one of five
parallel store scopes (STORE-STEAM, STORE-GOG, STORE-EA, STORE-META); the cross-store parts
(one title owned in several stores, the shared interfaces, library merging) belong to
STORE-META, and this document only proposes what Epic needs from them.

The ask: "scope out integrating Epic Games Store… I want all the same great capabilities we
built for Ubisoft Connect."

Conventions: **(verified)** means checked in code, in a local non-secret file or in a primary
source. **(unverified)** means inferred, community-documented or untested, with the spike that
would settle it. Nothing in this document came from signing in to Epic or reading a token.

---

## 1. Summary and recommendation

**Recommendation: implement Epic natively in Rust in a new `epic-core` crate, driven by
quiverd like the Ubisoft side, and don't shell out to legendary.** Use legendary (GPL-3.0) and
Heroic as the protocol reference and as a development oracle only. Consider the MIT `egs-api`
crate as a dependency for the HTTP calls and manifest parsing once it has been reviewed.
Everything else already in Quiver (one queue, per-game prefixes, mods, save stores, settings
history, activity, Sight) is store-agnostic in substance and only keyed by Ubisoft product id
today, so the main architecture work is a `GameKey` that names the store.

Five findings shape the design:

1. **Epic games need no bridge.** A Ubisoft game's SDK talks to Connect over local pipes, so
   Quiver had to build `ubi-bridge`. An Epic game's EOS SDK talks to Epic's servers itself,
   authenticated by a one-time **exchange code** that the launcher passes on the command line
   (`-AUTH_PASSWORD=<code> -AUTH_TYPE=exchangecode …`). Some games also take an ownership-token
   file (`-epicovt=`). So legitimate licensing on Epic means "mint a real exchange code from
   the user's session just before launch and pass the arguments EGL passes" (verified:
   legendary `core.py get_launch_parameters`). Achievements unlock, EOS cloud saves sync and
   DLC is checked inside the game, against Epic, with no help from the launcher.
2. **Ubisoft titles bought on Epic are mostly a key-redemption problem, and Quiver already
   handles them once they are redeemed.** Epic's catalog marks them
   `ThirdPartyManagedProvider = UbisoftConnect` with `partnerLinkType = ubisoft` and `GameID`
   set to the **Ubisoft product id**. legendary's `activate --uplay` claims and redeems the
   Ubisoft codes onto the Epic account's linked Ubisoft account. After that the Ubisoft account
   owns the product (`ownership.target_partner = TargetPartner_EpicGames`). Ubisoft's CDN
   serves it, and ubi-bridge, Denuvo, cloud saves and play time work unchanged. On this machine
   the Epic library includes five such titles: AC Origins (3539), AC Odyssey (5059), Far Cry 4
   (420), AC III Remastered (5183) and AC Syndicate (1875). Quiver already plays two of them,
   3539 and 420. Whether those copies came through Epic, and so whether Quiver already plays an
   Epic-redeemed copy with no Epic involvement, needs one read-only check: spike S1.
3. **Epic's APIs are undocumented but stable and well mapped.** Library, catalog, manifests,
   exchange codes, ownership tokens, cloud saves (`datastorage … /savesync`), achievements
   (store GraphQL), Ubisoft code redemption (store GraphQL) and play time (`library-service …
   /playtime`, PUT and GET) are all known from legendary, Heroic, egs-api and community endpoint
   docs. They all require posing as EGL's OAuth client, which legendary, Heroic and egs-api all
   do. That is the main ToS grey area (steer 2).
4. **Epic shipped a native Linux launcher preview on 2026-10-14.** It runs Windows games
   through a GE-Proton runtime it downloads, carries over cloud saves, and comes with an
   announced "EAC Linux v2" that needs signed kernels and secure boot with attestation, piloted
   in Fortnite. This doesn't change the recommendation, but it gives Quiver a Steam-style
   fallback: list games EGL installed, and play them through EGL, for anything Quiver can't run
   (steer 9).
5. **Anti-cheat is the hard ceiling.** EOS EasyAntiCheat and BattlEye games run under Proton
   only when the developer opted in, and only with the Proton EAC/BattlEye runtimes. Quiver can
   provide those runtimes the way Heroic does. EAC v2 and kernel anti-cheat titles are out of
   reach for any Proton launcher.

Effort, roughly: phase 1 (accounts, library, the Ubisoft-on-Epic view, activation) is **M**.
Phase 2 (native installer and launch, which makes Epic-only games playable) is **L**. Phase 3
(cloud saves, play time, achievements) is **M**. The store abstraction is **M**, spread across
phases 1 and 2.

---

## 2. What "the same capabilities" means (checked against the code)

The checklist below is what Quiver does for Ubisoft today, from the crates rather than the
roadmap. Each item says whether it's built.

| # | Capability | Where it lives | State |
|---|---|---|---|
| C1 | Accounts: several accounts, default and per-game launch account, 2FA, keyring sessions, "signed in elsewhere" | `ubi-core::{auth, store, online}`, `Request::{Login, Login2fa, Logout, AccountDefault}` | built |
| C2 | Library: ownership, install state, owners, update flag, art candidates (config → games API → Steam cache → CDN), custom art | `ubi-core::{library, spaces}`, `ubi-api::{art, LibraryEntry}` | built |
| C3 | Game info: publisher, links, languages, add-ons owned or not with store links | `ubi-core::gameinfo`, `Request::GameInfo` | built |
| C4 | Install, update (with preview), verify and repair, uninstall, import an existing install (reflink, checked against the manifest), first-run scan | `ubi-core::{install, download, verify, builds, manifest}`, `ubid::downloads`, `Request::{Install, Import, ScanInstalls, …}` | built |
| C5 | Queue: pause, resume, reorder, bandwidth, max active, free-space check, per-slice resume and retry | `ubid::downloads`, `Request::Queue*`, `DownloadSettings` | built |
| C6 | Move an install to another folder | PROPOSAL only | **not built** |
| C7 | One Proton prefix per game, Proton choice, launch settings (env, DLL, args, wrappers) with revisioned history | `ubi-core::{launcher, settings}`, `Request::Settings*` | built (prefix snapshots designed only) |
| C8 | Licensed launch: bridge in the prefix, Connect pipes, Denuvo tokens from the host broker, ticket and owned ids in `InitRsp` | `ubi-bridge`, `ubi-core::{broker, launch}` | built |
| C9 | Cloud saves: pull before, push after, `.upload` markers, backups, restore, save stores, merge | `ubi-core::{cloudsave, savesync, saves, savestores}` | built |
| C10 | Play time: local sessions, Ubisoft play-session reporting (heartbeat every 330 s), remote total | `ubi-core::playsession`, `Request::{Playtime, RemotePlaytime}` | built |
| C11 | Progression: achievement definitions (product-config zip), stats, challenges, rewards, Connect level | `ubi-core::progress`, `Request::GameProgress` | definitions built; **unlock state and unlock reporting not built** (the bridge logs `EarnAchievement`) |
| C12 | DLC: add-ons listed, owned ids passed to the game | `gameinfo`, bridge `InitRsp` | built |
| C13 | Mods: layers, packages, versions, profiles, overlayfs in a user namespace, config patches, built-in audited mods (`products = [id]`) | `ubi-core::{mods, modversions, modpkg, overlay, confpatch}`, `mods/<product>/` | built; MOD-CONFIG options designed |
| C14 | Exports: .desktop entries, Steam shortcuts, Steam copies started through Quiver (`steam-run`) | `ubi-core::exports`, `steamrun` | built |
| C15 | Sight overlay and the pinned HUD | SIGHT.md, SIGHT-HUD.md | **designed, not built** |
| C16 | CLI and agents: every action is a daemon request with a source label, activity log, `confirmed` gates for risky actions | `ubi-api`, `ubi-cli`, activity | built (scoped tokens and MCP designed) |
| C17 | First-run: install folder, import Connect installs, Steam step, fixes on vanilla | `ubi-api::setup`, app `welcome.rs` | built |

Everything in `ubi-api` is keyed by `product_id: u32` (`Request::Install { product_id }`,
`LibraryEntry.product_id`, mod manifests' `products = [420]`, `games/<product>/…` on disk).
That key is the one structural thing Epic forces to change (section 4.1).

---

## 3. Capability matrix

Effort: **S** is days, **M** is one to two weeks, **L** is several weeks. Risk is how likely
the approach breaks or can't be done.

| Cap. | Epic approach | Prior art | Effort | Risk |
|---|---|---|---|---|
| C1 Accounts and auth | OAuth with EGL's client: browser login, the user pastes the `authorizationCode` (or a later WebKit helper captures it), then refresh tokens. Several Epic accounts. Linked Ubisoft account read from `externalAuths`. Section 5 | legendary `auth`, Heroic webview, egs-api `auth_code` | M | **Med**: client-id impersonation, Epic changing login (it did in 2022, from SID to auth code) |
| C2 Library and art | `library-service …/library/api/public/items?includeMetadata=true` (cursor pages), then catalog `…/bulk/items` per namespace. Art from `keyImages`: `DieselGameBoxTall` (tile), `DieselGameBox` (hero), `DieselGameBoxLogo` (logo). Filter out UE assets and mobile-only items | legendary `get_game_and_dlc_list`, Heroic `library.ts` | S–M | Low |
| C3 Game info | catalog: title, developer, description, `releaseInfo`, `customAttributes` (`CanRunOffline`, `CloudSaveFolder`, `FolderName`, `AdditionalCommandLine`, `OwnershipToken`, `ThirdPartyManaged*`). DLC from `dlcItemList`, store link from the title slug | same | S | Low |
| C4 Install, update, verify, uninstall, import | Native **chunked-manifest downloader**: assets/v2 manifest API → binary manifest (magic `0x44BEC00C`, zlib, sometimes AES-GCM with API-provided secrets) → chunks (`ChunksV4/<group>/<hash>_<guid>.chunk`, magic `0xB1FE3AA2`, zlib, 1 MiB windows) assembled into files. Per-file SHA-1 for verify. Updates diff file hashes and reuse old files' data; delta manifests when offered. Install tags (selective downloads). Import from legendary/Heroic `installed.json` and EGL `.item` manifests, verified against the manifest | legendary `downloader/mp`, `models/manifest.py`, egs-api `asset_download_manifests` | **L** | Med: manifest v22+ chunk paths, encrypted builds, prereqs |
| C5 Queue | Same `ubid::downloads` queue behind a store-agnostic `Installer`. Resume unit is per file plus per chunk instead of per slice. Same retry and free-space rules | Quiver's queue | M | Low |
| C6 Move | Store-agnostic (move the folder, rewrite the install record, re-point overlay mounts). Epic itself needs nothing; build it once for all stores | – | S (shared) | Low |
| C7 Prefixes | Unchanged launcher. The bridge becomes optional per launch. Prereqs from the manifest (`prereq_path`, `prereq_args`) run once per prefix with a marker. EOS overlay registry entry only when turned on | Heroic `prepareWineLaunch` | S | Low |
| C8 Licensed launch | `LaunchProvider::Epic`: mint an exchange code (`GET …/oauth/exchange`) just before start. Add `-AUTH_LOGIN=unused -AUTH_PASSWORD=<code> -AUTH_TYPE=exchangecode -epicapp=<app> -epicenv=Prod -EpicPortal -epicusername= -epicuserid= -epiclocale= -epicsandboxid=<namespace>`, plus `-epicovt=<file>` when `OwnershipToken=true` and `-epicdeploymentid=` from the sidecar. Offline with an empty token only when `CanRunOffline` | legendary `get_launch_parameters`, Heroic `launch()` | S–M | Low for EOS-auth games; per-game quirks (legendary keeps `game_overrides`) |
| C8u Ubisoft-on-Epic | Redeem once (section 4.4), then the existing Ubisoft path. Optional Epic co-license arguments if spike S2 shows a game wants them | legendary `activate --uplay`, `get_ubisoft_uri` | S–M | **Med**: unverified whether any title checks Epic at run time |
| C9 Cloud saves | Two kinds. (a) **EGS launcher saves** (`CloudSaveFolder` set; 47 of the 120 titles in this machine's Epic catalog cache): `datastorage …/api/v1/access/egstore/savesync/<account>/<app>/`. Saves are manifests of chunks named by UTC time, resolved through path variables (`{AppData}`, `{UserDir}`, `{UserSavedGames}`, `{InstallDir}`, `{EpicID}`) inside the prefix. Pull before, push after, Quiver backups and save stores. (b) **EOS Player Data Storage**: in-game, nothing to do | legendary `sync-saves`, `savegame_helper.py`; Heroic `save_sync.ts` | M | Med: newer/older heuristic is mtime ±60 s; deletions |
| C10 Play time | Local sessions as today. Report through `PUT library-service …/playtime/account/<id>` (`artifactId`, `machineId`, start/end segments, as EGL does), periodically while running. Total from `GET …/playtime/account/<id>/all` | community endpoint docs; EGL behaviour | S | Med (unverified: segment rules, scope on the launcher token) |
| C11 Achievements | Read only. Definitions plus the user's unlocks from store GraphQL (`launcher.store.epicgames.com/graphql`, the `Achievement` queries legendary uses). **Unlocks are reported by the game through EOS**, so Quiver never reports them. Sight shows new unlocks by polling at intervals and at session end | legendary `get_game_achievements[_user]` | S–M | Med: GraphQL may change; titles with only EOS (non-Epic) achievements show nothing in the store |
| C12 DLC | Mostly entitlements that the game checks itself through EOS Ecom with the exchange-code login. Installable DLC is its own app installed into the base folder (`legendary install <dlc>`). Ubisoft-on-Epic DLC is redeemed like the base game | legendary `--with-dlcs`, `activate --uplay` DLC loop | S | Low |
| C13 Mods and Options | Unchanged, keyed by `GameKey`. Ubisoft-on-Epic titles keep their Ubisoft product id, so built-in mods (`mods/420/…`) apply. Refuse known DLC-unlocker packages | – | S (after the key change) | Low |
| C14 Exports | .desktop and Steam shortcuts keyed `quiver:epic:<app>/<profile>`. No Steam-copy equivalent | – | S | Low |
| C15 Sight and HUD | Store-agnostic features work as designed (session card, HUD, mods, saves status). Epic parity: achievement toasts (polled), play time. The EOS overlay can't be bridged (no local IPC to take over), so it stays an optional per-game switch | Heroic EOS overlay manager, legendary `lfs/eos.py` | S on top of Sight | Low |
| C16 CLI and agents | `quiver epic …` nouns, `--store epic` filters, the same `--json` and `--source` labels. Activation and sign-in are gated (section 7, steer 6) | – | S | Low |
| C17 First-run | Welcome gets a Stores step: sign in to Epic, import Heroic/legendary/EGL installs, offer to activate Ubisoft titles bought on Epic | – | S | Low |
| AC | Anti-cheat: set `PROTON_EAC_RUNTIME` and `PROTON_BATTLEYE_RUNTIME` from installed runtimes (Steam's, or downloaded as Heroic does from Lutris' runtime list). Flag EAC titles and turn off the Sight layer for them (SIGHT-HUD decision 16). EAC v2 and kernel anti-cheat: unsupported, and the app says so | Heroic `launcher.ts`, `wine/runtimes` | S | **High** for whether a given game works; out of Quiver's hands |
| Social | Presence, friends (`egs-api update_presence`, `account_friends`). Later | egs-api | M | Med |

---

## 4. Architecture

### 4.1 The key change: `GameKey`

```rust
/// Which store a title, install or setting belongs to. Serialized as "ubisoft:3539",
/// "epic:Camellia"; a bare number keeps meaning Ubisoft so old clients and files still read.
enum GameKey { Ubisoft(u32), Epic(EpicApp), /* Steam, Gog, Ea: siblings */ }
struct EpicApp { app_name: String }  // Epic's stable asset id ("Camellia"); namespace and
                                      // catalog item id are looked up from it
```

- `ubi-api` requests take `game: GameKey` where they take `product_id: u32` today. Serde
  accepts both (`3539` → `Ubisoft(3539)`), so the CLI, the app and older scripts keep working.
- On-disk paths move from `games/<product>/…` to `games/<store>-<id>/…` for new stores only.
  Ubisoft paths stay where they are, so nothing is migrated.
- Fuzzy names (`quiver play origins`) search every store. If a name matches copies in two
  stores, the result is the merged title (STORE-META), not an error.
- Mod manifests: `products = [420]` keeps meaning Ubisoft, and `games = ["epic:Camellia"]` is
  added.

### 4.2 Store-agnostic interfaces Epic needs (brief; STORE-META owns the final shape)

```rust
trait StoreAccounts   { login_begin, login_complete, refresh, logout, list, status }   // C1
trait LibraryProvider { titles(account) -> Vec<Title>, details(key), art(key) }         // C2, C3, C12
trait Installer       { plan(key, opts) -> Plan, run(plan, sink: &mut dyn Progress),
                        verify(key), repair(key), uninstall(key), adopt(key, path) }    // C4, C5
trait LaunchProvider  { prepare(key, account) -> LaunchLicense, on_exit(&LaunchLicense) } // C8
struct LaunchLicense  { args: Vec<Secret<String>>, env: Vec<(String, Secret<String>)>,
                        files: Vec<TempFile>, companions: Vec<Companion> /* ubi-bridge */,
                        refresh: Option<Duration> }
trait CloudSaves      { locate(key, prefix) -> Vec<SaveRoot>, status, pull, push }      // C9
trait Progression     { achievements(key) -> Achievements, unlocks_reported_by: Game | Launcher }
trait Playtime        { remote_total(key), report(Session) -> Lease }                   // C10
trait Linker          { pending(account) -> Vec<Redeemable>, redeem(items, confirmed) } // Epic → Ubisoft
```

The launcher composes `LaunchLicense`s: a Ubisoft title adds the bridge companion and its
`UPLAY_ARGUMENTS`, and an Epic title adds arguments and an `.ovt` file. A Ubisoft title bought
on Epic can, if S2 says so, get both. The `Secret` wrapper keeps exchange codes out of logs,
the activity log and the resolved command the app shows (`-AUTH_PASSWORD=<redacted>`).

### 4.3 Crates and processes

```
quiverd
  ├─ ubi-core (unchanged; implements the traits for Ubisoft)
  ├─ epic-core (new)
  │    api.rs        EGS hosts and calls; EGL client id/secret/UA/label from a refreshable config
  │    auth.rs       code login, refresh with rotation, exchange codes, ownership tokens
  │    library.rs    items + catalog + keyImages; ThirdPartyManaged detection
  │    manifest.rs   binary/JSON manifests, chunk lists, file lists, install tags, decryption
  │    download.rs   chunk fetch/decompress/assemble; reuse on update; verify; repair
  │    launch.rs     LaunchLicense for Epic; prereqs; EOS overlay registry; offline rules
  │    saves.rs      savesync manifests ↔ prefix folders
  │    progress.rs   achievements (GraphQL), playtime (library-service)
  │    link.rs       externalAuths, Ubisoft codes: list, claim, redeem
  └─ ubid::downloads (one queue; Installer trait per store)
```

- **No new long-lived process.** Epic needs nothing like the broker, because games talk to
  Epic themselves. quiverd holds the Epic session and mints codes on request.
  `LaunchContext`-style requests for containers and `steam-run` get an Epic variant that
  returns the arguments (codes are single use and short lived: mint one per launch).
- **egs-api vs own code.** egs-api 0.14 (MIT, Feb 2026) covers auth, library, assets, manifest
  fetching and parsing, exchange codes, ownership tokens and cloud-save listing. It has no
  achievements, play time or Ubisoft redemption, and is built around Epic Asset Manager.
  Recommendation: start `epic-core::api` on egs-api if a one-day review shows its manifest
  parser covers v22+ chunk paths and encrypted manifests and its client config can be
  overridden. Otherwise write a thin client of our own (the call surface is about 20 HTTP
  calls). Steer 3.
- **License hygiene.** legendary is GPL-3.0 and Quiver is MIT. Read it as a spec, but don't
  copy code into Quiver. Calling the `legendary` binary as a subprocess would be fine
  license-wise; the recommendation against it is about the architecture.

### 4.4 Ubisoft titles sold on Epic: how it composes with ubi-bridge

Evidence: this machine's Heroic legendary catalog cache (non-secret catalog JSON); legendary
`cli.py activate --uplay` and `core.py get_ubisoft_uri`; Heroic `games.ts installUbisoft`;
Ubisoft protos `ownership.proto` (`target_partner = 38`, `TargetPartner_EpicGames = 1`),
`common_settings.proto` (`message Epic { exchangeCode }`), `download_install_state.proto`
(`epic_run_installation`) and `UplayDll.proto` (`StoreInit.targetPartner`).

```
Epic entitlement (Camellia, partnerLinkType=ubisoft, GameID=3539)
   │  Linker: externalAuths → linked Ubisoft user id
   │          store GraphQL: accountUplayCodes → claim(partnerLinkId) → redeem
   ▼
Ubisoft ownership 3539 (target_partner = EpicGames)  ── one-time and irreversible; link is permanent
   ▼
everything Quiver does today: Ubisoft CDN install, ubi-bridge, Denuvo broker,
cloud saves, play-session reporting, mods keyed 3539
```

- **One library entry.** The title is `ubisoft:3539` with a provenance line, "Bought on Epic ·
  activated on <Ubisoft account>", like Steam copies' "Owned on Steam". If it isn't redeemed
  yet, the line reads "Bought on Epic · not on your Ubisoft account yet", with an **Activate**
  action. The Epic entry is folded into the Ubisoft one through `GameID` (STORE-META's
  merge rule; Epic gives the Ubisoft id directly, so no fuzzy matching).
- **The linked account matters.** The Epic account's linked Ubisoft account
  (`externalAuths[type=ubisoft].externalAuthId`) receives the codes. Epic's help says a link,
  once made, can only ever be re-made with the same Epic account. Quiver shows which Ubisoft
  account that is, and if it isn't signed in to Quiver, says so before redeeming. If no account
  is linked, Quiver opens `https://www.epicgames.com/id/link/ubisoft` in the browser; it never
  links on its own.
- **Do these titles also need Epic licensing at run time?** Unverified. Arguments for "no":
  legendary prints "can be activated directly on your Ubisoft account and does not require
  legendary to install/run"; Epic's catalog sets `NeverUpdate=true` (Ubisoft updates them);
  ownership says EpicGames as partner, which Ubisoft already resolved. Arguments for "maybe":
  EGL starts them through `uplay://launch/<GameID>?AUTH_PASSWORD=<exchange>&AUTH_TYPE=exchangecode&epicusername=…`,
  and Connect's settings proto has a slot for an Epic exchange code. That could be for
  sign-in-with-Epic or auto-activation, or a run-time check. Design: the Ubisoft
  `LaunchProvider` stays primary. If S1/S2 show a title wants Epic material, the launcher adds
  the Epic co-license (exchange-code arguments after `UPLAY_ARGUMENTS`, and, if the SDK asks,
  the bridge answers `StoreInit.targetPartner = EpicGames` from the ownership record instead
  of today's hard-coded `None`).
- **Epic-distributed Ubisoft builds** (files on Epic's CDN, Connect needed at run time, the
  `UplayLaunch.exe` executable case legendary checks for) may exist for older titles. None of
  the five here are like that. If one turns up: Epic `Installer` + Ubisoft `LaunchProvider` +
  Epic co-license. Mark these "unsupported until seen".
- **EA titles on Epic** (`ThirdPartyManagedApp = Origin`, one on this machine) work the same
  way with `link2ea://`. That's STORE-EA's.

### 4.5 What counts as legitimate licensing

| OK | Not OK |
|---|---|
| The user's own Epic account, signed in by the user | Using anyone else's tokens; sharing sessions |
| Exchange codes and ownership tokens minted from that session for titles it owns | Generating or replaying codes/tokens for unowned titles |
| Downloading builds the account is entitled to, and decrypting their manifests with the `secrets` Epic's manifest API returns for that entitlement | Fetching builds for unowned titles; decrypting anything by other means |
| Redeeming Ubisoft codes Epic issued to this account for games it bought | Redeeming codes for others, reselling |
| `CanRunOffline` titles launched offline (what EGL allows) | Forcing offline launches of titles that need the online check |
| Proton EAC/BattlEye runtimes as Valve/Epic ship them, for games whose developers enabled Proton | Bypassing, spoofing or disabling anti-cheat |
| Mods that change files of an owned game | EOS emulators, DLC unlockers (ScreamAPI/Koaloader-style); Quiver's catalog and package import refuse known ones |

**Grey: client identity.** Epic has no public OAuth client for third-party store launchers.
legendary, Heroic and egs-api authenticate as EGL's own client (`34a02cf8…`, with EGL's user
agent and build label, refreshed from legendary's remote config). The current cached config on
this machine says label `Live-HighlandWarrior`, EGL `15.18.2`, newer than legendary's built-in
defaults. Quiver would have to do the same. It isn't DRM circumvention, but it does impersonate
a client. Steer 2.

---

## 5. Auth details

**Login (MVP: browser plus a pasted code).** GPUI has no web view, and quiverd is headless.

1. `Request::EpicLoginBegin` returns the URL
   `https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D<EGL client>%26responseType%3Dcode`.
   The app opens it with `xdg-open` and shows a code field; the CLI prints it.
2. The user signs in on epicgames.com (password, 2FA, captcha: all Epic's pages). The redirect
   endpoint shows JSON containing `"authorizationCode"`. The user copies the code, or the whole
   JSON; Quiver extracts the code either way.
3. `Request::EpicLoginComplete { code }`: quiverd posts
   `grant_type=authorization_code, code, token_type=eg1` to
   `account-public-service-prod03.ol.epicgames.com/account/api/oauth/token` with the client's
   basic auth. It stores the result (below) and adds the account: `account_id`, `displayName`,
   and the linked Ubisoft id from `externalAuths`.
4. `errors.com.epicgames.oauth.corrective_action_required` (EULA, privacy policy, and so on)
   becomes a notification with Epic's `continuationUrl` and **Open in browser**. Quiver never
   accepts anything on the user's behalf.

Later options (steer 1): (b) a tiny optional `quiver-epic-login` helper using WebKitGTK that
captures the code from the redirect, as Heroic's web view does (Heroic redirects to `localhost`
and reads `code`). (c) Device-code flow: Epic supports it only for some first-party clients,
which would mean borrowing yet another client's credentials, so it's not recommended.
(d) `--token <exchange code>` for people who already have a signed-in EGL. Importing EGL's or
legendary's stored sessions is **not offered**: it reads credential files, and because refresh
tokens rotate, it would sign the other client out.

**Tokens and refresh.**
- The response carries `access_token` (an `eg1~` JWT), `expires_at`, `refresh_token` and
  `refresh_expires_at`. Lifetimes vary by client and flow (community docs show 4 h and 8 h;
  unverified), so Quiver uses the timestamps it receives and never hard-codes them.
- quiverd is the only holder. Before each Epic call, a single-flight refresh runs if fewer than
  10 minutes are left (legendary's margin). Refresh is `grant_type=refresh_token`. Epic returns
  a **new refresh token**: write it to the keyring before using the new access token, so a
  crash can't strand the account.
- `invalid_grant` or a refresh token past `refresh_expires_at` marks the account "Signed out
  of Epic: Sign in again", as a notification. It pauses that account's downloads, and launches
  of its titles ask to sign in (or run offline when `CanRunOffline`). There is no automatic
  re-login.
- Exchange codes (`GET …/oauth/exchange`) are minted per launch, used once, and never stored.
  Their exact lifetime is unverified (short, minutes). Ownership tokens
  (`POST ecommerceintegration …/ownershipToken`) are written to
  `<prefix>/quiver-run/<ns><item>.ovt` with mode 0600 and deleted when the session ends.
- Running games refresh their own EOS sessions (the SDK swaps the code for its own tokens), so
  there is no Ubisoft-style ticket refresh during long sessions.

**Where secrets live.**

| What | Where |
|---|---|
| Refresh token (and the cached access token, for fast daemon restarts) | Secret Service keyring, service `quiver`, entry `epic:<account_id>`, JSON. Same mechanism as `session:<userId>` for Ubisoft (`ubi-core::store`) |
| Account index: id, display name, default, linked Ubisoft id | `<data>/accounts.json`, an `epic` section (non-secret) |
| EGL client config (client id/secret, UA, label) | Compiled-in defaults plus `<data>/epic-client.json` override (non-secret, public values); steer 2 for how it updates |
| Exchange codes, ownership tokens | memory, and a 0600 temp file in the prefix for the duration of a session |
| Never | plaintext tokens on disk (legendary keeps `user.json` in plaintext), tokens in logs, activity, job details or `--json` output |

**Multiple Epic accounts:** supported like Ubisoft's (default plus per-game `account=`).
Concurrent sessions don't seem to kick each other the way demux does (legendary and EGL coexist
unless you import EGL's session); this is unverified, so it's covered by S0.

---

## 6. Phased plan

**Phase 0: spikes (the user runs or approves each; nothing signs in without them).**

| Spike | Question | How |
|---|---|---|
| S0 | Does code login with the EGL client work today, after the Oct 2026 EGL v2 preview? Does a Quiver session sign EGL or Heroic out? | User signs in through a throwaway `quiver dev epic-login` build; check Heroic still works |
| S1 | Is `target_partner = EpicGames` on 3539 / 420? If so, Quiver already plays Epic-redeemed Ubisoft games with no Epic involvement | `quiver ownership 3539`, `quiver ownership 420` (read-only, existing session) |
| S2 | Does any Ubisoft-on-Epic title (or its SDK) ask for Epic material at run time? | `tools/recon/sdk-proxy` log of Origins under the bridge: look for `StoreInit`/partner requests; compare an EGL-started run if the user is willing |
| S3 | Manifest and chunk parsing on a real current build (v22+ paths, encrypted manifest) | Download a small free owned title; compare file list and hashes with `legendary info --json` |
| S4 | EOS launch under Quiver's launcher: exchange-code arguments, `-epicovt`, prereqs, DXVK | Launch two owned titles, one with `OwnershipToken=true` |
| S5 | Cloud-save round trip and path variables in a Proton prefix | One `CloudSaveFolder` title: pull, play, push, compare in EGL/Heroic |
| S6 | Achievements GraphQL and play-time PUT/GET with the launcher token | Read-only GETs first; one PUT after a real session |

**Phase 1 (M): Epic accounts and library; Ubisoft-on-Epic.**
- `GameKey` in `ubi-api` (serde-compatible), `games/<store>-<id>` paths for new stores.
- `epic-core` auth (code login, refresh, keyring, several accounts), library and catalog,
  art, game info, DLC lists.
- Library: Epic titles with "Bought on Epic", install state "Not installed" (install arrives
  in phase 2; until then the button says so), Ubisoft-on-Epic folded into the Ubisoft entry.
- **Activation**: `quiver epic activate --ubisoft [--dry-run]` and the app's Activate action.
  Lists what will be redeemed and onto which Ubisoft account; always needs a person's
  confirmation.
- Import (record only, verified in phase 2): Heroic/legendary `installed.json` and EGL
  `Manifests/*.item` found in Wine prefixes.
- CLI: `quiver epic login|logout|accounts`, `quiver library --store epic`, `quiver info epic:<app>`.
- First-run Stores step.

**Phase 2 (L): Install and play Epic-only games (the MVP for Epic-only titles).**
- Native downloader in the queue: install, update with preview, verify and repair, uninstall,
  install tags, installable DLC, delta manifests, free-space check, resume per file and chunk,
  retries, bandwidth limit.
- Adopt imported installs by verifying against the manifest (reflink copy where possible).
- Launch: Epic `LaunchProvider`, bridge optional, prereqs once per prefix, offline rules,
  redacted resolved command, sessions and local play time, .desktop and Steam exports.
- Move (C6) for every store.
- Anti-cheat: EAC/BattlEye runtime detection and download; per-game anti-cheat flag.

**Phase 3 (M): Saves, play time, achievements.**
- EGS savesync into the existing save-store model. One cloud store per game, like Ubisoft's.
  Backups before pulls, restore, conflict view. EOS-storage games marked "saves sync in game".
- Epic play time: remote total as the headline (as decided for Ubisoft), report sessions.
- Achievements: definitions and unlocks in the app's Saves & progress tab, `quiver progress`.
- Optional EOS overlay per game (installed once into Quiver's data dir, registry `OverlayPath`
  in that prefix only).

**Phase 4 (S–M): Sight and polish.**
- Sight session card, HUD and toasts for Epic games (store-agnostic once Sight exists).
  Achievement toasts from polling.
- Presence and friends (later), EGL v2 fallback integration if steer 9 says yes.

---

## 7. Risks

1. **Client impersonation and ToS** (Med likelihood of friction, high impact). Every endpoint
   needs EGL's client. Epic has tolerated legendary/Heroic for years with no public
   enforcement, but nothing in writing allows it. Mitigations: never automate purchases or
   anything beyond what the user asks for; rate limits equal to or below EGL's; honest
   handling of corrective actions.
2. **API drift, sharper with EGL v2.** The new launcher (Oct 2026) may move to new clients,
   hosts or login flows. legendary's remote config absorbs drift centrally; Quiver needs its
   own updatable client config (steer 2) and S0 before building.
3. **Downloader correctness** (L effort). Chunk dedup across files, file reuse on update and
   encrypted manifests are where bugs live. Mitigation: legendary as an oracle in tests (file
   lists, SHA-1s), and fixture manifests.
4. **Anti-cheat.** Some popular Epic titles won't run, whatever Quiver does (EAC v2, kernel
   anti-cheat, developers who never enabled Proton). The app must say "won't run on Linux"
   rather than fail obscurely (a curated list plus EAC/BattlEye file detection in the install).
5. **Ubisoft-on-Epic run-time checks** (S2). If a title wants Epic material, the co-license
   path covers it, but it needs a real capture.
6. **Irreversible actions.** Ubisoft code redemption and account linking can't be undone.
   They are person-only actions, and agents get a pending approval at most (steer 6).
7. **Save safety.** EGS savesync decides newer or older by mtime and the manifest's UTC name.
   Quiver's backup-before-pull and save stores contain the damage; deletions aren't synced
   (as with Ubisoft).
8. **Unverified endpoints.** Play time (PUT segments), GraphQL achievements and Ubisoft-code
   GraphQL are community-documented. Ship them behind graceful errors (a failing endpoint
   becomes an `errors` entry, as `progress.rs` already does).

---

## 8. Open questions and steers

1. **Login UX.** (a) Browser plus pasted code only; (b) also an optional WebKitGTK helper that
   captures the code; (c) device-code flow with another Epic client.
   *Recommend (a) for phase 1 and (b) when it annoys; never (c).*
2. **Posing as EGL's client, and how its config updates.** (a) Compiled-in values plus a local
   override file, updated with Quiver releases; (b) also read legendary's public remote config
   (`egl_config`), which ties Quiver to legendary's service; (c) don't integrate Epic if
   impersonation is unacceptable.
   *Recommend (a); accept impersonation as the prior art does, and document it in the README.*
3. **egs-api or our own client.** (a) Depend on egs-api (MIT) for calls and manifests;
   (b) write our own thin client and parser; (c) shell out to legendary for installs.
   *Recommend a one-day review, then (a) if it covers v22+ manifests and config overrides,
   else (b). Not (c).*
4. **MVP scope.** (a) Phase 1 only first (accounts, library, Ubisoft-on-Epic activation),
   since five of the Epic titles here are Ubisoft games Quiver runs once they're on the
   Ubisoft account (two, 3539 and 420, are installed in Quiver already); (b) phases 1
   and 2 together, so Epic-only games are playable at first release.
   *Recommend (a), then 2 right after.*
5. **Library presentation.** (a) One merged library with store badges and a store filter;
   (b) a store switcher.
   *Recommend (a), consistent with Steam copies (STORE-META decides the merge rules).*
6. **Who may redeem Ubisoft codes and sign in to Epic.** (a) Person only, every time (a
   dialog or terminal confirm), agents denied; (b) agents may request it as a pending approval.
   *Recommend (b) for activation (it lists exactly what will be redeemed and onto which
   account), and (a) for sign-in and sign-out, which matches "Sign in: agents Deny".*
7. **Epic cloud saves on modded profiles.** Same as Ubisoft (one cloud store per game, others
   local)? *Recommend yes; no Epic-specific rules.*
8. **EOS overlay.** (a) Off by default, per-game switch; (b) on by default; (c) never.
   *Recommend (a): Sight covers the launcher-side features, the EOS overlay is in-process
   (another D3D hook next to mods like aco-dlss), and only some games need it (account
   portal, in-game purchases).*
9. **Epic's own Linux launcher (EGL v2).** (a) Ignore it; (b) read its installs and offer
   "Play through Epic's launcher" for titles Quiver can't run (as with Steam copies);
   (c) hand everything Epic to it.
   *Recommend (a) now, (b) once EGL v2 leaves preview and its install records are known.*
10. **Anti-cheat runtimes.** (a) Use Steam's installed runtimes when present, else download
    from Lutris' runtime list as Heroic does; (b) Steam's only.
    *Recommend (a), with the source shown in Settings › Compatibility.*
11. **Epic play-time reporting.** (a) On by default, like Ubisoft's `report_playtime`; (b) off
    by default because the endpoint is undocumented. *Recommend (a), with the existing switch
    covering both stores, once S6 confirms the PUT works.*
12. **Importing from Heroic/legendary.** Installs only (verified against manifests), never
    their sessions? *Recommend yes; reading their token files is off the table.*

---

## Sources

Local, read-only:
- Quiver: `crates/ubi-api/src/lib.rs` (`Request`, `LibraryEntry`), `crates/ubi-core/src/{store,auth,launcher,playsession,progress,savesync,broker,online}.rs`,
  `crates/ubi-bridge/src/win/api.rs` (achievements stub, `target_partner: None`), `proto/proto_ownership/ownership.proto`,
  `proto/proto_settings/common_settings.proto`, `proto/proto_download_install_state/download_install_state.proto`,
  `proto/proto_uplaydll/UplayDll.proto`.
- Heroic (`HeroicGamesLauncher` @ 7e7d7883, 2026-08-15): `src/backend/storeManagers/legendary/{games,library,user,thirdParty}.ts`,
  `…/legendary/eos_overlay/eos_overlay.ts`, `…/legendary/commands/*.ts`, `src/backend/save_sync.ts`,
  `src/backend/launcher.ts` (EAC/BattlEye runtimes), `src/backend/wine/runtimes/runtimes.ts`, `src/frontend/screens/WebView/index.tsx` (login capture).
- Heroic's legendary catalog cache on this machine (non-secret catalog metadata and cached remote
  config; `user.json` was not read).

Web:
- legendary (GPL-3.0, 0.21.1, 2026-09-08): <https://github.com/legendary-gl/legendary>, in
  particular `legendary/api/egs.py`, `legendary/core.py`, `legendary/cli.py`,
  `legendary/models/manifest.py`, `legendary/models/chunk.py`, `legendary/lfs/eos.py`,
  `legendary/utils/savegame_helper.py`, README (auth).
- egs-api (MIT, 0.14.0): <https://docs.rs/egs-api/latest/egs_api/struct.EpicGames.html>, <https://github.com/AchetaGames/egs-api-rs>
- Play time endpoints (community): <https://github.com/LeleDerGrasshalmi/FortniteEndpointsDocumentation/blob/main/EpicGames/LibraryService/Playtime/Add/Add.md>,
  <https://github.com/LeleDerGrasshalmi/FortniteEndpointsDocumentation/blob/main/EpicGames/LibraryService/Playtime/Info/All.md>
- OAuth token fields (community): <https://github.com/LeleDerGrasshalmi/FortniteEndpointsDocumentation/blob/main/EpicGames/AccountService/Authentication/Token.md>
- Exchange code is passed by the launcher as `AUTH_PASSWORD`: <https://forums.unrealengine.com/t/how-to-test-eos-lct-exchangecode/886227>
- EOS vs Epic achievements: <https://onlineservices.epicgames.com/news/adding-achievements-to-your-game> (sign-in wall; read through search summaries), <https://store.epicgames.com/news/the-epic-games-store-my-achievements-update>
- Ubisoft linking: <https://www.epicgames.com/help/en-US/epic-games-store-c-202300000001639/error-messages-c-202300000001736/this-profile-is-already-linked-to-another-user-error-when-linking-ubisoft-account-to-epic-games-account-a202300000012185>,
  <https://www.epicgames.com/help/c-202300000001639/c-202300000001735/i-can-t-launch-my-ubisoft-game-through-the-epic-games-launcher-a202300000012106>,
  <https://www.epicgames.com/help/epic-games-store-c-202300000001639/a202300000011441>
- EGL Linux preview and EAC Linux v2: <https://www.gamingonlinux.com/2026/10/epic-games-launcher-preview-arrives-native-linux-support-fortnite-for-linux-is-coming-and-upgraded-easy-anti-cheat/>,
  <https://www.gamingonlinux.com/2026/10/epic-games-launcher-preview-is-slowly-becoming-actually-usable-on-linux/>,
  <https://gamingonlinux.com/2026/10/epic-games-launcher-preview-updated-to-fix-logins-on-some-linux-systems>,
  <https://store.epicgames.com/news/epic-games-launcher-preview> (403 when fetched; read through search summaries)
- EAC on Proton background: <https://www.gamingonlinux.com/articles/16978/>; Heroic 2.4 (EOS overlay, anti-cheat runtimes): <https://www.gamingonlinux.com/2022/08/heroic-games-launcher-v240-is-out-with-gog-cloud-save-support>
- Epic's 2022 login change (SID → authorization code): <https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/issues/2531>
- Not reviewed in this pass: Rare (<https://github.com/RareDevs/Rare>), a Qt front end on legendary; it adds no protocol knowledge beyond legendary's.
