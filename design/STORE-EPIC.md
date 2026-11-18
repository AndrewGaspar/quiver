# Epic Games Store in Quiver (scoping and design notes)

Status: designed 2026-11-16; steers decided and stage 1 (accounts, library, catalog, the Epic
provider, CLI and the app's sign-in) built 2026-11-17 (section 9 says what was
built and how it differs; section 8 has the decisions). Stage 2 (native installs and launch)
waits for the sign-in spike S0 (section 9.4). This is one of five
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

### 9.6 Sign-in window (quiver-webview), 2026-11-18

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
