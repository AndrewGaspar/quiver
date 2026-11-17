# Store: EA (the EA app, formerly Origin) (scoping notes)

Status: scoping only (2026-11-16). Nothing here is built and no EA service was contacted.
Companion to the sibling scopes STORE-STEAM, STORE-EPIC, STORE-GOG and STORE-META (the Meta VR
store), written in parallel; the store-agnostic interfaces are sketched here only as far as EA
needs them, and STORES.md owns the shared shape.

The ask: "integrate EA Games, with all the same great capabilities we built for Ubisoft
Connect." This document lists those capabilities as the code has them today, says how EA does
each one, what open-source prior art exists, and what it would cost, and draws the line between
legitimate licensing and DRM circumvention for EA specifically.

Bracketed numbers such as [M1] are sources, listed in [Sources](#sources). **(unverified)** marks
claims not confirmed by a primary source or by running anything.

---

## 1. Summary and recommendation

**Recommendation: build EA support natively, the way Quiver already does Ubisoft. That means
an EA client core in Rust and an LSX service inside `quiverd` that games connect to in place
of the EA app. Write it as Quiver's own MIT code, using Maxima as the protocol reference
rather than as a linked dependency. Don't run the EA app in Wine as the main path.**

Why:

- **It's been done, so it's feasible.** Maxima [M1] (ArmchairDevelopers, Rust, GPL-3.0) is a
  working open-source EA launcher on Linux. It handles EA sign-in (OAuth with PKCE), the
  library over EA's GraphQL, downloads, OOA licences from EA's licence server, an LSX server
  the games talk to, Denuvo tokens through that server, and EA cloud saves. It's used in
  production by KYBER, the Star Wars Battlefront II mod platform [W14]. Quiver needs the same
  pieces, and they map almost one to one onto what Quiver already has for Ubisoft:
  `auth`/`store` → EA Nucleus OAuth, `library` → GraphQL entitlements, `install`/`verify` →
  DiP zip downloads, the Denuvo broker → LSX `RequestLicense`, `savesync` → cloudsync.
- **It's simpler than ubi-bridge in one important way.** Ubisoft's SDK reaches Connect over
  named pipes, which is why Quiver needs a Windows binary in the prefix. EA's SDK reaches the
  EA app over **TCP on 127.0.0.1:3216** [M3][W1], and a Wine process's TCP connection reaches
  the host directly. The LSX server can therefore live in `quiverd` on the host. No EA
  binaries and no Quiver Windows binary go in the game's prefix, only a few registry keys.
  This also works from Steam's pressure-vessel container, as the broker's TCP already does.
- **The EA app in Wine is the fragile path.** EA Desktop updates keep breaking it under
  Proton. In 2026 alone there were Proton Experimental fixes in January ("many EA games
  unplayable after a recent EA Desktop update"), a crash fix in April, and a stable 11.0-2e
  fix in October [W10][W11][W12]. It also wants one shared EA-app prefix, which breaks
  Quiver's prefix-per-game model, and its own UI owns sign-in, downloads and saves, so Quiver
  would be wrapping another launcher rather than replacing it. Keep it only as a later
  **fallback runner** for titles the native path can't handle (steer 1).
- **Don't link Maxima.** It is GPL-3.0 and Quiver is MIT. It describes itself as
  "pre-pre-pre-alpha" [M1]. It holds one global `Maxima` mutex, has `todo!()` and
  `unimplemented!()` panics on live paths [M6][M9], stores tokens in a plain TOML file [M5],
  and has one account. None of that fits `quiverd`'s multi-account, multi-client daemon. Use
  it as a reference for protocol facts, which aren't copyrightable, and as a behaviour oracle
  in tests. The LSX message types could come from `origin-sdk` (MIT, an XSD-generated LSX
  client) [O1] after review. See steer 2 on licensing.

**What "the same capabilities" will and won't mean for EA:**

- **Matches Ubisoft:** sign-in, library, art, install, update, verify, uninstall, per-game
  prefixes, launch with real licences (Denuvo included), cloud saves, local play time, mods
  and profiles, Options, the Sight HUD, the CLI and agent policy, exports and first-run.
- **Lags behind:** reporting achievements and play time *to EA*. The write APIs aren't known
  yet; Maxima only reads them [M8]. DLC installs need Sims 4-style content packs, which
  Maxima only plans [M1]. Steam and Epic copies of EA games are in development in Maxima, and
  its `link2ea` handler is a `todo!()` [M9].
- **Never possible:** anything using **EA Javelin Anticheat**: Battlefield 6/2042/V/1, EA
  SPORTS FC, Madden, F1 24/25, skate. Apex Legends is blocked on Linux by its own anti-cheat
  as of Oct 2024 [W5][W6][W7][W8]. EA has a job listing about Linux and Proton "in the
  future" [W9], but nothing has shipped. Quiver should say "Not playable on Linux" with the
  reason (steer 7).

**Top risks** (detail in [§7](#7-tos-ban-risk-and-eas-history-of-breaking-things)):

1. EA changes break things often: a login path patched, an OOA token format changed, EA SDK
   updates inside games. This is a standing maintenance cost, larger than Ubisoft's so far.
2. **Denuvo activation lockouts.** About 5 activations per 24 h, and Proton or hardware
   fingerprint changes count as new machines [W13]. The licence request's `machineHash` must
   stay stable across Proton versions and prefixes.
3. ToS: EA's User Agreement forbids reverse engineering and tampering with protection
   measures [W15]. A third-party client that presents itself as the EA app (`JUNO_PC_CLIENT`,
   `pc_sign`) is in the same grey zone as Quiver's use of Connect's AppId. I found no public
   reports of EA banning accounts for using Maxima **(unverified absence)**.
4. Licence fidelity. Quiver must store EA's licence exactly as EA delivered it and answer the
   game's entitlement queries only from EA's answer. That is the line in
   [§4.4](#44-where-the-licensing-line-is-for-ea).

---

## 2. What "the same capabilities" means today (verified in code)

I checked the capability list against the crates on `master` (91c965d). "Built" means code
and a daemon request exist. "Designed" means a design doc exists and there's no code.

| # | Capability | State in Quiver | Where |
|---|---|---|---|
| C1 | Accounts and auth: several accounts, a default, 2FA, keyring, "signed in elsewhere" | Built | `ubi-core/auth.rs` (UbiServices sessions, remember-me), `store.rs` (Secret Service, `accounts.json`), `Request::Login/Login2fa/Logout/AccountDefault` |
| C2 | Library sync, merged across accounts, with art and metadata | Built | `library.rs` (demux ownership), `spaces.rs` and `ubi-api/art.rs` (art candidates), `gameinfo.rs`, `Request::Library/GameInfo` |
| C3 | Install, download, queue, pause and resume, update preview, verify and repair, uninstall, import | Built | `install.rs`, `builds.rs`, `download.rs`, `verify.rs`, `ubid/downloads.rs`; `Request::Install/Update/Verify/Queue*/Import/ScanInstalls/Uninstall` |
| C3b | Move an install | Designed only | PROPOSAL lists `quiver move`; there's no request or code |
| C4 | A Proton prefix per game | Built | `launcher.rs` (`Config.prefixes`, `<prefixes>/<slug>`) |
| C5 | Launch with licensing: the bridge replaces the client, the Denuvo token broker, ownership and CD keys in `InitReq` | Built | `ubi-bridge` (named pipes, `win/api.rs`, `win/aux.rs`), `broker.rs`, `ubi-ipc` `BrokerRequest::{DenuvoGameToken, PlaySession}` |
| C6 | Cloud saves: pull before, push after, backups, save stores, merge | Built | `cloudsave.rs`, `savesync.rs`, `saves.rs`, `savestores.rs` |
| C7 | Achievements | Partly built: definitions and progress are *read* from UbiServices. The bridge answers `GetAchievements` with an empty list and only logs earns ("not reported to Ubisoft yet") | `progress.rs`, `ubi-bridge/src/win/api.rs:403` |
| C8 | Play time and progression: sessions reported to Ubisoft, local history, stats, Club | Built | `playsession.rs` (POST, then PUT every 330 s, then DELETE), `progress.rs`, `Request::Playtime/RemotePlaytime/GameProgress` |
| C9 | DLC: add-on ownership passed to the game, the Add-ons tab, store links | Built | the bridge's `InitReq` owned ids and CD keys, `ubi-app/ui/game/addons.rs` |
| C10 | Mods: layers, versioned mods, profiles, overlayfs, config patches, per-profile prefix folders, save stores | Built (MODS-V2) | `mods.rs`, `modversions.rs`, `overlay.rs`, `confpatch.rs` |
| C11 | Mod Options (MOD-CONFIG) | Designed only | `design/MOD-CONFIG.md` |
| C12 | Sight overlay and the pinned HUD | Designed only | `design/SIGHT.md`, `design/SIGHT-HUD.md` |
| C13 | CLI and agents: source labels, activity, approvals for "Ask" actions, settings history and restore | Built. Scoped tokens and MCP are designed only | `ubi-cli`, `ubid/info.rs::needs_approval`, `Request::Settings*/Activity` |
| C14 | Exports: Steam shortcuts, .desktop entries, Steam copies through `steam-run` | Built | `exports/*`, `steamrun.rs`, `design/STEAM-COPIES.md` |
| C15 | First run: the welcome, install folder, importing, Steam step, fixes | Built | `ubid/setup.rs`, `ubi-app/ui/welcome.rs` |

One structural fact matters more than any single capability. **Every daemon request is keyed
by `product_id: u32`** (`ubi-api/src/lib.rs`, `enum Request`), and so are the on-disk layout
(`games/<product>/…`, `mods/<product>/…`), exports (`quiver:<product id>/<profile>`) and
settings scopes. EA's identifiers are strings: offer ids like `Origin.OFR.50.0002694`, plus a
content id, a master title id and a multiplayer id [M2][M7]. EA can't land without a store
qualifier on game ids ([§3.1](#31-store-agnostic-interfaces-brief)).

---

## 3. Architecture

```
 quiver-app   quiver CLI   agents          (unchanged: ubi-api over quiverd.sock)
        \         |         /
         +---- quiverd ----+
         |  stores: ubisoft (ubi-core) · ea (ea-core) · …     ← StoreId on every GameKey
         |  shared: queue, prefixes, launcher, mods/overlays, save stores, history, activity,
         |          exports, Sight/HUD, play sessions
         |
         |  ea-core (host, Rust, MIT)
         |    nucleus    OAuth2 code + PKCE, refresh, token info, pc_sign      [M4][M5]
         |    gql        service-aggregation-layer GraphQL: owned games, offers, builds,
         |               downloadUrl, images, DLC, achievements, play times, subscription [M8]
         |    content    DiP manifest (__Installer/installerdata.xml), zip-over-HTTP-range
         |               downloader, CRC verify, touchup                          [M1]
         |    ooa        licence request to proxy.novafusion.ea.com, .dlf write   [M2]
         |    cloudsync  lock/read|write, manifest, authorize, transfer, release  [M7]
         |    lsx        LSX server: 127.0.0.1:<port>, XML + challenge, AES session [M3][O1]
         |
         |  per launch: registry keys in the prefix, licence refreshed, LSX session bound to
         |  this launch, cloud pull; on exit: cloud push, session end
         v
   proton run game.exe   (per-game prefix; no EA app, no Quiver .exe in the prefix)
        └── EA SDK in the game ──TCP 127.0.0.1:3216 (or EALsxPort)──▶ quiverd LSX
```

### 3.1 Store-agnostic interfaces (brief)

These are only what EA forces. STORES.md owns the real shape (`GameRef`, not `GameKey`).

- **`GameKey { store: StoreId, id: String }`** replaces `product_id: u32` in `Request`, in data
  paths (`games/ea-<offer>/…`; Ubisoft's existing `games/<id>/` paths stay as they are) and in
  export keys (`quiver:ea:<offer>/<profile>`; STORES.md section 2.2). Ubisoft stays the default store, so
  `quiver play origins` still works. This is the single largest change; every other
  interface depends on it.
- **`StoreAccounts`**: sign in, sign out, list, default, refresh, and "signed in elsewhere".
  EA's sign-in is a browser flow, not email and password ([§4](#4-auth-details)), so `Login`
  becomes a store-specific flow with a common `LoginStep` enum: `Password`, `TwoFactor`,
  `OpenUrl { url }` and `PasteRedirect`.
- **`StoreLibrary`**: owned entries (games, add-ons, subscription grants), each with an
  ownership source (`purchase | subscription | steam | epic | free`) and an expiry, plus art
  candidates and game info.
- **`StoreInstaller`**: plan (files, sizes, languages), fetch (resumable, sliced, rate-aware),
  verify (the store's own hashes), post-install steps (EA touchup), and recognising an
  existing install for import. The queue, free-space checks, pause and resume stay in
  `quiverd`.
- **`StoreLaunch`**: `prepare(game, account, prefix) -> LaunchPlan` (environment, args,
  registry writes, licences, a session-scoped service to start). Ubisoft returns the bridge,
  `UPLAY_ARGUMENTS` and the SDK DLL copy. EA returns its registry keys, the LSX binding and
  its env. A `SessionService` trait covers anything games talk to while running, such as the
  Ubisoft broker or EA's LSX, with a play-session lease like `playsession::Lease`.
- **`StoreCloudSaves`**: `pull` and `push` for one *cloud store* folder. Save stores (MODS-V2)
  stay generic. EA's locking fits inside the store implementation.
- **`StoreProgress`**: achievement definitions and state, remote play time, stats, and an
  optional `report_session` and `grant_achievement`.

Everything else (mods, config patches, overlays, settings history, Sight, HUD, exports,
activity, agent policy) is already store-agnostic in behaviour and needs only the `GameKey`
change.

### 3.2 The LSX service in quiverd

- **Transport.** A TCP listener on 127.0.0.1. Messages are XML elements `<LSX>…</LSX>`, each
  terminated by a null byte. On connect the server sends a `Challenge` event (sender `EALS`);
  the game answers with a `ChallengeResponse`, and after that both sides use AES-128 with a
  session key derived from the challenge [M3][O1]. Requests are grouped by domain: auth
  (`GetAuthCode`), license (`RequestLicense`), entitlements (`QueryEntitlements`,
  `QueryManifest`), game and config, profile, friends and presence, IGO (overlay),
  achievements (`GrantAchievement`, `QueryAchievements`), progressive install, and voip
  [M3][O1].
- **How a game finds it.** Through `HKLM\SOFTWARE\WOW6432Node\Origin` with `ClientPath` set:
  without that key some games never connect [M10]. Environment from the launch [M6]:
  `EALsxPort`, `ContentId`, `EAGameLocale`, `OriginSessionKey`, `EALaunchEAID`, the licence
  and auth token variables, and others. Quiver writes the registry keys when it creates the
  prefix, as it copies the Ubisoft SDK DLLs today.
- **Binding a connection to a launch.** Maxima runs one listener and matches connections to
  processes by scanning for `MXLaunchId` in the environment [M3]. Quiver should give each
  launch its own port through `EALsxPort`, so a connection *is* its session without any PID
  scan. That needs a spike: not every game is known to honour `EALsxPort` **(unverified)**.
  Port 3216 then becomes the fallback, with Quiver's systemd scope and cgroup (designed in
  PROPOSAL) as the tiebreak.
- **Tokens stay on the host,** the same rule as the Ubisoft broker. One difference: EA's
  launch environment carries access tokens (`EAGenericAuthToken`, `EAAccessTokenJWS`,
  `EALaunchUserAuthToken`) [M6], so a token does reach the game process, as it does with the
  real EA app. Quiver should pass the minimum each game needs (spike S4). It must also
  **redact these variables** from job details, the activity log and play-session records,
  which record launch environments today.
- **Play-session lease.** The LSX connection's lifetime is the session, as the Ubisoft
  bridge's lease is now. It feeds `PlaySessions` (local history, the HUD's session timer)
  and, once the API is known, EA's session reporting.

### 3.3 Not chosen: the EA app in Wine

How Lutris and Heroic users do it: install `EAappInstaller.exe` into a Wine or GE-Proton
prefix, sign in in its window, and launch games from it. Updates are the usual failure point,
and switching Wine builds without a fresh prefix breaks it [W16]. Heroic has no native EA
store. It supports EA games bought on the *Epic* store, where the EA app is bootstrapped into
the game's prefix [W17], and its wiki covers `link2ea://` handler failures under Wine-GE
[W18]. As a Quiver integration this path would give:

- **Sign-in, library, downloads, saves:** owned by the EA app's UI, so no CLI or agent
  surface, no queue and no save stores.
- **Prefixes:** one large shared prefix (Lutris style), or the EA app duplicated into every
  game prefix.
- **Licensing:** done by EA's own client, so it's unquestionably legitimate. This is its one
  real advantage.
- **Breakage:** follows every EA Desktop update; see the 2026 Proton fixes above.

Keep it as a **fallback runner**: a per-game setting `runner=ea-app` that launches the game
through an EA app installed in a dedicated prefix. It's for titles the native path can't
handle yet, such as pre-DiP manifests (Dead Space 2, Bad Company 2) and Battlelog games
(BF3, BF4), which Maxima doesn't support either [M1]. That is steer 1, and it's not MVP.

---

## 4. Auth details

### 4.1 Flow (from Maxima's implementation [M4][M5])

1. **Authorize.** Open `https://accounts.ea.com/connect/auth` in the user's browser with
   `client_id=JUNO_PC_CLIENT` (the EA app's client id), `response_type=code`, `locale`,
   `nonce`, `code_challenge_method=S256` with a PKCE `code_challenge`, and `pc_sign`. EA's own
   web page handles the password, captcha and 2FA, so Quiver never sees the password.
2. **Redirect.** EA redirects to the EA app's URI `qrc:/html/login_successful.html?code=…`.
   Maxima registers a `qrc:` handler, which forwards the query to a local listener on
   127.0.0.1:31033 [M9]. Maxima's maintainers support only this browser flow and have
   declined a browserless login [M12].
3. **Token.** `POST https://accounts.ea.com/connect/token` exchanges the code (with the PKCE
   verifier and the client's credentials, which are embedded in the EA app and public in
   Maxima's source; not reproduced here) for an access token, a refresh token and
   `expires_in`. Refresh uses the refresh token; Maxima refreshes 10 s before expiry [M5].
4. **Identity.** `GET https://accounts.ea.com/connect/tokeninfo` gives the user id, and
   GraphQL `GetBasicPlayer` and `GetUserPlayer` give the persona and display name [M8].
5. **Derived tokens.**
   - Nucleus auth-code exchanges for a game's own client id answer LSX `GetAuthCode`
     (multiplayer and game backends).
   - A short-lived "opaque" OOA token (550 s) is created per launch.
   - Both are minted by EA's servers from the user's session [M3][M6].

**Dead paths:** direct email and password login was "patched" by EA [M4]. Maxima's
`OnlineOffline` mode passes the email and password in the game's environment and in a licence
GET query string [M2][M6]. **Quiver never does either.**

### 4.2 `pc_sign` and the machine hash

- **`pc_sign`** is a JSON fingerprint signed with HMAC-SHA256 using one of two keys from the
  EA app. It carries the BIOS, motherboard and disk serials, the GPU device id, a MAC address,
  an FNV-1a "mid" hash and a timestamp [M5]. EA uses it at sign-in.
- **The OOA machine hash** (`machineHash`, from `HardwareInfo::generate_hardware_hash`) is
  sent with every licence request, and licences are bound to it [M2].
- **On Linux,** DMI serials (`/sys/class/dmi/id/board_serial`, `product_serial`) are root-only,
  so what Maxima reads there is **unverified**. What matters for Denuvo is that the value is
  **stable** and **consistent with what the game computes inside Wine**. If Quiver's hash
  changes when Proton or the prefix changes, every launch could become a new activation
  (spike S5, steer 5).

### 4.3 Storage and accounts

- **Storage.** Refresh tokens go in the Secret Service keyring under `quiver`, as
  `ea:<userId>` (STORES.md section 3.2), next to the Ubisoft entries. The account index goes in
  `accounts.json` with a `store` field. Nothing is stored in a plain file (Maxima uses
  `auth.toml` [M5]).
- **Several EA accounts** work as Ubisoft's do: merged library, per-game launch account, and
  cloud saves following the launch account.
- **Signed in elsewhere.** It's unknown whether EA enforces a single live session the way
  Ubisoft's demux does **(unverified)**. If refresh starts failing with `invalid_grant`, mark
  the account "sign in again" and never retry in a loop.
- **Agent policy is unchanged.** Signing in and out is "Ask" from the CLI and "Deny" for
  agents. The browser flow is interactive anyway.

### 4.4 Where the licensing line is for EA

This is the hard constraint, made concrete. **Allowed:** being the EA app's local service
honestly, with ownership decided by EA's servers for the user's own account.

| Allowed (ownership checked by EA) | Not allowed (forging or circumvention) |
|---|---|
| Request each OOA licence from EA's licence server with the user's token and the real machine hash, then write the `.dlf` **exactly as received** to `C:\ProgramData\Electronic Arts\EA Services\License\<contentId>.dlf` [M2] | Creating, editing, re-signing or "extending" a licence; re-encrypting edited XML; using the embedded OOA AES key to read or write licence contents (Quiver tracks the fetch time itself, steer 4) |
| Relaying the game's Denuvo request ticket (`RequestLicense`) to EA and returning EA's `game_token` [M3] | Injecting a token from elsewhere, as Maxima's `MAXIMA_DENUVO_TOKEN` override does [M3]; reusing one account's token for another account's game |
| Answering `QueryEntitlements` from EA's entitlement list for the launch account (GraphQL `sdkEntitlements`) [M8] | Answering with entitlements EA didn't return. That is exactly what "DLC unlockers" do, by fooling the EA app and game into believing you own the whole catalog [W19] |
| A stable machine hash derived from this machine | Randomising or rotating the hash to dodge Denuvo or OOA activation limits; copying licences between machines |
| Adding a free or subscription offer with `grantEntitlement`, which **EA's server validates** (`GrantEntitlementNotFreeOrNoAppropriateSubscriptionError`) [M8], only on the user's explicit action | Claiming offers automatically, or for accounts other than the one that asked |
| Offline play with licences EA issued, until EA's expiry | Patching `activation.dll`/`Core/` or the game executable; anything in the game's DRM code; making the game skip its licence check |
| `GetAuthCode` minted by Nucleus for the requesting game's client id [M3] | Returning placeholder codes to get past the game's online checks. Maxima falls back to the literal `"invalid"` [M3]; Quiver returns an LSX error instead |

The reverse-engineering clause of EA's ToS [W15] applies to the whole native approach (and
to Maxima, origin-sdk and Quiver's Ubisoft work). [§7](#7-tos-ban-risk-and-eas-history-of-breaking-things)
weighs that. The table above is the line Quiver won't cross *within* that.

---

## 5. Capability matrix

Effort: **S** is days, **M** is one to three weeks, **L** is more than a month of focused work.
"Prior art" names the source we'd follow.

| # | Capability | EA approach | Prior art | Effort | Risk |
|---|---|---|---|---|---|
| C1 | Account and auth | Nucleus OAuth2 code + PKCE in the system browser, with a `qrc:` redirect capture or a pasted redirect URL; refresh token in the keyring; several accounts | Maxima `core/auth` [M4][M5] | M | Med: EA has changed login before; `pc_sign` keys may rotate; hardware serials |
| C2 | Library sync, art, metadata | GraphQL `getPreloadedOwnedGames`, `getLegacyEntitlements`, `GetGameProductOfferIds`, `FullGameOverview`, `GameImages`, `GetHeroBackgroundImage`, `GetGameSystemRequirements`; Contentful proxy for store content [M8] | Maxima `core/library.rs`, `service_layer.rs` | M | Low–Med: GraphQL persisted queries can change shape |
| C3 | Install, download | `availableBuilds` + `downloadUrl` → one zip on EA's CDN, fetched by HTTP range per entry; DiP manifest `__Installer/installerdata.xml`; run `Touchup.exe` in the prefix (registry, redists) [M1][M8] | Maxima `content/` (zip, zlib, downloader), `core/manifest` | L | Med: download URL rate limit `CG_LIMIT_EXCEEDED` lasted about a day after about five repeated downloads [M13]; pre-DiP games unsupported |
| C3 | Update | DiP version check against `availableBuilds`; re-fetch the entries whose CRC differs (zip central directory) **(unverified that EA updates are per-file)** | Maxima "downloading and updating" [M1] | M | Med |
| C3 | Verify and repair | CRC32 from the zip's central directory against local files; repair re-fetches bad entries. Fits Quiver's verify model | none complete | M | Low |
| C3b | Move, uninstall | Local; rewrite the prefix's install registry keys; uninstall also removes the `.dlf` licences | Quiver (store-agnostic) | S | Low |
| C3 | Import an EA app, Maxima or Lutris install | Read `installerdata.xml` and the EA registry keys in the prefix, match the offer, then verify | Maxima "locating games" [M1] | M | Low |
| C4 | Per-game prefixes | Unchanged; at prefix creation add `HKLM\SOFTWARE\WOW6432Node\Origin\ClientPath` and the EA Desktop keys [M10]. No EA app in the prefix | Maxima PR #70 adds per-game prefixes too [M11] | S | Low |
| C5 | Launch with licensing (OOA, Denuvo) | Before launch: refresh the licence if stale (EA licence server). During: the LSX server in quiverd answers `RequestLicense` (Denuvo), `GetAuthCode`, `QueryEntitlements`, config and profile ([§3.2](#32-the-lsx-service-in-quiverd)) | Maxima `ooa/`, `lsx/`; origin-sdk types [M2][M3][O1] | L | **High**: per-title quirks, SDK versions in game updates, Denuvo activation counting (S5) |
| C6 | Cloud saves | `cloudsync.juno.ea.com`: lock read or write → XML manifest (`href`, `md5`, `localName` with `%Documents%`/`%SavedGames%`) → authorize → GET or PUT → release; includes come from the offer's `cloud_save_configuration_override` [M7]. Maps onto save stores: the cloud store is the folder set the manifest names | Maxima `core/cloudsync.rs` (has known bugs: no truncate on download, hex-vs-base62 md5 mismatch [M7]) | M | Med: a bad sync can lose saves; Quiver's backup-before-sync rule applies |
| C7 | Achievements: read | GraphQL `gameAchievements`, `ownedGameAchievements` (name, description, `awardCount`, images) [M8] | Maxima queries | S | Low |
| C7 | Achievements: earn | Games call LSX `GrantAchievement` and `PostAchievementEvents` [O1]; how the EA app forwards these to EA is **unknown**. MVP: keep a local earned set, as the Ubisoft bridge plan does (SIGHT §7) | none (Maxima has no achievements handler) | M after capture | Med: needs traffic capture of the EA app (S6) |
| C8 | Play time and progression | Read: GraphQL `GetGamePlayTimes`. Report: `gameSessionEnd` mutation [M8], plus a start or heartbeat **(unverified; maybe RTM presence)**. Local history as today | Maxima `rtm/`, GraphQL | S read, M report | Med (report side unknown) |
| C9 | DLC: ownership | `QueryEntitlements` from EA's entitlements (`sdkEntitlements`, `GetDlcsForGame`); Add-ons tab from `addonSearch`; store links | Maxima `GetDlcsForGame` [M8] | M | Low |
| C9 | DLC: installs (Sims 4 packs, extra content) | Separate offers with their own builds, installed into the base game folder **(unverified layout)** | Maxima: planned only [M1] | L | Med |
| C9b | EA Play subscription | `GetSubscriptionStatus`; vault titles via `grantEntitlement` (server-validated); licences need periodic re-verification [W20]. EA Play Pro is "EA app only" on PC [W20], so whether Pro entitlements reach a third-party client is **unverified** | Maxima GraphQL | S–M | Med |
| C10 | Mods and profiles | Unchanged (overlayfs over the install, config patches, per-profile prefix folders). EA examples: Sims 4 `Documents/Electronic Arts/The Sims 4/Mods` as a per-profile prefix folder; Frostbite games through Frosty's `-dataPath ModData` as a launch arg plus files **(unverified under Wine)**; KYBER uses Maxima directly [W14] | Quiver | S | Low (online titles: anti-cheat ToS) |
| C11 | Options (MOD-CONFIG) | Unchanged, store-agnostic | Quiver design | — | — |
| C12 | Sight and HUD | HUD unchanged. Panel: map LSX IGO requests (overlay open, store, browser URL, friends) as SIGHT §7 maps `OverlayShowReq`; answer "unavailable" until a section exists, so no game waits | Maxima `lsx/request/igo.rs` | S–M | Low; Sight's layer stays off for anti-cheat titles |
| C13 | CLI and agents | `--store ea` where a name is ambiguous; same verbs. Agent policy: sign-in Deny; `grantEntitlement` Ask; licence refresh System | Quiver | S | Low |
| C14 | Exports and Steam/Epic copies | .desktop and Steam shortcuts: unchanged. **Steam copies of EA games** (Mass Effect LE, Jedi, Titanfall 2 and others start the EA app via `link2ea://` [W18]): a `steam-run --store ea` launch option that brings up quiverd's LSX instead. How EA validates a Steam-sourced entitlement over LSX is **unverified**. Some titles no longer need the EA app on Steam (It Takes Two, Veilguard, BF6) [W21][W22][W23] | STEAM-COPIES pattern; Maxima "Epic/Steam" in development, `link2ea` `todo!()` [M9] | L | Med–High |
| C15 | First run | Welcome gains "Add an EA account" (browser flow), "Import EA installs" (scan prefixes for `installerdata.xml`) and a note on anti-cheat titles | Quiver | S | Low |
| — | Anti-cheat titles | Detect Javelin (`EAAntiCheat.GameServiceLauncher.exe` or a curated list) and show "Not playable on Linux: EA Javelin Anticheat"; Play is disabled unless forced (steer 7) | GOL anti-cheat database [W8] | S | — (unfixable by Quiver) |

---

## 6. Phased plan, MVP first

**Phase 0: spikes.** These are scratch binaries outside the product crates. Each one that
touches EA needs the user to sign in themselves, and asks first, per the real-prefix rule.

| Spike | Question | Done when |
|---|---|---|
| S1 auth | Does the browser flow work for Quiver: `qrc:` handler vs pasted URL, and refresh lifetime? | Tokens in the keyring survive a day, and refresh works |
| S2 library | Do the GraphQL queries return the user's EA, Steam-linked and EA Play entries, with ids and art? | A dump matches what the EA web library shows |
| S3 content | Download and verify one small DiP game; run touchup in a per-game prefix | It installs, and CRC verify is clean |
| S4 LSX | Does one OOA game reach its menu against quiverd's LSX with a fresh licence? Which env variables are actually required? Is `EALsxPort` honoured? | A logged LSX transcript, written up like the Ubisoft bridge notes |
| S5 machine hash | Is the hash stable across Proton versions and prefixes, and does it match what the game computes in Wine? Test on a non-Denuvo OOA title first | Two Proton versions, one licence, no re-activation |
| S6 capture | What does the EA app send for achievements and session start or heartbeat? (The EA app is Qt, so the secur32 tap probably doesn't apply; maybe a proxy with a prefix CA) **(unverified)** | Endpoints written down, or the item marked unknowable |

**Phase 1: MVP (one EA account, own-store games).**

- The `GameKey` refactor across `ubi-api`, `quiverd` and storage, with Ubisoft paths left
  unchanged (no migration).
- `ea-core`: nucleus, gql (library, art, info), content (DiP install, update, verify),
  ooa, and the LSX server (license, auth code, entitlements, config, profile; everything
  else answered with an error so no game blocks).
- Library tiles with an EA store badge, install queue, Play in a per-game prefix, local play
  time, and the CLI (`quiver login --store ea`, `quiver library`, `quiver install`,
  `quiver play`).
- Anti-cheat detection and the "Not playable on Linux" state.
- Exit criteria: one OOA game and one Denuvo game played twice across a Proton change without
  an activation error, and verify clean after an update.

**Phase 2: parity for owned games.**

- Cloud saves through save stores (backup before every sync).
- Achievements and play time read from EA.
- DLC ownership through `QueryEntitlements` and the Add-ons tab.
- EA Play badges, expiry, and claim on explicit action.
- Several EA accounts; import of EA app, Maxima and Lutris installs; move.
- Mods and config patches for EA games, plus Sims 4 Mods as a per-profile prefix folder.

**Phase 3: the hard edges.**

- DLC installs (Sims 4 packs).
- Steam copies (`steam-run --store ea`) and Epic copies (with STORE-EPIC).
- Achievement and session reporting to EA (if S6 succeeds).
- Sight IGO mapping and friends and presence over RTM.

**Later.** The EA app fallback runner for pre-DiP and Battlelog titles (steer 1), and
pre-DiP manifests.

---

## 7. ToS, ban risk and EA's history of breaking things

**ToS.** EA's User Agreement (updated 14 May 2026) says:

- "You may not reverse engineer or attempt to extract or otherwise use source code".
- Attempting "to circumvent, disable or tamper with these measures shall terminate this
  license".
- An "Unauthorized Third-Party Program" is anything EA believes enables cheating, "allows
  users to bypass, modify or hack the game interface, environment, protections and/or
  experience", or "intercepts, 'mines', or otherwise collects information from or through
  the game".
- EA "may suspend or terminate your use of our games and services" [W15].

Penalties are locks, suspensions and bans, tracked cumulatively [W24].

**Reading:**

- A native EA client is reverse engineering. It identifies itself as the EA app
  (`JUNO_PC_CLIENT`, `pc_sign`), which is the same posture as Quiver's Ubisoft client using
  Connect's AppId.
- It doesn't circumvent protection measures if it stays on the "allowed" side of
  [§4.4](#44-where-the-licensing-line-is-for-ea).
- Mods remain "unauthorized" in EA's terms. For single-player games that is enforcement
  theory. For online titles it's a live ban risk, and Quiver should never put mods or the
  Sight layer into an online anti-cheat title.

**Ban risk in practice.** Maxima has been public since 2023 and is used by KYBER's player
base [W14]. I found no reports of EA actioning accounts for using it **(unverified absence;
Maxima's issue tracker shows rate limits [M13] but no bans)**.

The realistic account-level risks are different:

- **Denuvo lockouts.** About 5 activations per 24 h; Proton, `WINE_CPU_TOPOLOGY` or launch
  option changes have been reported to count as new machines [W13].
- **Download rate limits** [M13].
- **Licence or login breakage** after an EA change.

**EA's history of breaking third-party and Linux paths:**

- Direct password login was patched, and Maxima's function is now `unimplemented!()` [M4].
- The OOA request needed an "opaque token on invalid machine hash" fix (Maxima, June 2025),
  and licence-update handling changed again in January 2026 [M14].
- Origin was shut down on 17 April 2025, forcing the move to the 64-bit EA app [W4].
- EA Desktop updates broke "many EA games" under Proton in January 2026 [W10].
- Apex Legends was blocked on Linux by policy in October 2024 [W5].

Expect a breaking change on EA's side every few months. That is why the design keeps EA
behind its own crate and spike-tested transcripts, and why Quiver treats LSX transcripts the
way it treats the Ubisoft SDK proxy logs: a regression corpus.

---

## 8. Open questions and steers

Each steer lists the options with my recommendation marked **(rec)**.

1. **Main path: native LSX or the EA app in Wine.**
   (a) Native: ea-core plus LSX in quiverd. **(rec)**
   (b) The EA app in a shared prefix.
   (c) Native, plus a per-game `runner=ea-app` fallback later for pre-DiP and Battlelog titles.
   Rec: (a) now, (c) after Phase 2 only if a title the user owns needs it.
2. **Maxima's code and its GPL-3.0 licence.**
   (a) Link maxima-lib (the shipped quiverd becomes GPL-3; relicense Quiver).
   (b) Drive `maxima-cli` as a subprocess (arms-length, but its single-account, panicking
   model leaks into Quiver).
   (c) Write Quiver's own MIT implementation using Maxima only as a protocol reference and
   test oracle, optionally depending on origin-sdk (MIT) for LSX types. **(rec)**
   Note: Maxima's maintainers said they'd reject PRs with "excessive LLM usage" [M10], so
   upstreaming agent-written fixes isn't a reliable channel.
3. **Capturing the sign-in redirect.**
   (a) Register an `x-scheme-handler/qrc` .desktop handler that forwards to quiverd, as
   Maxima does. It's system-wide and clashes with any real Qt `qrc:` use.
   (b) Show the URL, then the user pastes the final redirect URL from the browser into the
   app or CLI.
   (c) A small WebKitGTK sign-in window that intercepts the redirect.
   Rec: (b) for the MVP (works for CLI and app, no system registration), with (a) as an
   opt-in convenience.
4. **Licence files.**
   (a) Never decrypt: write EA's bytes as received and refresh on Quiver's own clock (fetch
   time, plus a re-fetch when the game reports a licence error). **(rec)**
   (b) Embed EA's OOA AES key to read `StartTime` and expiry, as Maxima does.
   Rec: (a). It keeps Quiver clearly outside "tampering", and Maxima's own two-week refresh
   is labelled a guess [M2].
5. **Machine identity for `pc_sign` and the OOA hash.**
   (a) The real hardware values, read as the EA app would see them under Wine, computed once
   and stored.
   (b) The real values read natively on Linux (DMI serials need root).
   (c) A per-machine value derived from `/etc/machine-id`.
   Rec: decide after S5. Whatever is chosen must be stable per machine and never varied to
   avoid limits. Tell the user that hardware serials are sent to EA, as the EA app does.
6. **MVP titles.** Which EA games do you own, and on which store (EA app, Steam, Epic, EA
   Play)? The MVP needs one OOA title and one Denuvo title from the EA app store. Rec: pick
   from your library; avoid Javelin titles.
7. **Anti-cheat titles.**
   (a) Hide them.
   (b) Show "Not playable on Linux (EA Javelin Anticheat)" with Play disabled and
   `quiver play --force` available.
   (c) Show them and let launches fail.
   Rec: (b), with the list kept as data (like built-in mods) and the reason linked.
8. **EA Play.**
   (a) Show subscription titles as library entries with expiry and claim only on explicit
   user action (agents: Ask). **(rec)**
   (b) Show only claimed titles.
   (c) Ignore EA Play.
9. **Steam and Epic copies of EA games.**
   (a) Phase 3, after an LSX-over-Steam spike, reusing STEAM-COPIES and STORE-EPIC.
   **(rec)**
   (b) Part of the MVP.
   (c) Never; Steam's own path plus the EA app.
10. **Traffic capture of the EA app** (for achievements and session reporting).
    (a) Allowed later in a scratch prefix with your account, asked first and reverted after,
    like the Connect tls-tap. **(rec)**
    (b) Not allowed: Quiver keeps achievements and play time read-only and local.
11. **Reporting achievements.** Until the grant API is known:
    (a) A local earned set per account and game, shown in Sight and the app, never sent.
    **(rec)**
    (b) Not handled at all.
12. **Store qualifiers in names and ids.**
    (a) `GameKey { store, id }` everywhere, with Ubisoft as the default store so existing
    commands and paths keep working. **(rec; the exact shape is in STORES.md section 2)**
    (b) Map EA offers into a u32 namespace.
13. **Old titles.** Out of scope for EA, or the fallback runner: pre-DiP manifests (Dead
    Space 2, Bad Company 2) and Battlelog games (BF3, BF4)? Rec: out of scope until steer 1(c).

**Unverified items to settle by spike:**

- Whether `EALsxPort` is honoured by all games.
- Whether EA updates are per-file.
- The DLC install layout.
- How EA validates Steam-sourced entitlements over LSX.
- Session reporting beyond `gameSessionEnd`.
- Whether EA Play Pro entitlements reach a third-party client.
- How the machine hash is read under Wine.
- Whether EA enforces one live session per account.
- Frosty under Wine.

---

## Sources

Fetched 2026-11-16. Maxima source files are from `ArmchairDevelopers/Maxima@master`.

Maxima and EA client internals:
- [M1] Maxima README and repository metadata (GPL-3.0; created 2023-08, last push 2026-08; feature and "known unsupported" lists): https://github.com/ArmchairDevelopers/Maxima
- [M2] OOA licence client (`proxy.novafusion.ea.com/licenses`, `.dlf` layout, licence path, two-week guess): https://github.com/ArmchairDevelopers/Maxima/blob/master/maxima-lib/src/ooa/mod.rs
- [M3] LSX connection, licence and auth handlers: https://github.com/ArmchairDevelopers/Maxima/tree/master/maxima-lib/src/lsx
- [M4] Endpoints, client id, login: https://github.com/ArmchairDevelopers/Maxima/blob/master/maxima-lib/src/core/endpoints.rs, `core/clients.rs`, `core/auth/login.rs`
- [M5] Auth context (PKCE, `pc_sign`), token storage and refresh: https://github.com/ArmchairDevelopers/Maxima/tree/master/maxima-lib/src/core/auth
- [M6] Launch environment and licence timing: https://github.com/ArmchairDevelopers/Maxima/blob/master/maxima-lib/src/core/launch.rs
- [M7] Cloud sync protocol: https://github.com/ArmchairDevelopers/Maxima/blob/master/maxima-lib/src/core/cloudsync.rs
- [M8] GraphQL operations (library, builds, `downloadUrl`, achievements, play times, `gameSessionEnd`, `grantEntitlement`, `sdkEntitlements`, subscription): https://github.com/ArmchairDevelopers/Maxima/tree/master/maxima-lib/src/core/graphql
- [M9] Bootstrap (`qrc:` login hand-off, `link2ea` `todo!()`, `origin2`): https://github.com/ArmchairDevelopers/Maxima/blob/master/maxima-bootstrap/src/main.rs
- [M10] Issue #71: the `WOW6432Node\Origin\ClientPath` key, and the maintainer's stance on LLM PRs: https://github.com/ArmchairDevelopers/Maxima/issues/71
- [M11] PR #70: rework with per-game prefixes, umu, LSX refactor; open: https://github.com/ArmchairDevelopers/Maxima/pull/70
- [M12] Issue #53: browserless login not planned: https://github.com/ArmchairDevelopers/Maxima/issues/53
- [M13] Issue #57: `CG_LIMIT_EXCEEDED`, about a day to reset: https://github.com/ArmchairDevelopers/Maxima/issues/57
- [M14] Commit history (opaque OOA token fix 2025-06; licence update handling 2026-01): https://github.com/ArmchairDevelopers/Maxima/commits/master
- [O1] origin-sdk (MIT, LSX client from XSDs; challenge and AES-128-ECB; protocol modules including achievements and entitlements): https://github.com/ploxxxy/origin-sdk, https://docs.rs/origin-sdk/0.1.2/origin_sdk/protocol/index.html

EA, Linux and policy:
- [W1] LSX port 3216 (EA forums): https://forums.ea.com/discussions/battlefield-v-en/bfv-not-starting-lsx-server-port-3216/6781209
- [W4] Origin shutdown, 17 April 2025: https://www.tomshardware.com/video-games/pc-gaming/ea-will-shut-down-the-origin-app-on-april-2025-company-asks-users-to-migrate-to-the-new-ea-app
- [W5] Apex Legends blocks Linux and Steam Deck: https://www.gamingonlinux.com/2024/10/ea-respawn-now-block-apex-legends-from-running-on-linux-and-steam-deck
- [W6] Battlefield 6 is not playable on Linux: https://www.gamingonlinux.com/2025/10/battlefield-6-releases-today-and-it-will-not-be-playable-on-any-linux-steamos-system
- [W7] Javelin rebrand, still blocks Linux: https://gamingonlinux.com/2025/04/ea-rebrand-and-refresh-their-anti-cheat-into-ea-javelin-anticheat-still-blocks-linux-steam-deck
- [W8] GamingOnLinux anti-cheat database, EA Javelin: https://www.gamingonlinux.com/anticheat/vendor/ea-javelin-anticheat/
- [W9] Javelin job listing mentions future Linux and Proton support (March 2026): https://www.gamingonlinux.com/2026/03/ea-javelin-anticheat-job-listing-mentions-future-support-for-linux-and-proton/
- [W10] Proton Experimental fixes the EA app again (January 2026): https://www.gamingonlinux.com/2026/01/proton-experimental-updated-to-fix-the-ea-app-again-on-steamos-linux
- [W11] Proton 11.0-2e EA app fixes (October 2026): https://steamdeckhq.com/new-small-proton-update-fixes-ea-app-issues/
- [W12] Proton stable's EA app fixes: https://mixed-news.com/en/proton-11-0-2e-first-stable-release-since-august-ea-app/
- [W13] Denuvo lockouts from changing Proton versions: https://www.gamingonlinux.com/2025/05/denuvo-will-lock-you-out-of-games-on-linux-steamos-steam-deck-if-you-keep-changing-proton-versions
- [W14] KYBER goes open source; uses Maxima (January 2026): https://www.gamingonlinux.com/2026/01/star-wars-battlefront-ii-private-server-and-mod-launcher-kyber-goes-open-source
- [W15] EA User Agreement (updated 14 May 2026): https://tos.ea.com/legalapp/WEBTERMS/US/en/PC/
- [W16] Lutris EA app installation problems: https://forums.lutris.net/t/ea-app-not-installing/22481
- [W17] Heroic 2.15: EA games from the Epic store: https://www.gamingonlinux.com/2024/08/heroic-games-launcher-v215-has-expanded-gog-support-ea-games-from-epic-store-support
- [W18] Heroic wiki, "How to install EA Games on Linux" (`link2ea` under Wine-GE): https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/wiki/How-to-install-EA-Games-on-Linux (read through a search snippet of a mirror)
- [W19] What a DLC unlocker does (cited only as the anti-pattern): https://anadius.pages.dev/ea-dlc-unlocker
- [W20] EA Play and EA Play Pro on PC (EA help): https://help.ea.com/en/help/ea-play/ea-play-for-pc/, https://help.ea.com/en/help/ea-play/upgrade-to-ea-play-pro-for-pc/
- [W21] It Takes Two no longer needs the EA app on Steam: https://www.gamingonlinux.com/2024/05/it-takes-two-now-steam-deck-verified-ea-app-launcher-no-longer-required
- [W22] Battlefield 6 drops the EA app requirement on Steam: https://www.windowscentral.com/gaming/battlefield-6-scraps-a-decade-old-requirement-on-steam
- [W23] Veilguard won't require the EA app on Steam: https://primagames.com/news/dragon-age-the-veilguard-wont-require-ea-app-on-steam
- [W24] EA account suspension policy: https://help.ea.com/help/account/electronic-arts-account-suspension-policy
