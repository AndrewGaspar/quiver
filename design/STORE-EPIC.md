# Epic Games Store in Quiver (scoping and design notes)

Status: designed 2026-11-16; steers decided and stage 1 (accounts, library, catalog, the Epic
provider, CLI and the app's sign-in) built 2026-11-17 (section 9 says what was
built and how it differs; section 8 has the decisions). The sign-in spike S0 passed
2026-11-18; phase 3 (cloud saves, play time, achievements, section 9.5), the library cleanup
and sign-in window (9.8, 9.9) and stage 2 (installs and launch, 9.10; the steps to try it in
9.11) were built 2026-11-18. This is one of five
parallel store scopes (STORE-STEAM, STORE-GOG, STORE-EA, STORE-META, the last being the Meta VR
store); the cross-store parts (one title owned in several stores, the shared interfaces, library
merging) belong to STORES.md, and this document only proposes what Epic needs from them.

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
   Quiver had to build `quiver-bridge`. An Epic game's EOS SDK talks to Epic's servers itself,
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
   serves it, and quiver-bridge, Denuvo, cloud saves and play time work unchanged. On this machine
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
| C1 | Accounts: several accounts, default and per-game launch account, 2FA, keyring sessions, "signed in elsewhere" | `quiver-core::{auth, store, online}`, `Request::{Login, Login2fa, Logout, AccountDefault}` | built |
| C2 | Library: ownership, install state, owners, update flag, art candidates (config → games API → Steam cache → CDN), custom art | `quiver-core::{library, spaces}`, `quiver-api::{art, LibraryEntry}` | built |
| C3 | Game info: publisher, links, languages, add-ons owned or not with store links | `quiver-core::gameinfo`, `Request::GameInfo` | built |
| C4 | Install, update (with preview), verify and repair, uninstall, import an existing install (reflink, checked against the manifest), first-run scan | `quiver-core::{install, download, verify, builds, manifest}`, `quiverd::downloads`, `Request::{Install, Import, ScanInstalls, …}` | built |
| C5 | Queue: pause, resume, reorder, bandwidth, max active, free-space check, per-slice resume and retry | `quiverd::downloads`, `Request::Queue*`, `DownloadSettings` | built |
| C6 | Move an install to another folder | PROPOSAL only | **not built** |
| C7 | One Proton prefix per game, Proton choice, launch settings (env, DLL, args, wrappers) with revisioned history | `quiver-core::{launcher, settings}`, `Request::Settings*` | built (prefix snapshots designed only) |
| C8 | Licensed launch: bridge in the prefix, Connect pipes, Denuvo tokens from the host broker, ticket and owned ids in `InitRsp` | `quiver-bridge`, `quiver-core::{broker, launch}` | built |
| C9 | Cloud saves: pull before, push after, `.upload` markers, backups, restore, save stores, merge | `quiver-core::{cloudsave, savesync, saves, savestores}` | built |
| C10 | Play time: local sessions, Ubisoft play-session reporting (heartbeat every 330 s), remote total | `quiver-core::playsession`, `Request::{Playtime, RemotePlaytime}` | built |
| C11 | Progression: achievement definitions (product-config zip), stats, challenges, rewards, Connect level | `quiver-core::progress`, `Request::GameProgress` | definitions built; **unlock state and unlock reporting not built** (the bridge logs `EarnAchievement`) |
| C12 | DLC: add-ons listed, owned ids passed to the game | `gameinfo`, bridge `InitRsp` | built |
| C13 | Mods: layers, packages, versions, profiles, overlayfs in a user namespace, config patches, built-in audited mods (`products = [id]`) | `quiver-core::{mods, modversions, modpkg, overlay, confpatch}`, `mods/<product>/` | built; MOD-CONFIG options designed |
| C14 | Exports: .desktop entries, Steam shortcuts, Steam copies started through Quiver (`steam-run`) | `quiver-core::exports`, `steamrun` | built |
| C15 | Sight overlay and the pinned HUD | SIGHT.md, SIGHT-HUD.md | **designed, not built** |
| C16 | CLI and agents: every action is a daemon request with a source label, activity log, `confirmed` gates for risky actions | `quiver-api`, `quiver-cli`, activity | built (scoped tokens and MCP designed) |
| C17 | First-run: install folder, import Connect installs, Steam step, fixes on vanilla | `quiver-api::setup`, app `welcome.rs` | built |

Everything in `quiver-api` is keyed by `product_id: u32` (`Request::Install { product_id }`,
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
| C5 Queue | Same `quiverd::downloads` queue behind a store-agnostic `Installer`. Resume unit is per file plus per chunk instead of per slice. Same retry and free-space rules | Quiver's queue | M | Low |
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

Reconciled in STORES.md section 2: the type is `GameRef { store, id }` with a string id, and
`EpicApp` stays a newtype inside `epic-core`. The forms below (`epic:Camellia`, bare numbers for
Ubisoft, `games/epic-<app>/`) are unchanged.

```rust
/// Which store a title, install or setting belongs to. Serialized as "ubisoft:3539",
/// "epic:Camellia"; a bare number keeps meaning Ubisoft so old clients and files still read.
enum GameKey { Ubisoft(u32), Epic(EpicApp), /* Steam, Gog, Ea: siblings */ }
struct EpicApp { app_name: String }  // Epic's stable asset id ("Camellia"); namespace and
                                      // catalog item id are looked up from it
```

- `quiver-api` requests take `game: GameKey` where they take `product_id: u32` today. Serde
  accepts both (`3539` → `Ubisoft(3539)`), so the CLI, the app and older scripts keep working.
- On-disk paths move from `games/<product>/…` to `games/<store>-<id>/…` for new stores only.
  Ubisoft paths stay where they are, so nothing is migrated.
- Fuzzy names (`quiver play origins`) search every store. Copies linked by a vendor-declared id
  (such as Epic's `GameID` for Ubisoft titles) are one merged title. Matches in unlinked stores
  are listed for the person to choose from (STORES.md section 2.4).
- Mod manifests: `products = [420]` keeps meaning Ubisoft, and `games = ["epic:Camellia"]` is
  added.

### 4.2 Store-agnostic interfaces Epic needs (brief; STORES.md owns the final shape)

```rust
trait StoreAccounts   { login_begin, login_complete, refresh, logout, list, status }   // C1
trait LibraryProvider { titles(account) -> Vec<Title>, details(key), art(key) }         // C2, C3, C12
trait Installer       { plan(key, opts) -> Plan, run(plan, sink: &mut dyn Progress),
                        verify(key), repair(key), uninstall(key), adopt(key, path) }    // C4, C5
trait LaunchProvider  { prepare(key, account) -> LaunchLicense, on_exit(&LaunchLicense) } // C8
struct LaunchLicense  { args: Vec<Secret<String>>, env: Vec<(String, Secret<String>)>,
                        files: Vec<TempFile>, companions: Vec<Companion> /* quiver-bridge */,
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
  ├─ quiver-core (unchanged; implements the traits for Ubisoft)
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
  └─ quiverd::downloads (one queue; Installer trait per store)
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

### 4.4 Ubisoft titles sold on Epic: how it composes with quiver-bridge

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
everything Quiver does today: Ubisoft CDN install, quiver-bridge, Denuvo broker,
cloud saves, play-session reporting, mods keyed 3539
```

- **One library entry.** The title is `ubisoft:3539` with a provenance line, "Bought on Epic ·
  activated on <Ubisoft account>", like Steam copies' "Owned on Steam". If it isn't redeemed
  yet, the line reads "Bought on Epic · not on your Ubisoft account yet", with an **Activate**
  action. The Epic entry is folded into the Ubisoft one through `GameID` (STORES.md's
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

Later options (steer 1): (b) a tiny optional helper using WebKitGTK that captures the code
from the redirect, as Heroic's web view does (Heroic redirects to `localhost` and reads `code`).
**Built** as `quiver-webview` (section 9.6); it is now the default, and pasting is the fallback. (c) Device-code flow: Epic supports it only for some first-party clients,
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
| Refresh token (and the cached access token, for fast daemon restarts) | Secret Service keyring, service `quiver`, entry `epic:<account_id>`, JSON. Same mechanism as `session:<userId>` for Ubisoft (`quiver-core::store`) |
| Account index: id, display name, default, linked Ubisoft id | `<data>/accounts.json`, entries with `store: "epic"` (non-secret; STORES.md section 3.2) |
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
- `GameKey` in `quiver-api` (serde-compatible), `games/<store>-<id>` paths for new stores.
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

**Decided (2026-11-17).**

| # | Steer | Decision |
|---|---|---|
| 1 | Login UX | **(a)** browser plus a pasted authorization code. Agents never sign in |
| 2 | Posing as EGL's client | **(a)** accepted, as legendary and Heroic do: compiled-in values plus `<data>/epic-client.json`, documented in the README |
| 3 | egs-api or our own | review it for a day, depend on it if it is good, else a thin client and parser of our own; never shell out to legendary; legendary and Heroic are GPL references only. Reviewed: **(b), our own** (section 9.1) |
| 4 | MVP scope | **(b)** phases 1 and 2 together (the user wants installs), staged: stage 1 (accounts, library) first, stage 2 (installs, launch) after the sign-in spike |
| 5 | Library presentation | **(a)** one merged library, store badges |
| 6 | Who may redeem and sign in | **(b)** for activation (a pending approval for agents); **(a)** for sign-in and sign-out (person only) |
| 7 | Cloud saves on modded profiles | as recommended: same as Ubisoft |
| 8 | EOS overlay | **(a)** off by default, per-game switch |
| 9 | EGL v2 | **(a)** ignored for now |
| 10 | Anti-cheat runtimes | **(a)** Steam's when present, else Lutris' list |
| 11 | Play-time reporting | **(a)** on, under `report_playtime`, after S6 confirms the PUT |
| 12 | Imports | installs only, verified against manifests; never their token files |

The options as they were put:

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
   *Recommend (a), consistent with Steam copies (STORES.md section 2.4 sets the merge rules).*
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

## 9. As built: stage 1, accounts and library (2026-11-17)

Nothing in this stage signed in to Epic, called Epic, or read another launcher's files. Every
Epic call goes through a transport seam, and the tests run the whole flow on recorded-shape
fixture JSON (invented ids).

### 9.1 egs-api review (steer 3): not used

egs-api 0.14.0 (MIT, released 2026-02-18; source from crates.io, about 10k lines) was read for
the four questions the steer asked:

| Question | Finding |
|---|---|
| Current manifests (v22+) | The binary parser reads the header, meta, chunk list and file list with `unwrap_or(0)` on every field, so a truncated or newer manifest parses into zeros instead of failing. The chunk-part reader computes `position - start - size` (backwards), which underflows (a panic in debug builds) as soon as Epic adds bytes to a chunk part. The header's stored flag is treated as "compressed" if non-zero, so an **encrypted manifest isn't detected**, let alone decrypted. Chunk paths stop at `ChunksV4` (correct today) |
| Chunk download | `Chunk::from_vec` indexes the buffer without bounds checks (panics on a short read of untrusted network data), ignores the header's size field, and doesn't check the chunk's SHA-1 or rolling hash. Whole chunks are held in memory; no streaming, resume or reuse |
| Client config overrides | None. EGL's client id and secret are string literals in two places, the user agent and a fixed `X-Epic-Correlation-ID` are hard-coded in the client builder |
| API quality, maintenance | One `EpicGames` facade with mutable session state; most calls have a `try_` form and a form that logs and returns `None`. Its session struct derives `Debug` with the tokens in it. It depends on `reqwest` 0.13 with default features (OpenSSL), next to the workspace's rustls `reqwest` 0.12. Six minor releases in four days in Feb 2026, nothing since (17 stars). Built for Epic Asset Manager: Fab, assets, cosmos |

Its endpoint list and JSON shapes were a useful cross-check (MIT). **Decision: (b), a thin
client and parser of our own** (`epic-core::api`, about 20 calls in all). The stage 1 surface
is six calls; the manifest parser and chunk downloader are stage 2, written against the
format with legendary as an oracle in tests (its code read, not copied).

### 9.2 What was built

- **`crates/epic-core`** (the STORES.md section 5 crate):
  - `config`: EGL's client id, secret, user agent and label compiled in;
    `<data>/epic-client.json` overrides any field (steer 2).
  - `http`: a `Transport` trait, `Reqwest` (rustls, 30 s timeout) and `Fake`. Requests carry a
    label, and only the label and status are logged. Errors from reqwest are reduced to their
    kind, because the URL can hold a token.
  - `api`: token (authorization code, refresh), end session, account, `externalAuths`, library
    items (cursor pages), catalog `bulk/items` (with DLC and main game). Epic's errors parse
    into `EpicError` (`invalid_grant`, corrective actions with `continuationUrl`). Their
    messages are scrubbed of token-shaped strings: Epic quotes the refresh token it refused.
  - `auth`: the sign-in URL; `parse_code` takes the bare code, the redirect page's JSON
    (whole, pretty-printed or partial) or a URL with `code=`, and explains
    `"authorizationCode": null` ("sign in on that page first"). `Tokens` use the reply's
    timestamps (or relative lifetimes), never assumed ones.
  - `secrets`: the keyring (service `quiver`, entry `epic:<account id>`), and an in-memory
    store for tests.
  - `accounts`: `<data>/epic/accounts.json` (ids, names, default, linked Ubisoft account,
    "needs sign-in"). No secret in it.
  - `library`: records plus catalog into titles. Left out: UE marketplace items, private
    sandboxes, engines and plugins, items with no Windows build. DLC and extras mark their
    main game's DLC list as owned. A record whose catalog item didn't come back stays, named by
    its app. Caches in `<data>/epic/cache/`: the last good library per account (used, with
    the date, when Epic can't be reached) and catalog items (3 days).
  - `Epic`: sign-in (session in the keyring before the account is listed), several accounts
    and a default, sign-out (ends the session at Epic, best effort; deletes the entry),
    `access_token` with a single-flight refresh per account that stores the rotated pair
    before using it. A refused refresh (`invalid_grant`, a corrective action, an expired
    refresh token or a missing keyring entry) marks the account "sign in again" and deletes
    the dead entry; nothing retries. The library reads every account (catalog requests 4 at a
    time, one per namespace, 50 ids per request) and is reused for 15 minutes.
  - `provider::EpicProvider`: `epic:<app>` entries (art in the new `Art.epic`, owners as Epic
    account ids, not installable), the game page (developer, description, DLC with owned
    flags, the owner's name, `linked` for Ubisoft titles), and Play refused with the reason
    ("isn't installed: … next Quiver build", or "a Ubisoft Connect game: play it as
    ubisoft:3539").
- **Merging (STORES.md section 2.4).** `LibraryEntry.records: Vec<StoreRecord>` with
  `Relation::{CopyOf, Linked}`. An Epic title whose catalog says Ubisoft
  (`ThirdPartyManagedProvider`/`ThirdPartyManagedApp` naming Ubisoft or Uplay, or
  `partnerLinkType = ubisoft`) and gives a numeric `GameID` carries a `Linked` record to
  `ubisoft:<GameID>`. `merge_libraries` folds it into that Ubisoft entry as a `CopyOf` record
  (Epic badge via `stores()`, Epic art as a fallback) when a signed-in Ubisoft account has the
  game, and otherwise keeps the Epic entry. Nothing merges on names.
- **Protocol** (additive): `Request::Epic(EpicRequest::{LoginBegin, LoginComplete, Logout,
  Accounts, AccountDefault})` (its `Debug` never prints the code), `Event::StoreAccountsChanged`,
  `GameDetail.{description, linked}`, `Dlc.owned`, `ArtOrigin::Epic`.
  `quiver_core::secret::Secret<T>` (STORES.md section 3.2) redacts itself when printed.
- **quiverd**: the Epic provider is in the store list; `epic.rs` handles `Request::Epic`.
  `LoginComplete` and `Logout` are refused from any source but `cli` and `gui` (agents: Deny).
  After a sign-in, quiverd reads the library in the background so the catalog is cached
  before anyone looks. `Stores` counts folded copies toward Epic.
- **CLI**: `quiver epic login [--code C] [--open]` (prints the URL, reads the pasted code,
  one line or a block of JSON), `quiver epic logout [account]`, `quiver epic accounts
  [--json]`, `quiver epic default <account>`. `quiver library --store epic` lists Epic titles
  and Ubisoft entries with Epic copies, with a note on each ("Epic, not installed", "also
  bought as epic:…", "a Ubisoft Connect game, not on a signed-in Ubisoft account"). `quiver
  info epic:<app>` shows the Epic page even when the copy is folded into a Ubisoft entry.
- **App**: Settings › Accounts has an Epic Games part: accounts (linked Ubisoft account,
  "sign in again"), Make default, Sign out (asks by relabelling the button), and Sign in to
  Epic: opens Epic's page in the browser and shows the three steps and a field for the code.
  An Epic tile's button says "Not installed", and pressing it shows the provider's reason
  instead of the Ubisoft install dialog. The store page shows the description and the linked
  Ubisoft game, and says Epic achievements aren't read yet.
- **README**: the Epic section, including which client Quiver signs in as and the override
  file (steer 2).

### 9.3 Differences from the design

- **Epic's account index is its own file**, `<data>/epic/accounts.json`, not entries with
  `store: "epic"` in `accounts.json` (STORES.md section 3.2). That file is Ubisoft's, and a
  quiverd from before stores rewrites it without fields it doesn't know.
- **Activation (redeeming Ubisoft codes) isn't built.** The library already says which Epic
  titles are Ubisoft games and whether a signed-in Ubisoft account has them, which is what
  activation needs; the redemption itself (store GraphQL, irreversible) waits for its own
  step with the pending-approval gate (steer 6).
- **No first-run Stores step** yet (as asked): Settings › Accounts only.
- **Release dates** aren't shown: the catalog's `releaseInfo.dateAdded` is when the build
  was added, not the release, so the page lists the date as unavailable.
- The library's first read after a sign-in makes one catalog request per game namespace
  (so about 120 for a library like this machine's), 4 at a time, once; later reads use the
  cache.

### 9.4 Spike S0: your first sign-in (the steps)

What it checks: code login with EGL's client works today (after the EGL v2 preview), the
library and catalog parse on real data, Alan Wake Remastered is there, and Quiver's session
doesn't sign Heroic or the Epic Games Launcher out. Nothing is installed or launched, and
nothing is written to your Epic account.

1. **Install the build** (after merging): `tools/install-user.sh`. `quiver daemon status`
   should show quiverd on the new build (it restarts onto it on its own once no game runs).
2. **If you use Heroic or EGL**, note now that they're signed in: Heroic › Settings ›
   Accounts (or its sidebar) shows your Epic name; `legendary status` prints "Epic account:
   <name>"; EGL shows your name at the bottom left. Leave them as they are.
3. **Sign in:** `quiver epic login --open`. It prints and opens
   `https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode`.
   (In the app: Settings › Accounts › Sign in to Epic does the same.)
4. Sign in on Epic's page (password, 2FA, captcha). It ends on a page of JSON:
   `{"warning":"Do not share this code…","redirectUrl":"…","authorizationCode":"<32 letters and digits>","exchangeCode":null,"sid":null}`.
   If `authorizationCode` is `null`, you weren't signed in in that browser: sign in on
   epicgames.com, then open the link again.
5. **Paste** the code (or the whole page) at `Code:` and press Enter, within a few minutes (the
   code works once). Expect `Signed in to Epic as <name> (default account) (linked Ubisoft
   account: <name>)`. If Epic asks for something first (terms, privacy policy), Quiver prints
   the page to open; do that on Epic's site, then sign in again.
6. **Check:**
   - `quiver epic accounts`: `* <name> <account id>  Ubisoft: <your Ubisoft name>`.
   - `quiver stores`: `Epic … game(s)  signed in as <name>`.
   - `quiver library --store epic`: Alan Wake Remastered, with `(Epic, not installed)` and
     its ref (`epic:<app name>`); your Ubisoft titles from Epic (Origins, Odyssey, Far Cry 4,
     AC III Remastered, Syndicate) either as their Ubisoft entries with `(also bought as
     epic:…)`, or as Epic entries "not on a signed-in Ubisoft account".
   - `quiver info epic:<Alan Wake's app name>`: developer Remedy, description, DLC if any.
   - `grep -c eg1 ~/.local/share/quiver/epic/accounts.json` prints `0` (no token in the file;
     the session is in the keyring).
   - Optional, read-only (spike S1): `quiver ownership 3539 --json` and `quiver ownership
     420 --json`; `"target_partner": 1` means the copy came through Epic.
7. **Heroic or EGL still signed in?** Right after, and again tomorrow (Quiver refreshes its
   session when the access token is near expiry, about every 8 hours): open Heroic and refresh
   its library, or run `legendary list` (it uses and refreshes legendary's own session); open
   EGL and its Library. A "please sign in again" there is the answer to S0's second
   question.
8. **Send back:** the output of `quiver stores` and `quiver library --store epic` (titles are
   enough; account ids can be cut), `grep -i epic ~/.local/share/quiver/quiverd.log` (or
   `journalctl --user -u quiverd | grep -i epic` when quiverd runs as the systemd unit; the
   log has labels and statuses, never tokens), Alan Wake Remastered's ref, and whether
   Heroic/EGL stayed signed in.
9. **Undo**, if you want: `quiver epic logout` (ends Quiver's session at Epic and deletes
   its keyring entry; other launchers' sessions are separate).

### 9.5 As built: phase 3, cloud saves, play time, achievements (2026-11-18)

Built alongside stage 2 (installs and launch), on its own branch, so the launch calls two
hooks this phase provides (below). Nothing here called Epic: every endpoint is exercised on
recorded-shape or synthesized JSON through the fake transport, and the savesync tests run
against an in-memory savesync with Epic's listing, link and pre-signed-URL behaviour. The
protocol was checked against legendary 0.21 master (170964e), Heroic (2dbb802), egs-api-rs
(fc78686) and the community endpoint docs (cb7f1bc), read as references; no code was taken.

**epic-core, new modules** (stage 1's and stage 2's files only gained small additive
changes: `Method::Put` and a raw body on `http::Request`, `pub(crate)` on `Api`'s send
helpers, one `p3` field in `Epic`, the provider's capabilities and a `fill_detail` call):

- `savefmt`: the binary manifest (header, meta, chunk list, file list, custom fields;
  FString ASCII/UTF-16) and chunk (header v3, zlib, SHA-1 and Epic's rolling hash) formats,
  read and written, for saves only. Every length from the network is checked; an encrypted
  or JSON manifest, a part outside its chunk or a hash mismatch is an error, never a panic.
  Packaging follows EGL's: one 1 MiB stream across files sorted case-insensitively, windows
  zero-padded, `ChunksV4/<group>/<hash>_<guid>.chunk`, feature level 18, meta app name
  `<app><account id>`, `CloudSaveFolder` (and `_MAC`) in the custom fields.
- `savepaths`: `CloudSaveFolder` in a Proton prefix. `{AppData}` is `AppData/Local` (the
  catalog writes `{AppData}/../LocalLow` and `/../Roaming`), `{UserDir}` Documents,
  `{UserProfile}`, `{UserSavedGames}`, `{InstallDir}`, `{EpicID}`, a written-out `C:\…`;
  components matched case-insensitively as Wine does; macOS-only variables, unknown ones and
  placeholders (`C:\Users[username]\…`, one real catalog entry) say why they can't be placed.
  `CloudIncludeList` / `CloudExcludeList` (comma-separated globs, `dir/` prefixes).
  Tested on the real values from this machine's catalog cache (43 of 88 titles have a folder).
- `saves`: savesync list (`GET …/savesync/<account>/<app>/`), links (`POST` the same with
  `{"files": [names relative to the app]}`), pre-signed GET/PUT. The sync engine as
  STORES.md §3.7 sets it: per file against the newest cloud version and a **base** (the
  state both sides had at the last sync, `<data>/epic/saves/<app>.json`, per account);
  changed on one side → that side wins; on both → a conflict that nothing settles on its
  own; no base yet → EGL's time rule (±60 s, inside it a conflict). Before launch: download
  only. After exit and on request: download, then upload a whole new version (chunks first,
  manifest last), read back before the base moves. A backup of every file a download or
  restore replaces (`<save_backups>/epic-<app>/<time>/`). Deletions aren't synced: a file
  deleted here isn't downloaded again and the next version leaves it out (older versions keep
  it; Quiver never deletes a cloud file). `MaxSizeMB` is respected.
- `playtime`: totals (`GET …/playtime/account/<id>/all`, cached 10 minutes) and session
  reports (`PUT …/playtime/account/<id>`, `machineId` (random, stable, `<data>/epic/
  machine-id`), `artifactId`, `startTime`, `endTime`, both segment flags true). The queue
  (`playtime-queue.json`) never sends a session twice: one entry per session id; written as
  `unconfirmed` before it goes; only Epic's answer moves it on (accepted → `reported`; 5xx,
  429, 401 → back to `queued`, up to 10 tries; other errors → `failed`; no connection →
  `queued`, uncounted); a request with no answer stays `unconfirmed` and isn't resent.
- `achievements`: store GraphQL (`launcher.store.epicgames.com/graphql`, bearer token,
  EGL's store user agent): `productAchievementsRecordBySandbox` (definitions, cached a day,
  a stale copy used when the store fails) and `playerAchievementGameRecordsBySandbox` (the
  account's unlocks, cached 3 minutes), merged as the store shows them (locked names and
  descriptions until unlocked, `hidden` masked). `new_unlocks` keeps the unlocks already seen
  per account and game (`achievements/<account>/<app>.json`); the first look only records,
  so earlier unlocks never toast, and each new one toasts once. Icons are downloaded once for
  toasts.
- `progress`: phase 3's shared state, the account a game's progress is for (the default if
  it owns the game, else the first owner), the one-time yes for account writes
  (`writes.json`), and `fill_detail`, which puts Epic's play time, achievements and cloud
  state on the game page (`GameDetail.{playtime, achievements, cloud}`), which the app's
  Overview and Sight's Achievements section already read.
- `session`: `before_launch` (remember the prefix and install, pull) and `after_exit` (last
  unlock check, queue and send the session, push), and `Poller` (unlocks every 5 minutes).
  None of it fails a launch: an unreachable cloud or a conflict is in the status line and the
  game starts with this machine's saves.

**Protocol (additive).** `EpicRequest::Game(EpicGameRequest::{Saves, SavesSync, SavesRestore,
Playtime, PlaytimeSend, Achievements})` with its types in `quiver_api::epicgame`;
`Event::AchievementUnlocked.{game, key}` (Epic: `product_id` 0, the store's id in `key`);
`ReportState::ByGame`; Epic's capabilities say `cloud_saves: quiver`, `achievements:
read_only`, `playtime: quiver`.

**quiverd** (`epicgame.rs`): the requests; backups go to Quiver's `save_backups`; queued
sessions are retried every 10 minutes. Gates (STORES.md §7 item 10): the **first upload of
saves** and the **first play-time report** are writes to the person's Epic account, so they
need a person (`confirmed`, from the app or a terminal) once; after that they happen on their
own (steer 11 (a): on by default once verified, under `report_playtime`). Until then sessions
queue as `held` and uploads wait (the status says so). Settling a conflict and restoring a
backup need `confirmed` from agents. The hooks for the launch:

```rust
let s = epicgame::begin(&daemon, app, &prefix, Some(&install_dir), session_id).await; // pull, start polling
job.status = s.note().to_owned();
/* run the game */
let note = epicgame::end(&daemon, s).await;                                           // unlocks, play time, push
```

Unlocks become `AchievementUnlocked` events (Sight toasts, the open page reloads) and
activity lines. **Stage 2 wires these into its launch** (they are `allow(dead_code)` until it
does); stage 2 should also call `Epic::remember_place` at install, so the Saves tab shows the
folder before the first launch.

**CLI.** `quiver epic saves <game> [--json] [--local]`, `… sync [--keep local|cloud] [--yes]`,
`… restore <backup|latest>`; `quiver epic playtime [<game>]`, `--send [--yes]` (asks at the
terminal when the first write needs a yes). `quiver progress`, `quiver playtime` and `quiver
saves` show Epic's side for Epic games.

**App.** Epic games' pages get the Saves & progress tab: Epic's play time and the report
queue (Report to Epic / Send now), the cloud saves card (account, folder, this PC's folder,
newest cloud version and how many are kept, last sync, what a sync would do; Sync now, or for
a conflict Keep this PC's / Keep the cloud's), the file list side by side with the conflicting
files marked, backups with Restore, and the achievements. Account writes and anything that
replaces files ask by relabelling the button first. "Saves sync in game" for games without a
folder. The Overview's Play time stat is Epic's total.

**Sight.** The Achievements section already reads `GameDetail.achievements`, so it lists
Epic's. Toasts for Epic unlocks match their session by `game` (not `product_id`, which is 0).

### 9.6 Differences from the design

- **One store per Epic game, the real folder.** Steer 7 asked for Ubisoft's rule (one cloud
  store, others local). Other stores need the folder bind-mounted per launch (STORES.md §3.7
  `Tree` locations), which isn't built for any store yet, so every profile of an Epic game
  shares its real save folder, as Steam games do. The base and the conflict view are per
  account.
- **Uploads are whole versions** (as EGL's are), so "newer file wins per file" means the
  newer files on each side are merged into the next version: cloud-newer files come down
  first, then the merged folder goes up.
- **Play time is reported after each session** (`AfterSession`), not periodically: EGL sends
  both segment flags true for a session, and one PUT per session is what the never-twice
  queue can guarantee. A session quiverd loses (a daemon crash mid-game) isn't reported.
- **"Saves sync in game"** is shown for every title whose catalog names no `CloudSaveFolder`:
  the catalog can't tell an EOS-storage game from one without cloud saves, so the wording
  says "if the game has them".
- **The EOS overlay** (phase 3's last bullet) isn't built: it belongs with stage 2's launch
  (a registry entry in the prefix), and steer 8 keeps it off by default.
- **Not verified live** (unconfirmed in every reference): the first two segments of a cloud
  key (Quiver keeps them as given and finds chunks beside `manifests/`), the PUT status of a
  savesync upload (any 2xx), what the play-time segment flags mean, and whether the store
  GraphQL wants more than EGL's user agent. The steps below settle them.

### 9.7 Your first Epic session: Alan Wake Remastered (`epic:7315f614b0504081bcdceef138075c9c`)

Once stage 2's installer is merged and installed (`tools/install-user.sh`), and Alan Wake
Remastered is installed through Quiver. Steps 1–5 only read; **steps 6 and 8 write to your
Epic account and need your OK** (they are what opens the two one-time gates).

1. **Reads first.** `quiver progress epic:7315f614b0504081bcdceef138075c9c` prints Epic's
   play time for the game and its achievements with your unlocks (the first real store
   GraphQL read; an error line there means the queries or the user agent need adjusting).
   `quiver epic playtime` lists Epic's totals for all your games.
2. `quiver epic saves epic:7315f614b0504081bcdceef138075c9c` should say **saves sync in
   game**: Alan Wake Remastered's catalog names no save folder (checked in this machine's
   catalog cache), so the launcher cloud isn't involved for it.
3. **Play** it through Quiver for ten minutes or more, with Sight open if you like (Shift+F2;
   its Achievements section lists Epic's). If you earn an achievement, a toast should appear
   within about five minutes, and once more at the end at the latest; never twice.
4. Exit the game. `quiver epic playtime epic:7315f614b0504081bcdceef138075c9c` lists the
   session as **waiting for your first yes** (reporting is on, the gate is closed).
5. `grep -i "epic" ~/.local/share/quiver/quiverd.log | tail -30` (or `journalctl --user -u
   quiverd`): labels and statuses only (`playtime totals`, `achievements`, `player
   achievements`), never tokens.
6. **Account write (your OK): the first play-time report.** `quiver epic playtime --send --yes`
   should print the session as `reported`. A few minutes later, `quiver epic playtime
   epic:7315f614b0504081bcdceef138075c9c` (it asks Epic again) should show the total up by
   about the session's length, and the Epic Games Launcher (or epicgames.com › your library)
   should agree. From now on sessions are reported when they end. If Epic refused it, the
   line says `refused by Epic` with Epic's reason: send that back.
7. **For the save round trip, pick a small owned game that has a save folder** (Alan Wake
   Remastered has none): for example Loop Hero (`{AppData}/Loop_hero/`) or The Bridge.
   Install it through Quiver, play until it saves, exit. `quiver epic saves <game>` shows the
   files as **newer on this PC** and "uploads wait for your first yes".
8. **Account write (your OK): the first save upload.** `quiver epic saves <game> sync --yes`
   (or Upload to Epic, pressed twice, in the game's Saves & progress tab) should print the
   files and `new cloud version <UTC time>`. Then `quiver epic saves <game>` should say In sync
   with one version kept. To see it from Epic's side, read-only: `legendary list-saves` (if
   you use legendary) or the Epic Games Launcher's cloud-save prompt for that game.
9. **Pull back** (optional, read-only for your account): move the save folder aside
   (`quiver epic saves <game>` prints it), then `quiver epic saves <game> sync`: the files come
   back from the cloud. Moving the folder back and syncing again shows a conflict only if
   both sides changed; pick one with `--keep local|cloud` (the other side is backed up first).
10. **Send back:** the output of steps 1, 2, 4, 6 and 8, and the quiverd log lines.
    **Undo**, if you want: uploaded versions can be deleted only in Epic's launcher (Quiver
    never deletes cloud files); a reported session can't be taken back.

### 9.8 Library cleanup (2026-11-18)

After the first real sign-in the library had 120 Epic entries, no art on any of them, and
about 30 tiles named by a 32-hex id. What they were, from quiverd's own caches
(`<data>/epic/cache/`, read-only):

- **The hex tiles were real games and their DLC whose catalog lookup failed.** All 32
  records without a catalog item were in the 11 namespaces where the library has more than
  one record: Fortnite (three apps), Borderlands 2 (game and two DLC), Killing Floor 2,
  Civilization VI (game and the Aztec DLC), Death Stranding (five), Fallout: New Vegas
  (seven), Alien: Isolation, Pillars of Eternity, Tyranny, Tropico 5 and a sandbox named
  Hydra (two each). Every single-record namespace resolved. Stage 1 asked `bulk/items` for
  a namespace's ids comma-separated in one call, and that form came back without any of
  them. The newer catalog names apps (DLC especially) by hex id, so those records showed
  their app name. None were UE assets, engines, launcher items or private sandboxes: all
  120 records are `APPLICATION`s in public sandboxes.
- **`epic:Quail` "20XX" and `epic:Flounder` "3 out of 10, EP 1: …" are real games** (both
  `games`/`applications`, executable, Windows, with art; 3 out of 10 is an episodic game
  Epic gave away). They stay.
- **No art anywhere** was the app refusing the URLs: `cdn1.epicgames.com` wasn't an allowed
  art host, and Epic's image URLs have spaces, `&` and often no extension, which the art
  cache's path rules refused. The provider already sent the right URLs.

What changed:

- **Catalog:** one `bulk/items` call per item (as legendary asks), 4 at a time, a failed call
  tried once more after 1.5 s. Items Epic doesn't return are asked for again at the next
  read (they were never cached). Private-sandbox items aren't asked for. quiverd now logs
  `epic_core` at info, so catalog failures show in `quiverd.log`.
- **Library rule** (`library::build`): a tile is a game or app with a catalog name and art.
  Each owned record is one of: a **game** (`games` or `applications`, executable, Windows,
  no main game), **DLC** (`mainGameItem`, listed in an owned game's `dlcItemList`, or
  `addons`; shown on its game's page as owned, never a tile), or **hidden with a reason**:
  UE asset, private sandbox, engine/plugin/asset, digital extra, audience entitlement, not a
  game or app, nothing to install, no Windows build, DLC whose game isn't owned, the catalog
  didn't describe it, no name (a hex title), no art, or managed by a launcher Quiver doesn't
  run (the EA app: STAR WARS Battlefront II). Ubisoft titles still fold into their Ubisoft
  entries. The game page's DLC list leaves out audience entitlements, engine kits and editor
  tools, and shows owned DLC first with an "Owned" mark.
- **One game, one tile:** records of one catalog item, or of one namespace with the same
  name, are one title (`Title.apps` keeps every app name, and `epic:<any of them>` finds
  it). The tile's ref is the app the catalog lists for the item, else the first acquired.
- **Hidden items** come back from `Request::Epic(EpicRequest::Hidden)` (`Vec<HiddenItem>`),
  and `quiver library --all` (with `--store epic` or not) lists them after the library as
  `epic:<app>  <title> [<sandbox>]: <reason>` (`--json --all` prints `{library, hidden}`).
  `quiver info epic:<hidden app>` says why it isn't in the library.
- **Art:** `cdn1.epicgames.com`, `cdn2.epicgames.com` and `cdn2.unrealengine.com` are art
  hosts. A URL that isn't a plain `<path>.jpg|png|webp` is cached as
  `~/.cache/quiver/art/url/<host>/h/<sha1 of the URL>.<ext>`, with the extension taken from
  the image's bytes. Slots: tile `DieselGameBoxTall` (else `OfferImageTall`, `Thumbnail`),
  hero `DieselGameBox` (else `DieselStoreFrontWide`, `OfferImageWide`), logo
  `DieselGameBoxLogo`.
- With the cache as it was, the new rule gives 87 titles (82 tiles, the five Ubisoft ones
  folded) and 33 hidden: the 32 unresolved records and Battlefront II. The first library
  read on the new build asks for the 32 again, one at a time, and they become Fortnite,
  Borderlands 2, Killing Floor 2 and the rest, with their DLC on their pages.
- `QUIVER_DEMO_LIBRARY=<file>` (demo mode only) shows a saved `quiver library --json` in the
  app, for screenshots of a real library without a daemon.

**Checking it:** after `tools/install-user.sh` and quiverd's restart, run `quiver library
--store epic --refresh`: no hex names; Fortnite, Borderlands 2, Death Stranding, Fallout: New
Vegas and the rest listed by name. `quiver library --store epic --all` ends with "Not in the
library (N)": expect Battlefront II (the EA app) and any item Epic still doesn't describe.
`grep "Epic catalog" ~/.local/share/quiver/quiverd.log` names any item that failed. In the
app every Epic tile has box art, and an Epic game's page lists its DLC with what's owned.

### 9.9 Sign-in window (quiver-webview), 2026-11-18

The user asked for sign-in inside Quiver instead of copying `authorizationCode` out of Epic's
JSON page by hand. `quiver epic login` and the app's **Sign in to Epic** now open Epic's own
sign-in page in a small window of Quiver's, which hands the code back by itself.

**Design.**

- A separate binary, `crates/quiver-webview` (`quiver-webview epic-login --url <url>
  [--timeout <s>]`), so neither quiverd nor the app has WebKit in its process. The URL is the
  one `EpicRequest::LoginBegin` returns (the same `quiver epic login` always printed); the
  helper refuses any URL not on `https://www.epicgames.com/`.
- The window is titled "Quiver — Sign in to Epic Games". It watches each finished load. When
  the page is `https://www.epicgames.com/id/api/redirect…`, it reads that page's body (WebKit's
  main resource, the raw JSON), takes `authorizationCode`, prints it on **stdout** and exits.
  The code never goes to stderr, a log or a file; stdout is a pipe the caller reads.
- `"authorizationCode": null` (Epic finished the sign-in on another page first) loads the
  sign-in URL again; with the session cookie now set, Epic redirects straight to a page with a
  code. After three such retries it gives up with a message.
- **No Epic cookies on disk.** The web view uses an ephemeral website-data manager (cookies,
  local storage and caches in memory only), and the process's `XDG_CACHE_HOME` and
  `XDG_DATA_HOME` point into a private 0700 directory (`$XDG_RUNTIME_DIR/quiver-webview-XXXXXX`)
  deleted on every exit path, including SIGTERM/SIGINT/SIGHUP (handled in GLib's main loop) and
  the caller dying (`PR_SET_PDEATHSIG`).
- Closing the window is a cancel; a timeout (600 s by default) ends it too.
- Exit codes: 0 signed in (code on stdout), 1 failed, 2 window closed, 3 timed out, 4 no
  WebKitGTK or no display, 64 bad arguments.
- Client: `quiver_api::epic::webview::sign_in(url, timeout) -> Outcome` (`Code(SignInCode)`,
  whose `Debug` is redacted; `Cancelled`; `TimedOut`; `Unavailable(reason)`; `Failed(reason)`).
  It finds the helper at `$QUIVER_WEBVIEW`, next to the running binary, then on `PATH`, and gives
  the helper 15 s past its own timeout before stopping it (SIGTERM, then kill).
- **CLI**: `quiver epic login` uses the window by default ("Opening Epic's sign-in window…").
  `--paste` (or `--open`) is the old browser-and-paste flow. If the window is unavailable it
  says why ("The sign-in window isn't available (…); install webkit2gtk-4.1 for it. Falling back
  to pasting the code.") and continues with the paste flow. Closing the window prints "Sign-in
  cancelled" and exits non-zero. `--code` is unchanged.
- **App**: Settings › Accounts › Epic Games › Sign in to Epic runs the helper off the UI thread
  and shows a card while the window is open; the code goes straight to `LoginComplete`. A
  missing window falls back to the browser and code field with a toast saying why; a cancel,
  timeout or failure leaves a short note. "Paste a code instead" is always offered on that card.
- **Packaging**: `packaging/layout.sh` installs `quiver-webview` beside the other binaries (so
  `tools/install-user.sh` and the PKGBUILD both ship it); the PKGBUILD lists
  `webkit2gtk-4.1` under `optdepends`.

**Bindings: none (dlopen).** The helper opens `libwebkit2gtk-4.1.so.0` at run time and looks up
the 22 C functions it calls (GTK 3 window and main loop, `g_signal_connect_data`, GLib timeouts
and Unix signals, WebKit's ephemeral data manager, context, view, `load_uri`, `get_uri`, and
the main resource's `get_data`) through that handle, which also resolves GTK's and GLib's
symbols from WebKit's own dependencies (`src/ffi.rs`). Weighed against:

| Option | Build weight | Maintenance | Reading the page body |
|---|---|---|---|
| **dlopen, hand-written signatures (chosen)** | `libc` + `serde_json`; no headers or pkg-config at build time | 22 stable C signatures (the WebKitGTK 4.1 and GTK 3 APIs are frozen); unsafe code is confined to two small files | `webkit_web_resource_get_data` directly |
| webkit2gtk-rs 2.0 + gtk-rs 0.18 | ~40 crates, needs webkit2gtk/gtk3/soup headers to build the whole workspace | gtk3-rs is archived upstream; kept alive by Tauri | same calls, safe wrappers |
| wry (+ tao) | heavier still (wry, tao, webkit2gtk-rs, gtk-rs) | maintained by Tauri | only through JavaScript and an IPC handler |

The deciding point: with dlopen, `webkit2gtk-4.1` is a true run-time optional dependency. The
workspace and the package build on machines without it, and a missing library is exit code 4
and a clear fallback, not a binary that fails to start because a shared library is missing.

**Tested** (`crates/quiver-webview/tests/sign_in.rs`, run by `cargo test --workspace`) against a
local server on 127.0.0.1 in a private Xvfb (a free display number of its own, `DISPLAY` set
only for the helper, `WAYLAND_DISPLAY` removed, the session bus pointed nowhere); never on the
desktop. The server's `/login` redirects with HTTP 302 (and `/login-js` with JavaScript) to its
own `/id/api/redirect`, which serves Epic's JSON. The test-only `--redirect-prefix` points the
matcher at it; it and `--close-after-load` are refused unless `--url` is a loopback URL. Cases:
the code on stdout (and not on stderr); the JavaScript redirect; `null` once, then a code
(the redirect page is visited twice); `null` every time (exit 1, nothing on stdout); the window
closed (`--close-after-load` closes it the way the window manager does: exit 2); a 2 s timeout
(exit 3); a missing library (`QUIVER_WEBVIEW_TEST_LIB`, loopback only: exit 4); no display
(exit 4). After every run the private directory is checked to be gone. The test skips with a
note when Xvfb or WebKitGTK isn't installed. Unit tests cover the argument and URL rules, the
redirect matcher and the JSON (`src/args.rs`), and the client's exit-code mapping and hung-helper
handling with stand-in scripts (`quiver-api`).

**How to check it** (after `tools/install-user.sh`; it doesn't need quiverd restarted for the
window itself, but `quiver epic login` talks to quiverd as before):

1. `quiver-webview epic-login --url 'https://www.epicgames.com/id/login' --timeout 30`, then close
   the window: it prints `quiver-webview: the sign-in window was closed` and `echo $?` says 2.
   Nothing is signed in.
2. For a real sign-in (adds or refreshes an Epic account, as `quiver epic login` always has):
   Settings › Accounts › Epic Games › Add Epic account (or `quiver epic login`). Sign in in the
   window; it closes by itself and the account appears. `ls $XDG_RUNTIME_DIR | grep
   quiver-webview` prints nothing afterwards.
3. Fallback: `QUIVER_WEBVIEW=/nonexistent quiver epic login` prints the "isn't available …
   Falling back to pasting the code" line and the old prompt (Ctrl+C to leave).

### 9.10 As built: stage 2, installs and launch (2026-11-18)

Nothing in this stage downloaded a real build or launched a real game. Every path ran
against built manifests and chunks on a local HTTP server, behind the recorded API shapes,
and parsed manifests were checked against legendary 0.21.1 (run with a scratch config).

**Content engine** (`epic-core::{manifest, chunk, content}`, our own, legendary read as a
spec only):

- **Manifests.** Binary manifests (magic `0x44BEC00C`, zlib, SHA-1 of the body checked),
  versions up to the v22+ fields (per-chunk secret guids, compressed window sizes,
  encryption tags, `ChunksV5/<secret or plain>/<group>/<hash>_<guid>.chunk` paths) and the
  v24 encrypted section, which is decrypted with the build's keys (AES-GCM). JSON manifests
  (first byte `{`) are read too. Every read is bounds-checked: a truncated or hostile
  manifest is an error, never a panic or a huge allocation (a fuzz loop over truncations is
  a test). `manifest::write` serializes, for tests and for keeping the installed manifest.
- **Chunks.** Header v1–v4, AES-GCM for encrypted chunks (nonce: the first 12 bytes of the
  chunk's SHA-1, as legendary does), zlib, then the SHA-1 checked against the header and
  the manifest, and the guid against the manifest. A bad chunk is refused and fetched again.
- **Engine.** `content::install` downloads each needed chunk once (8 at a time, in the order
  files first use them), through the queue's bandwidth limit, with the queue's retry policy
  (backoff, `retries` counted). One writer thread assembles files part by part into
  `<path>.quiver-part`, checks each file's SHA-1 and renames it. Chunks over a 256 MiB
  budget wait in `.quiver-install/epic-chunks/`, so a large game never holds GBs in memory.
  Executables get 0755. A resume record (`.quiver-install/epic-state.json`: build,
  manifest fingerprint, finished files with their SHA-1, saved every 2 s and on any stop)
  means a pause, a crash or a daemon restart resumes without fetching finished files. Free
  space is checked before downloading (`check_space`, with the usual margin). Only
  manifest paths are written (no `..`, no absolute paths). Updates keep unchanged files,
  rebuild changed ones and remove files the new build dropped. `verify` hashes every file
  (SHA-1) on a blocking task, `repair` rebuilds only the bad and missing ones, and
  `uninstall` removes only the manifest's files and the folders they leave empty. Install
  tags are supported (`None`: everything).
- **Updates: reuse and deltas** (added with update polling, 2026-11-18). A chunk guid names
  one immutable window of data, so a part of a changed file cut from a chunk the old
  build's copy of the *same path* also used is read from that old copy instead of
  downloaded (`content::reusable_parts`); the old file stays in place until the new one is
  renamed over it. Before reading from an old copy the install hashes it against the
  installed manifest's SHA-1 (a damaged or half-replaced copy is downloaded instead); the
  preview trusts sizes only. Data another file held is still downloaded (no cross-file
  reuse). When the old and new builds have different build ids, `Deltas/<new build id>/<old
  build id>.delta` is fetched next to the chunks (no session, as legendary's
  `get_delta_manifest`); any failure means no delta. Its file entries replace the new
  manifest's only where the path and SHA-1 match, the parts add up to the file and every
  chunk they cut is listed (by the delta, the new build or the old one) with the range
  inside it; a delta whose chunks live under another `Chunks*` folder is not used. The
  combination is still the new build (same files and SHA-1s, so the resume record carries
  over), every file is checked as it is written, and if an update through the delta fails
  for any reason but a cancel it runs again from the full manifest. The installed manifest
  saved afterwards is Epic's plain one. DLC updates take the same path. `UpdatePreview`
  says what a delta and reuse save (`notes`), and its download is what the update will
  really fetch.

**Epic calls** (`epic-core::api`): the build (`launcher …/assets/v2/platform/Windows/
namespace/<ns>/catalogItem/<id>/app/<app>/label/Live`: manifest locations, SHA-1, keys),
the manifest file (no session; signed URLs never logged; SHA-1 checked), the exchange code
(`GET account …/oauth/exchange`) and the ownership token (`POST ecommerceintegration
…/ownershipToken`). Keys and tokens are `Secret`s and live for one operation.

**Installs** (`epic-core::{ops, installs}`):

- A game installs to `<library>/<FolderName or title slug>` (Alan Wake Remastered:
  `~/Games/AlanWakeRemastered` with the default library) and gets its own prefix
  `<prefixes>/<title slug>` (`-epic` appended when a Ubisoft game already uses the name).
- Records: `<data>/epic/installed.json` (folder, prefix, build, launch executable and
  arguments, prerequisites and whether they ran, installed DLC, the account, adopted or
  not) and `<data>/epic/manifests/<app>.manifest`, the installed build's manifest kept
  decrypted. Neither holds a key, a signed URL or a token.
- **DLC:** owned DLC whose catalog item has a build of its own installs into the game's
  folder after the game, and is listed on the record; DLC without one is an entitlement
  the game checks with Epic itself.
- **Adopt** (`quiver import epic:<app> <folder>`): an existing folder (another launcher's
  install, a copy) becomes Quiver's install in place after its files are checked against
  the current build's manifest; only missing or different files are downloaded. A folder
  with none of the game's files is refused. No other launcher's records or sessions are read.
- Phase 3's `remember_place` is called at install and adoption, so the Saves tab knows the
  folders before the first launch.

**The queue and the protocol** (the R1 sweep for downloads, STORES.md §11):

- `Install`, `Import`, `Uninstall`, `Update`, `Verify`, `VerifyResult`, `InstallPreview`,
  `UpdatePreview` and the `Queue*` requests take a game ref (`GameField`, so Ubisoft games
  still go out as `product_id` and older daemons and clients read them). Ubisoft requests
  behave exactly as before; `epic:` refs go to `quiverd/src/epic.rs`.
- `QueueEntry`, `InstallPreview`, `UpdatePreview` and `VerifyReport` carry `game` for
  other stores' games (`product_id` is then 0). The queue (`downloads.rs`) is keyed by
  `GameRef`; Ubisoft queue lines in `queue.json` are unchanged.
- Epic items run on the same queue as Ubisoft's (`max_active`, bandwidth limit, pause,
  resume, reorder, remove with discard, restart resume, auto-start after a game exits).
  Verify-only jobs share the Ubisoft job plumbing (`begin_verify`/`end_verify`) and their
  reports go to `<data>/games/epic-<app>/verify.json`.
- `EpicRequest::Manifest { game }` (`quiver epic manifest <game> [--json]`) fetches the
  current build's manifest without installing anything: version, files, sizes, launch
  executable, prerequisites, whether it is encrypted, and what an update would change. It
  is the cheap real-data check before a large download.

**Launch** (`quiver_core::launcher::play_store_game`, `quiverd/src/epic.rs::play`):

- An installed Epic game plays through Quiver with the plan every Quiver launch gets:
  `mods::launch_plan` (profiles, fixes, overlays in a user + mount namespace, config
  patches), the game's settings (Proton build, env, DLL overrides, arguments, wrappers),
  Options, Tweaks, the perf session and the HUD. Mods and settings key on
  `games/epic-<app>/`, `mods/epic-<app>/`.
- Before the game: the prefix (plain Proton, no Ubisoft SDK), then phase 3's
  `epicgame::begin` (cloud saves pulled, achievements watched), then the build's
  prerequisite installer once per prefix (a marker in `quiver-run/`; the record notes it
  ran). After it exits: `epicgame::end` (play time to Epic, saves up), the local play
  session (`settings::record_session` for `epic:<app>`), and the ownership token file
  removed.
- Licence: a fresh exchange code per launch, and EGL's arguments in legendary's order: the
  manifest's launch command, the catalog's `AdditionalCommandLine`, `-AUTH_LOGIN=unused
  -AUTH_PASSWORD=<code> -AUTH_TYPE=exchangecode -epicapp=<app> -epicenv=Prod`,
  `-epicovt=<file>` when the catalog says `OwnershipToken` (the token written 0600 to the
  prefix's `quiver-run/`, deleted at exit), `-EpicPortal -epicusername= -epicuserid=
  -epiclocale= -epicsandboxid=<namespace>`. The job's detail shows the arguments with
  `-AUTH_PASSWORD=<redacted>`; the code is never logged or written.
- `Stop` shuts the prefix down; the library marks the game running; quiverd counts it as a
  running game (it doesn't restart onto a new build under it).

**CLI and app.** `quiver install|update|verify [--repair]|uninstall|import|pause|resume|
cancel|stop|play epic:<app>` (or the game's name), `quiver install epic:<app> --dry-run`
(the preview), `quiver queue`. In the app an Epic tile's button is **Install** (the same
install dialog: destination, download and install sizes, free space), then **Play**; the
Downloads view lists Epic items with the same controls. Ubisoft games sold on Epic still
say to install them as their Ubisoft product, and games of launchers Quiver doesn't run
stay out of the library (§9.8).

**Not done or unverified.**

- The real formats are checked only against legendary's reading of them: the v24
  encrypted section and the encrypted-chunk nonce have never met a real encrypted build,
  and the JSON manifest reader is from the format as remembered (no real JSON manifest
  here). `quiver epic manifest` on a real game is the first check.
- Delta manifests and reuse (above) are built against fixtures only: no real Epic delta has
  been fetched, so its URL (`Deltas/<new>/<old>.delta` under the chunk base, legendary's
  order), whether real deltas list the old build's chunks, and their manifest version are
  unverified. Most builds have no delta; reuse from the old copies works without one.
- The exchange code is minted before the prerequisites run. Codes last minutes, so a
  prerequisite installer that takes longer than that would leave the game with an expired
  code (start it again; the prerequisites don't run twice).
- No anti-cheat runtime handling yet (EAC/BattlEye), no EOS overlay, no install-tag picker
  in the app (everything installs), no move.
- Auto-update for Epic games isn't on (Quiver doesn't poll Epic's builds); `quiver update
  epic:<app>` or Install on the game updates to the current build.

### 9.11 Installing and playing Alan Wake Remastered (the steps)

After merging and `tools/install-user.sh` (quiverd restarts onto the new build when no game
runs). Steps 1 and 2 only read; step 3 downloads about 40 GB; step 5 uses your Epic session
to mint an exchange code (as EGL does at every launch).

1. **The build, read-only:** `quiver epic manifest epic:7315f614b0504081bcdceef138075c9c`.
   Expect a build version, a manifest version, a file count in the thousands, the install
   and download sizes, `launches …/AlanWake.exe` (or similar) and maybe prerequisites. An
   error here (manifest version, decryption, a missing key) is worth sending back before
   downloading anything; `--json` has everything.
2. **The preview:** `quiver install epic:7315f614b0504081bcdceef138075c9c --dry-run`: the
   destination (`~/Games/AlanWakeRemastered` unless your library folder is elsewhere), sizes
   and whether it fits.
3. **Install:** `quiver install epic:7315f614b0504081bcdceef138075c9c` (or **Install** on the
   tile in the app). It follows the job; `quiver queue` and the Downloads view show speed,
   pause and resume. Try `quiver pause` and `quiver resume` on it once: finished files
   aren't fetched again.
4. **Verify:** `quiver verify epic:7315f614b0504081bcdceef138075c9c`: every file OK.
5. **Play:** `quiver play epic:7315f614b0504081bcdceef138075c9c` (or **Play** in the app). The
   first launch creates the prefix and may run the prerequisites; the job's detail lists the
   arguments with the code redacted. Phase 3's steps (§9.7) apply from here.
6. **Send back** the output of steps 1–4, whether the game started and reached its menu, the
   job detail (`quiver status --json` or the Activity view), and `grep -i epic
   ~/.local/share/quiver/quiverd.log | tail -50`.
7. **Undo:** `quiver uninstall epic:7315f614b0504081bcdceef138075c9c [--prefix]` removes the
   game's files (only the manifest's) and, with `--prefix`, its prefix.

---

## Sources

Local, read-only:
- Quiver: `crates/quiver-api/src/lib.rs` (`Request`, `LibraryEntry`), `crates/quiver-core/src/{store,auth,launcher,playsession,progress,savesync,broker,online}.rs`,
  `crates/quiver-bridge/src/win/api.rs` (achievements stub, `target_partner: None`), `proto/proto_ownership/ownership.proto`,
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
