# How Ubisoft Connect reads and reports achievements (desk research, 2026-11-16)

Goal: find, without any live call, how an achievement **unlock** is reported upstream and how
unlock **status** is read, so Quiver can report unlocks the bridge currently only logs
(`EarnAchievementReq`) and show per-user unlock state.

Evidence: `proto/` (vendored from upc.exe), Quiver's demux client (`crates/ubi-core/src/demux.rs`)
and progress code (`crates/ubi-core/src/progress.rs`), the Connect UI bundle (`uicommon.pak`),
and **strings extracted from the user's own `upc.exe` after unpacking it offline** (run in a
throwaway Wine prefix inside a network namespace with no connectivity — no network calls, no
login, nothing written to the user's prefix). upc.exe is packed (`.ubx*` sections); the readable
strings only appear after the real process self-unpacks in memory, which is why the plain
`strings upc.exe` earlier found nothing.

## Two separate mechanisms exist. Connect uses both.

### A. NEW — UbiServices REST "legacyPlayerAchievements" API  ← most likely path for ACO/India/Origins

upc.exe source files (from the embedded Jenkins paths) and their strings:

- `AchievementsGetEarnedFromApi.cpp` — **READ unlock status**
  `GET /legacyPlayerAchievements?spaceId=<spaceId>`
  Response JSON schema (verbatim from upc.exe):
  ```
  { "legacyPlayerAchievements": [ {
      "achievementLegacyId": int, "spaceId": str, "profileId": str,
      "completed": bool, "displayName": str, "displayDescription": str, "iconAssetUrl": str
  } ] }
  ```
- `AchievementsPostEarnedToApi.cpp` — **REPORT an unlock**
  `POST /legacyPlayerAchievements/completions`
  body carries `idempotencyId` and a completion with an ISO timestamp
  (`{:%Y-%m-%dT%H:%M:%OS.0000000Z}`). Success log: "Game's legacy achievements are updated".
- Host: `public-ubiservices.ubi.com` (prod). The `/legacyPlayerAchievements...` suffix is
  appended at runtime to a profiles base; the exact prefix (`/v1/profiles/{profileId}/...` vs a
  dedicated base) was **not** a single literal in the dump — this is the one thing a v2 capture
  should confirm.
- **Auth is the sticking point.** Besides the usual UbiServices headers (`Authorization: Ubi_v1
  t=<ticket>`, `Ubi-AppId`, `Ubi-SessionId`), the **completions POST is additionally signed**:
  `AchievementsPostEarnedToApi` computes a hash over the message ("Failed to use hash function on
  message") and sends **`Ubi-Signature`** + **`Ubi-Signature-timestamp`** headers. OpenSSL
  HMAC-SHA256 is linked in. Quiver will have to reproduce this signature (key/derivation unknown
  from strings); the header **values** would be visible in a v2 TLS capture of this POST.
- Steam/Epic linked accounts register separately: `POST .../playerAchievements/registrations/steam`
  and `.../epic` (not needed for Quiver-native).

### B. LEGACY — demux `ach_frontend` service, package `mg.protocol.uplay` (proto/proto_uplay/Uplay.proto)

- Service name on the demux socket is **`ach_frontend`** (string sits right beside
  `AchievementsConnectionHandler.cpp` in the dump, in the same block as `download_service`,
  `cloudsave_service`, …). Quiver's demux client opens services by name exactly like this
  (`ServiceConnection`, `demux.rs`); `ach_frontend` is simply not in Quiver's `service!(...)`
  list yet.
- Messages (`Uplay.proto`, already compiled into `ubi-proto` would need a module add):
  - `Req { AuthenticateReq(orbit_token) | ReadAchievementsReq | WriteAchievementsReq }`
  - `ReadAchievementsReq { user_id, Product{product_id, platform_id} }`
    → `ReadAchievementsRsp { user_id, AchievementBlob{ ProductAchievements[]{ Product,
      Achievements{ Achievement{ achievement_id, timestamp } } } } }`  ← **read unlock status**
  - `WriteAchievementsReq { AchievementBlob }` → `WriteAchievementsRsp {}`  ← **report unlock**
  - Auth on this connection is `AuthenticateReq.orbit_token` (the orbit/ubi token), i.e. the same
    socket-level ticket Quiver already passes to `Demux::authenticate`.
- This path is keyed by the product config field **`achievements_sync_id`** ("is missing
  achievements sync id or uplay game code", "Sync id should be set"). Games "upgraded_from_legacy"
  carry it. Note `mg.protocol.uplaydll.WriteAchievementsReq` (game→upc, different package) is the
  **game side**; `mg.protocol.uplay.WriteAchievementsReq` is upc→demux.

### Which path for ACO Origins (1847) / Chronicles India (3539)?

The REST `legacyPlayerAchievements` API is the newer unified one and is the most probable report
path for both; `ach_frontend`/`WriteAchievements` is the fallback for titles that still have an
`achievements_sync_id`. A v2 capture (match-all tap, binary demux decode) settles which is used
and — crucially — reveals the `Ubi-Signature`/`Ubi-Signature-timestamp` values and the exact REST
prefix. v1 missed all of this because its filter only kept `public-ubiservices` REST whose
plaintext matched a keyword and skipped binary demux frames.

## Game → upc side (already partly in Quiver)

Game calls `UPLAY_ACH_EarnAchievement` → upc `AchievementsEarn.cpp` sets the earn in a local cache
+ spool, then `AchievementsSync.cpp` → `AchievementsPostEarnedToApi` (REST) and/or the
`ach_frontend` `WriteAchievements`. In Quiver this arrives at the bridge as
`mg.protocol.uplaydll.EarnAchievementReq{ id }` (`crates/ubi-bridge/src/respond.rs::achievements`,
TODO at line 426). `GetAchievementsReq` → `GetAchievementsRsp{ AchievementData{ id, name,
description, isEarned, timestamp } }` is how the bridge should answer the game's read; today it
returns an empty list.

## What Quiver fetches today (no unlock state)

`progress.rs` reads only **definitions** (names/descriptions/icons from the product-config
`uplay.achievements` zip on the asset CDN) and the UbiConnect economy/rewards/challenges REST.
`Achievements.unlocked_known = false`. Neither READ path above (REST legacyPlayerAchievements, or
demux `ach_frontend` ReadAchievements) is implemented.

## Recommendation for the orchestrator's goal (report Origins ach 59)

Highest-confidence implementable path **without a capture**: demux `ach_frontend` +
`mg.protocol.uplay.WriteAchievementsReq` — it reuses Quiver's existing demux socket and
orbit-token auth, no request signing. Add an `ach_frontend` service to `demux.rs`, a
`ubi::protocol::uplay` module to `ubi-proto`, and have the bridge's `EarnAchievementReq` handler
send `WriteAchievementsReq{ AchievementBlob{ ProductAchievements{ Product{product_id=<ACO space's
product>, platform_id}, Achievements{ Achievement{ achievement_id=59, timestamp=now } } } } }`.
Risk: `ach_frontend` may reject modern titles that use the REST path (would time out silently, as
demux does). The REST `legacyPlayerAchievements/completions` path is more likely correct but needs
the `Ubi-Signature` scheme, which only a capture reveals. Suggest: try `ach_frontend` first (cheap,
offline-designable); fall back to the v2 capture to get the signature if the demux write is
ignored.
