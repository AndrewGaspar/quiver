# Quiver as a multi-store launcher (synthesis)

Status: design only (2026-11-16). Nothing here is built. This document reconciles the five store
scopes written in parallel: `STORE-STEAM.md`, `STORE-EPIC.md`, `STORE-GOG.md`, `STORE-EA.md` and
`STORE-META.md` (the Meta PC VR store). It owns the cross-store architecture. The store docs keep
their store-specific detail, spikes and store-only steers. Where a store doc disagreed with the
reconciled design, it was corrected in the same commit (section 10).

Conventions: **effort** is normalised across the five docs, which used slightly different scales:
**S** is days, **M** is one to three weeks, **L** is a month or more. **Risk** combines technical,
policy and data-loss risk (Low, Med, High). **(unverified)** keeps its meaning from the store docs;
each such claim has a spike in section 8.4.

---

## 1. Summary

**Quiver stays one daemon with one queue, one launcher and one library. Each store becomes a
provider behind a small set of traits, and every game is named by a store-qualified `GameRef`.**
Ubisoft is the first provider and keeps its existing ids, paths, export keys and Steam shortcut
appids exactly as they are.

1. **Identity.** `GameRef { store, id }`, written `epic:Camellia`. A bare number still means a
   Ubisoft product, and every key Ubisoft data already uses stays bare. New stores are namespaced:
   `games/epic-Camellia/` on disk, `quiver:epic:Camellia/<profile>` as an export key.
2. **Providers.** Each store implements the same optional traits: `Accounts`, `Library`,
   `Installer` (plus `ContentSource` where Quiver fetches the bytes), `Launch` (returning a
   `LaunchPlan` that names its `RuntimeService`s), `CloudSaves`, `Progress`, `Playtime` and
   `Entitlements`. The queue, prefixes, mods, Options, Sight, the HUD, exports, activity and agent
   policy stay shared and never branch on the store.
3. **Licensing** is always the store's real backend deciding, for the user's own account. One
   policy covers every store (section 7).
4. **Order.** The refactor first, in slices that keep Ubisoft working. Then: Steam local mode;
   Epic library and activation of Ubisoft titles; GOG; Steam around Steam's launch; Epic native
   installs; EA; Meta (reasoning in section 8.2).

---

## 2. Game identity

### 2.1 The type

```rust
// quiver-api (today ubi-api)
pub enum StoreId { Ubisoft, Steam, Epic, Gog, Ea, Meta }   // "ubisoft", "steam", "epic", "gog", "ea", "meta"

/// One store's record of a game. Display form "epic:Camellia"; a bare number is Ubisoft.
pub struct GameRef { pub store: StoreId, pub id: GameId }

/// Validated: [A-Za-z0-9][A-Za-z0-9._-]{0,127}. Safe in paths, URLs, VDF and env values.
pub struct GameId(String);

impl GameRef {
    pub fn ubisoft(id: u32) -> Self;
    pub fn as_ubisoft(&self) -> Option<u32>;
    pub fn key(&self) -> String;       // "3539" | "epic:Camellia"   (export keys, settings scopes)
    pub fn dir_name(&self) -> String;  // "3539" | "epic-Camellia"   (data folders)
}
```

**A struct with a string id, not an enum of typed ids** (STORE-EPIC proposed
`enum GameKey { Ubisoft(u32), Epic(EpicApp), … }`). Shared code (paths, exports, mods, Options,
Sight, HUD) only ever needs the key, and an enum would make every new store a change to
`quiver-api`. Each provider parses its ids into its own newtypes (`EpicApp`, `SteamAppId`,
`GogProductId`, `EaOfferId`, `MetaAppId`) at its boundary.

| Store | `id` is | Example | Notes |
|---|---|---|---|
| Ubisoft | product id | `3539` | unchanged |
| Steam | app id | `292030` | what every Steam file and URL uses |
| Epic | app name | `Camellia` | Epic's stable asset id; namespace and catalog item are looked up from it |
| GOG | product id | `1423049311` | numeric, but kept as a string: ids may exceed u32 (spike G2) |
| EA | base-game offer id | `Origin.OFR.50.0002694` | what library, builds and licence requests start from. `contentId` and `masterTitleId` are metadata; whether editions fold on `masterTitleId` is settled by EA S2 |
| Meta | numeric app id | the manifest's `appId` | the Platform SDK's identity; the canonical name (`ready-at-dawn-lone-echo`) is metadata and the OAF launch argument |

### 2.2 Forms

| Form | Ubisoft (unchanged) | Other stores | Used by |
|---|---|---|---|
| Display, CLI, JSON wire | `ubisoft:3539` (input also accepts `3539`, `ubi:3539`) | `epic:Camellia`, `steam:292030` | `quiver play`, `--json`, events |
| Key | `3539` | `epic:Camellia` | export keys, settings scopes (`hud.game.<key>`), mod manifests |
| Data folder | `games/3539/`, `mods/3539/` | `games/epic-Camellia/`, `mods/epic-Camellia/` | history, playtime, saves, profiles, layers, options, perf |
| Export key | `quiver:3539[/<profile>]` | `quiver:epic:Camellia[/<profile>]` | `.desktop` `X-Quiver-Id`; Steam shortcut appid = `crc32(key) \| 0x80000000` |
| `.desktop` stem | `quiver-3539[-<profile>]` | `quiver-epic-Camellia[-<profile>]` | file names (found through `exports.json`, never parsed) |
| Environment | `QUIVER_GAME_ID=ubisoft:3539` | `QUIVER_GAME_ID=epic:Camellia` | PROPOSAL's launch markers (not built yet, so nothing to keep) |
| URL (if ever) | `quiver://play/ubisoft:3539?profile=p7f3k2q` | same shape | an `x-scheme-handler/quiver`; the id charset needs no escaping |

The export key keeps `/` for the profile only: `ExportKey::parse` splits on the first `/` today
(`crates/ubi-core/src/exports/mod.rs`), so the store is separated by `:`. That is why GOG's and EA's
`quiver:gog/<id>` and `quiver:ea/<offer>` were changed (section 10). Data folders use `-`: Ubisoft
folders are all digits and no store name contains `-`, so they can't collide.

### 2.3 Compatibility rules

1. **Existing Ubisoft keys never change:** data folders, export keys and so Steam shortcut appids
   (the existing test pins `crc32("quiver:5059")`), `.desktop` files, settings scopes, mod
   manifests' `products = [420]` and save-store folder names. Nothing is migrated.
2. **Requests** take `game: GameRef` with `#[serde(alias = "product_id")]`. `GameRef` deserializes
   from a JSON number (Ubisoft) or any string form, so old scripts and an older app keep working.
3. **Responses and events** always carry `game`. Ubisoft records also carry the old `product_id`
   (deprecated) until a protocol version bump, so `--json` consumers keep working.
4. **Persisted files** (`games.json`, `queue.json`, `exports.json`, `steam-games.json`, activity,
   play sessions): Ubisoft records keep writing `product_id` exactly as today, other stores write
   `game`, and readers accept either. Ubisoft-only data stays readable by an older `quiverd`.
5. **Mod manifests** keep `products = [..]` for Ubisoft and add `games = ["steam:292030", …]`;
   MOD-CONFIG `[match]` blocks get the same `games` key.
6. **Prefix folders** are already slug-based. A new store's game whose slug is taken gets a
   `-<store>` suffix; existing prefixes don't move.

### 2.4 One title, several stores

A library entry is a **title** with one or more **store records**, generalising today's
`LibraryEntry.steam: Option<SteamGame>` (which already makes a Ubisoft game bought on Steam one
entry):

```rust
pub struct LibraryEntry {
    pub game: GameRef,                 // the home record: what installs, launches and keys data
    pub records: Vec<StoreRecord>,     // every store record folded into this entry
    /* name, art, installed, running, update_available, … as today */
}
pub struct StoreRecord { pub game: GameRef, pub owned_by: Vec<AccountRef>, pub relation: Relation, pub route: Option<Route> }
pub enum Relation { Home, CopyOf, RedeemedFrom, Linked }
```

**Records merge only on an explicit link, never on a fuzzy name:**

| Link | Source | Home record |
|---|---|---|
| Ubisoft game sold on Steam | the existing Steam-copies classifier (STEAM-COPIES) | `ubisoft:<id>`, route "Steam, `steam-run`, bridge" |
| Ubisoft game sold on Epic | catalog `ThirdPartyManagedProvider = UbisoftConnect`, `GameID` = Ubisoft id | `ubisoft:<id>`, "Bought on Epic · activated on <account>" |
| EA game sold on Steam or Epic | `link2ea://`, `ThirdPartyManagedApp = Origin` | `ea:<offer>` (EA phase 3) |
| Same game in two unrelated stores (The Witcher 3 on GOG and Steam) | only the person, through "Show as one" | display grouping only; installs, mods, saves and settings stay per record |

`quiver play witcher` searches every store. One matching entry plays. Matches in unlinked stores
are listed, and the CLI asks for `--store` or a full ref; `--json` returns the candidates rather
than guessing. This narrows STORE-EPIC section 4.1, which merged any cross-store name match.

---

## 3. Store providers

### 3.1 Shape

Providers live in their own crates and run inside `quiverd`, which routes each request by
`game.store`. A provider declares what it implements, so the app marks what isn't wired instead of
inventing it (PROPOSAL principle 4).

```rust
pub trait StoreProvider: Send + Sync {
    fn id(&self) -> StoreId;
    fn capabilities(&self) -> Capabilities;            // which traits below exist, and how (e.g. saves: Delegated)
    fn accounts(&self) -> &dyn Accounts;
    fn library(&self) -> &dyn Library;
    fn installer(&self) -> Option<&dyn Installer>;
    fn launch(&self) -> &dyn Launch;
    fn cloud_saves(&self) -> Option<&dyn CloudSaves>;
    fn progress(&self) -> Option<&dyn Progress>;
    fn playtime(&self) -> Option<&dyn Playtime>;
    fn entitlements(&self) -> Option<&dyn Entitlements>;
}
```

Requests every store understands take a `GameRef`. Verbs that only make sense for one store go in a
nested enum, as `Request::Mods(ModsRequest)` already does: `Request::Epic(EpicRequest::Activate
{ .. })`, `Request::Gog(..)`. The existing flat `Steam*` requests stay until a cleanup.

### 3.2 Accounts and secrets

```rust
pub trait Accounts {
    fn begin_sign_in(&self, hint: SignInHint) -> SignInStep;
    fn continue_sign_in(&self, flow: FlowId, input: SignInInput) -> SignInStep;
    fn refresh(&self, account: &AccountRef) -> Result<()>;     // single-flight per account
    fn sign_out(&self, account: &AccountRef) -> Result<()>;
    fn list(&self) -> Vec<Account>;                            // with AccountState
}
pub enum SignInStep {
    Password, TwoFactor,                                       // Ubisoft today; Steam fallback
    OpenUrl { url: String, then: Then },                       // Then: PasteRedirect | PasteCode | WaitCallback
    ShowQr { url: String, poll: Duration },                    // Steam (Mobile app approval)
    ClientManaged { what: String },                            // Steam local mode; Meta's client in its prefix
    Done(Account),
}
pub enum AccountState { SignedIn, NeedsSignIn, SignedInElsewhere, ClientManaged }
```

The app and CLI render the steps generically: `OpenUrl` opens the browser and shows a paste field,
`ShowQr` draws the code, `WaitCallback` waits for a scheme handler (Meta's `oculus://`). The old
`Login` and `Login2fa` requests stay as Ubisoft shorthands.

**Rules for every store:**

- **Several accounts per store, one library.** Each account contributes ownership; there is a
  default, and the per-game `account` setting picks the launch account, which also decides whose
  cloud saves are used. Store limits appear as a refusal with a reason, not a different model:
  Steam has one active client user, a GOG comet sidecar serves one account at a time, Meta has one
  user per runtime prefix.
- **Never reconnect in a loop.** `invalid_grant`, an expired refresh token or "logged in
  elsewhere" marks the account `NeedsSignIn` or `SignedInElsewhere`, pauses its downloads and shows
  **Sign in again** (the Ubisoft decision, for every store).
- **Single-flight refresh.** Where a store rotates refresh tokens (Epic does; GOG unverified), the
  new token is written to the keyring *before* the new access token is used.
- **Vendor terms** (an Epic EULA prompt, for example) become a notification that opens the
  vendor's page. Quiver never accepts anything on the user's behalf.

| Secret | Where |
|---|---|
| Refresh tokens, session blobs | Secret Service keyring, service `quiver`, entries `<store>:<account id>` (`epic:…`, `gog:…`, `ea:…`, `steam:<steamid64>`, `meta:…`). Ubisoft keeps `session:<userId>` |
| Account index (id, name, default, linked accounts) | `<data>/accounts.json`; each entry gains `store`, absent = Ubisoft |
| Launch material a game must read (Epic `.ovt`) | a 0600 file under `<prefix>/quiver-run/`, deleted at session end |
| Handoffs to sidecars (comet's tokens, until it reads them from an fd) | a 0600 file in `$XDG_RUNTIME_DIR/quiver/<svc>/` (0700 directory), deleted once read |
| Any Quiver-written copy of a vendor's tickets | 0600. Today's `localconfig.vdf.quiver-backup` gets the default umask and should be fixed regardless of stores (STORE-STEAM section 4) |
| Never | plaintext token files, tokens on a sidecar's argv, tokens in logs, activity, job details, play-session records or `--json` |

A `Secret<T>` type in `quiver-core` redacts itself in `Debug` and `Display`. `LaunchPlan` arguments
and environment values can be `Secret`, and everything that records a launch prints `<redacted>`.
Where a vendor's protocol *requires* a secret on the game's command line or environment (Epic's
single-use exchange code, EA's launch tokens), it is minted per launch and never stored. The same
redaction covers Meta's access token in CDN query strings.

### 3.3 Library, metadata and art

```rust
pub trait Library {
    fn owned(&self, account: &AccountRef) -> Result<Vec<Entitlement>>;
    fn metadata(&self, game: &GameRef) -> Result<GameMeta>;
    fn art(&self, game: &GameRef) -> ArtCandidates;            // tile, hero, logo, icon, in preference order
    fn changes(&self) -> Option<ChangeStream>;                 // Steam: inotify on steamapps/ and appcache/
}
pub struct Entitlement { pub game: GameRef, pub kind: Kind /* Game | Dlc | Addon */, pub parent: Option<GameRef>,
                         pub source: Source /* Purchase | Subscription{expires} | Free | Partner(StoreId) */,
                         pub links: Vec<GameRef> /* vendor-declared links (section 2.4) */ }
```

`GameMeta` carries title, developer, publisher, release date, description, languages, store URL and
supported OS, each field tagged with its source so the game page can show it. Art keeps today's
candidate chain (store art, then custom). Tile ratios differ (Ubisoft 240:280; Epic's
`DieselGameBoxTall` and GOG's covers are taller; Meta's are landscape or square), so the grid crops
and never stretches. API caches live in `<data>/<store>/cache/` with timestamps, and a failing
endpoint becomes an `errors` entry, as `progress.rs` does today, never an empty library.

### 3.4 Content: `Installer` and `ContentSource` on the existing queue

Two layers reconcile GOG's `ContentSource`, Epic's `Installer`, EA's `StoreInstaller`, Meta's
`FileManifest` and Steam's delegating installer:

```rust
/// What the queue drives. One per store that installs anything.
pub trait Installer {
    fn preview(&self, game: &GameRef, opts: &InstallOpts) -> Result<InstallPreview>;  // sizes, free space, languages, DLC
    fn run(&self, job: &mut Job, op: Op) -> Result<()>;      // Op: Install | Update | Verify{repair} | Uninstall | Adopt(path)
    fn on_moved(&self, game: &GameRef, from: &Path, to: &Path) -> Result<()>;  // fix up records the store keeps
}

/// For stores whose bytes Quiver fetches. quiver-core's PieceInstaller turns any ContentSource into an Installer.
pub trait ContentSource {
    fn builds(&self, game: &GameRef) -> Result<Vec<Build>>;
    fn plan(&self, game: &GameRef, build: &Build, opts: &InstallOpts) -> Result<InstallPlan>;
    fn fetch(&self, piece: &PieceRef) -> Result<ByteStream>;  // refreshes signed URLs internally
    fn decode(&self, piece: &PieceRef, raw: Bytes) -> Result<Bytes>;  // zlib | raw deflate | identity
}
pub struct InstallPlan { pub files: Vec<FileEntry>, pub setup: Vec<SetupStep>, pub manifest: Bytes }
pub struct FileEntry { pub path: RelPath, pub size: u64, pub hash: Hash, pub parts: Vec<Part> }
pub struct Part { pub piece: PieceRef, pub offset_in_piece: u64, pub len: u64 }   // pieces may be shared by files
```

One shape covers every store: a file is a sequence of byte ranges of pieces, and a piece is fetched
once, checked and decoded. Ubisoft's pieces are slices; GOG v2's are chunks with compressed and
uncompressed MD5 (v1: byte ranges of `main.bin`, whole-file MD5); Epic's are 1 MiB-window chunks
shared across files, with SHA-1 per file; EA's are zip entries fetched by HTTP range, with CRC32
from the central directory; Meta's are segments with SHA-256 per segment and per file.

**PieceInstaller** (in `quiver-core`) holds what `ubid/downloads.rs` and `ubi-core/install.rs` do
today: resume records in `.quiver-install/`, parallel fetch with retries and backoff, the bandwidth
limit, the free-space check with a margin, pause, resume and reorder, piece reuse on update (diff
the plans, fetch only what's missing), and the rule that **only manifest files are read or
written**.

**`SetupStep`** covers what a store needs in the prefix once per install: `Redist` (GOG
dependencies, Epic prerequisites), `RunOnce` (GOG `scriptinterpreter.exe`, EA `Touchup.exe`),
`Registry` (EA's `WOW6432Node\Origin` keys, GOG's keys), `Service` (GOG's GalaxyCommunication
dummy), `CopyDll` (Ubisoft's SDK DLLs, as today) and `XrPreflight` (Meta's Wine VR registry). Each
writes a marker and logs to the job, and the game page offers **Re-run setup**.

**Steam implements `Installer` by delegation:** `steam://install|validate|uninstall`, with progress
mirrored from `appmanifest_*.acf` through inotify and no `ContentSource`. A native depot installer
is a later opt-in (STORE-STEAM steer 1). **Ubisoft keeps its installer**, wrapped as an
`Installer`, until PieceInstaller reaches parity on GOG; only then, and only with byte-identical
test installs, do Ubisoft's manifests become a `ContentSource`.

### 3.5 Verify, repair, update, move, uninstall, import

| Operation | Shared (`quiver-core`) | Store-specific |
|---|---|---|
| Verify, repair | hash files against the plan, re-fetch only bad pieces, never touch non-manifest files | hash kind; Steam delegates to `steam://validate` |
| Update | plan diff and piece reuse, update preview, auto-update policy | build listing; Epic delta manifests; GOG xdelta later; Steam's `AutoUpdateBehavior` |
| Move | **built once for every store** (not built yet anywhere): rename or reflink, rewrite records, re-point overlay mounts | `on_moved`: GOG and EA registry keys; Steam: its storage manager; Meta: the OAF library path |
| Uninstall | files, prefix optional; offer to clean profiles and exports | EA also deletes `.dlf` licences; Steam delegates; Meta removes the manifest |
| Import | **verify against the manifest before adopting**, reflink where possible, first-run scan | finders: Heroic/legendary `installed.json`, EGL `.item`, `goggame-*.info`, `installerdata.xml`, the Heroic fork's `MetaGames`, Lutris and Connect (as today) |

Imports take installs only, never another launcher's sessions (section 7).

### 3.6 Launch plan and runtime services

```rust
pub trait Launch {
    fn prepare(&self, game: &GameRef, account: &AccountRef, ctx: &LaunchCtx) -> Result<LaunchPlan>;
    fn on_exit(&self, plan: &LaunchPlan, session: &SessionRecord) -> Result<()>;
}
pub struct LaunchPlan {
    pub route: Route,                       // QuiverProton | SteamClient{app, hooked} | VendorClient(kind) (later)
    pub exe: RelPath, pub args: Vec<Arg>, pub workdir: RelPath,
    pub env: Vec<(String, Value)>,          // Value may be Secret
    pub files: Vec<TempFile>,               // 0600, removed at session end
    pub setup: Vec<SetupStep>,              // ensured before launch
    pub services: Vec<Box<dyn RuntimeService>>,
    pub prefix: PrefixPolicy,               // Own | SteamCompat(app) | SharedRuntime{account}
    pub xr: Option<XrRequirement>,          // runtime json, compat runtime, preflight
    pub anti_cheat: AntiCheat,
}
pub trait RuntimeService {
    fn ensure(&self, session: &Session) -> Result<Ready, NotReady>;  // NotReady { reason, fix }
    fn attach(&self, session: &Session) -> Result<Lease>;           // the play-session lease
    fn events(&self) -> Option<EventStream>;                        // unlocks, overlay requests, licence errors
}
```

The launcher (`launcher.rs` today) merges the store's plan with the person's settings (env, DLL
overrides, args, wrappers), the profile's namespace and overlays, the save store's mounts, the
Sight env and the markers. Plans compose: a Ubisoft title bought on Epic gets the Ubisoft plan plus
Epic's exchange-code arguments as a co-licence, only if Epic spike S2 shows a game asks for them.

| Store | Runtime service | Where | Lifetime |
|---|---|---|---|
| Ubisoft | `ubi-bridge.exe` owns the SDK pipes; Denuvo broker on `127.0.0.1:47810` | prefix + host (built) | per launch, with a lease |
| GOG | **comet**, the Galaxy SDK service on `127.0.0.1:9977`, plus the GalaxyCommunication dummy | host sidecar now, in-process later | first GOG game to last; one account at a time until in-process routing |
| EA | the **LSX service** (XML over TCP, challenge + AES): `RequestLicense` (Denuvo), `GetAuthCode`, `QueryEntitlements`, config, profile | inside `quiverd` | per launch, own port via `EALsxPort`, 3216 as fallback (unverified, EA S4) |
| Epic | none: EOS talks to Epic itself, authenticated by an **exchange code** minted per launch (`-AUTH_PASSWORD=<code> -AUTH_TYPE=exchangecode …`), plus `-epicovt` when `OwnershipToken=true` | args + temp file | single use |
| Steam | **Steam's own client**; precondition "running and signed in as an owner"; `steam://rungameid`, with the opt-in `steam-run` hook | Steam | Steam's |
| Meta | **Meta's platform stack** (OVRServer, OAF, platform runtime); launch through OAF `/library/launch`; ready at `isIpcReady` | shared runtime prefix | per launch; Meta updates pinned |

`NotReady` is how a store explains why Play can't proceed, with a fix: "Steam isn't running",
"Signed in to Steam as another user (switch?)", "GOG games of another account are running", "Meta
runtime not set up". Containers and `steam-run` need launch material without a launch, so today's
`LaunchContext` becomes `LaunchMaterial { game, account, … }`: the bridge context for Ubisoft, the
arguments for Epic, the LSX port for EA.

### 3.7 Cloud saves and save stores

```rust
pub trait CloudSaves {
    fn mode(&self) -> SyncMode;                                // Quiver | Delegated | InGame
    fn locations(&self, game: &GameRef, account: &AccountRef, prefix: &Path) -> Result<Vec<SaveLocation>>;
    fn list(&self, loc: &SaveLocation) -> Result<Vec<RemoteEntry>>;   // path, size, hash, mtime
    fn get(&self, loc: &SaveLocation, path: &RelPath) -> Result<Bytes>;
    fn put(&self, loc: &SaveLocation, path: &RelPath, data: Bytes, mtime: DateTime<Utc>) -> Result<()>;
    fn delete(&self, loc: &SaveLocation, path: &RelPath) -> Result<()>;
    fn lock(&self, loc: &SaveLocation, how: LockKind) -> Result<Option<LockGuard>>;   // EA; others: None
}
pub struct SaveLocation { pub name: String, pub path_in_prefix: PathBuf, pub kind: LocationKind /* Slots | Tree */ }
```

- **Modes.** `Quiver`: Quiver syncs (Ubisoft `cloudsave_service`, GOG `cloudstorage`, Epic EGS
  `savesync`, EA `cloudsync`). `Delegated`: the store's client syncs (Steam; Meta's runtime if Rift
  Cloud v1 works under Wine, unverified); Quiver shows what it can read (Steam's
  `remotecache.vdf`) and still takes local backups. `InGame`: the game syncs (EOS Player Data
  Storage), and the Saves tab says so.
- **One sync engine** in `quiver-core` for every `Quiver`-mode store: back up before every pull,
  pull before launch, push after exit, newer file wins per file, ask on a real two-sided conflict
  (agents need `confirmed`), deletions not synced. GOG steer 8 and Epic risk 7 agree.
- **Save stores move from slot folders to folder trees.** `SaveStore.folder` (one folder under
  `savegames/<user>/`) becomes `folders: BTreeMap<location, folder>`; an old revision's `folder`
  reads as `{ "savegames": folder }`, so `games/<id>/saves.jsonl` parses unchanged. Ubisoft keeps
  its sibling-folder layout, because the bridge points the game at the folder. For `Tree`
  locations a non-default store lives in `<data>/games/<dir>/saves/<store id>/<location>/` and is
  bind-mounted over the real path inside the launch namespace (MODS-V2's `profile_config`
  mechanism; for Steam games, `steam-run`'s namespace). At most one store per game syncs with the
  cloud; all five docs agree.
- The slot view (`saves.rs`) applies to `Slots` locations; `Tree` locations get a file list with
  per-file cloud state.

### 3.8 Achievements, progress and Sight toasts

```rust
pub trait Progress {
    fn definitions(&self, game: &GameRef) -> Result<Vec<AchievementDef>>;
    fn state(&self, game: &GameRef, account: &AccountRef) -> Result<Option<Unlocks>>;   // None: unknown
    fn stats(&self, game: &GameRef, account: &AccountRef) -> Result<Stats>;
    fn unlock_path(&self) -> UnlockPath;      // Relay(service) | GameToStore | LocalOnly | Unknown
}
// quiverd event, consumed by Sight's toast lane, the app and the activity log:
pub struct AchievementUnlocked { pub game: GameRef, pub account: AccountRef, pub id: String,
                                 pub at: DateTime<Utc>, pub via: UnlockSource }
```

**The rule for every store: Quiver never unlocks or reports an achievement on its own
initiative.** It only relays what the game asked for, acting as the service the store's own SDK
would talk to. Where no relay to the store is known, unlocks stay in a local earned set and are
never sent.

| Store | Who reports the unlock to the store | Where Sight's toast comes from |
|---|---|---|
| Ubisoft | bridge receives `EarnAchievementReq` and logs it; reported to UbiServices once that API is captured | bridge → broker → quiverd; local earned set (SIGHT section 7) |
| GOG | comet relays the SDK's unlock to `gameplay.gog.com` | comet's `achievement_unlocked` custom event to a quiverd endpoint |
| Epic | the game, through EOS | polling store GraphQL during and at the end of the session |
| Steam | the game, through the client | inotify on `appcache/stats/UserGameStats_<account>_<app>.bin`; toast only when the Steam overlay is off for the game (STORE-STEAM steer 9) |
| EA | unknown: the game sends LSX `GrantAchievement`; how the EA app forwards it is not known (EA S6) | the LSX handler, into a local earned set |
| Meta | Meta's platform runtime | data source unverified (Meta S5); toast to hypxrhud in VR |

Toasts are de-duplicated against the known-earned set (Origins re-earns its whole set at every save
load). The game page shows whatever unlock state each store has, and `state` returns `None` when
it's unknown (GOG steer 10).

### 3.9 Play time

```rust
pub trait Playtime {
    fn remote_total(&self, game: &GameRef, account: &AccountRef) -> Result<Option<Duration>>;
    fn style(&self) -> ReportStyle;   // Heartbeat{every} | AfterSession | Periodic{every} | StoreRecords | None
    fn report(&self, session: &Session) -> Result<Option<Lease>>;
}
```

Ubisoft reports with `Heartbeat { 330 s }` (built). GOG uses `AfterSession` (a POST in minutes,
queued offline). Epic is `Periodic` (PUT segments to `library-service`, unverified, Epic S6). Steam
is `StoreRecords`: Steam records it and Quiver reads `localconfig.vdf`. EA is read-only until its
reporting call is known (`gameSessionEnd`, unverified). Meta is `None` (no API found). Local
`playtime.jsonl` with the settings revision is kept for every store through the session lease. The
existing `report_playtime` switch covers all stores, with a per-store override. Presence ("playing
X" to friends) is off by default everywhere.

### 3.10 DLC, entitlements and redemptions

```rust
pub trait Entitlements {
    fn addons(&self, game: &GameRef, account: &AccountRef) -> Result<Vec<Addon>>;   // owned?, installable?, store_url
    fn delivery(&self) -> DlcDelivery;      // SdkServed | FilesInstalled | GameChecksStore (a set)
    fn redeemable(&self, account: &AccountRef) -> Result<Vec<Redeemable>>;          // Epic → Ubisoft keys; EA Play claims
    fn redeem(&self, items: &[RedeemId], confirmed: Confirmation) -> Result<Vec<Redeemed>>;
}
```

A game learns its DLC in one of three ways. **SdkServed:** it asks a service Quiver runs, and the
answer is the store's list (Ubisoft bridge `InitRsp` owned ids; EA LSX `QueryEntitlements` from
EA's list; GOG comet `LIBRARY_INFO`). **FilesInstalled:** Quiver installs the content (GOG depots,
Epic DLC apps, EA packs later, Meta asset files). **GameChecksStore:** the game asks the store
itself (Epic through EOS, Steam through the client).

**Redemptions are irreversible account actions:** Epic's redemption of Ubisoft codes onto the
linked Ubisoft account, and EA Play claims through `grantEntitlement` (validated by EA's server).
`redeem` takes a `Confirmation` that only a person or an approved agent request can produce. Quiver
never links accounts; it opens the vendor's linking page.

### 3.11 Anti-cheat, across stores

`AntiCheat` is `None`, `EacProton` or `BattlEyeProton` (the developer opted in; Quiver provides the
runtime, Steam's when present, else downloaded as Heroic does), or `Unsupported { vendor, reason }`
(EA Javelin, EAC v2 with attestation, kernel anti-cheat). It comes from a curated data list keyed by
`GameRef`, like built-in mods, plus file detection at install. For any anti-cheat title the Sight
Vulkan layer is off and the HUD uses the desktop presenter (SIGHT-HUD decision 16), and mods are
refused for online titles. `Unsupported` games show "Not playable on Linux: <reason>" with Play
disabled; `quiver play --force` still exists (EA steer 7).

---

## 4. What stays store-agnostic

These seams are being built now (Options, mod SDK, perf and HUD). Each item says what must be true
for them to keep working when other stores arrive.

- **Per-game prefixes.** `PrefixPolicy::Own` for Ubisoft, Epic, GOG and EA (`ensure_prefix` plus
  the plan's `SetupStep`s). `SteamCompat` for Steam (`compatdata/<appid>/pfx`; Proton through a
  staged `CompatToolMapping` edit, only with Steam closed). `SharedRuntime` for Meta, whose stack
  must share a wineserver with the game: one runtime prefix per account, later cloned per game from
  a template (Meta steer 3). Proton choice, settings history and the designed prefix snapshots
  apply to all three.
- **Settings and history** stay in `games/<dir>/history.jsonl` with unchanged keys. `account` names
  an account of the game's store; `art` generalises from `steam|ubisoft` to any store the entry
  carries.
- **Profiles and mods.** The overlay mounts over the home record's install folder, whoever
  installed it. The built-in catalog moves to `mods/<dir>/…` (`mods/420/` unchanged). `builds =
  [hash]` uses each store's manifest hash. Mod import refuses the unlockers listed in section 7.
  Steam games get mods only through the `steam-run` namespace (STEAM-COPIES section 4).
- **Options (MOD-CONFIG).** `Options*` requests take `game` (aliasing `product_id`); files live
  under `mods/<dir>/options/` and `games/<dir>/options/`. **`build/options` should keep
  `product_id` while it is in flight;** the R1 sweep (section 8.1) converts it with everything
  else. If R0 lands first, new code should use `game_dir(&GameRef)` rather than
  `join(product_id.to_string())`.
- **Sight.** `ActiveSession` gains `game`, `store` and `route`, and the session card shows "Epic ·
  via Quiver" or "Steam · via Steam". Toasts come from `AchievementUnlocked` and other provider
  events. The game's overlay requests (Ubisoft `OverlayShowReq`, EA LSX IGO, comet's overlay
  service) map onto one `OverlayRequest { kind }`, answered with a Sight card or `Unavailable` so no
  game waits. Quiver never injects a vendor's proprietary overlay; the EOS overlay is a per-game
  opt-in, and Steam's coexists on its own hotkey.
- **HUD.** Scopes become `hud.game.<key>` and `hud.profile.<key>/<profile>` (Ubisoft keys bare);
  perf recordings live in `games/<dir>/perf/`. The `auto` presenter gains a third arm: **game**
  (Vulkan layer), **desktop** (anti-cheat titles) and the new **XR** presenter (hypxrhud) for VR
  sessions, chosen from `LaunchPlan.xr`. The XR metrics producer is new work (Meta section 4.5)
  that Steam VR titles would share.
- **Exports** use the key forms in section 2.2. Steam games need no shortcut of their own, but a
  *pinned profile* of one needs a one-shot "next launch of app X uses profile P" in quiverd,
  because `rungameid` can't carry an env var (STORE-STEAM section 7).
- **CLI and agents.** Every verb takes a game reference (`quiver install gog:1423049311`, `quiver
  play origins`), with `--store` to narrow a fuzzy name. Store-only verbs are nouns (`quiver epic
  activate`, `quiver steam patch`); sign-in is `quiver login --store gog`. The command registry,
  `--json`, source labels, activity and approvals are unchanged.
- **First run** gets one **Stores** step: each provider with sign in, skip or import, and a note on
  anti-cheat titles. Storage, Fixes, Controller and Theme stay shared.

---

## 5. Crate layout and rename path

| Crate | From | Holds |
|---|---|---|
| `quiver-api` | `ubi-api` | protocol, `GameRef`, `StoreId`, the model |
| `quiver-core` | split out of `ubi-core` | provider traits; paths (`game_dir`); `Secret` and the keyring wrapper; accounts index; launcher (prefix, Proton, namespace, markers); PieceInstaller; save-sync engine and save stores; mods, `modversions`, `modpkg`, overlay, `confpatch`; settings history; exports (`.desktop`, Steam shortcuts); session leases and local play time; anti-cheat data |
| `steam-local` | `ubi-core::exports::{steam, vdf, textvdf, steamart, steamgames, steamlaunch}` | Steam's files (discovery, libraries, manifests, `appinfo.vdf`, stats caches, staged writes) and the `steam://` driver; no network. Used by `quiver-core` (shortcuts), `steam-core` and `ubi-core` (Steam copies) |
| `quiverd` | `ubid` (binary already `quiverd`) | daemon: store registry and routing, queue, events, broker host, LSX listener |
| `quiver-cli` | `ubi-cli` (binary `quiver`; `ubi` stays an alias) | CLI |
| `quiver-app` | `ubi-app` (binary already `quiver-app`) | GPUI app |
| `ubi-core`, `ubi-proto`, `ubi-ipc`, `ubi-bridge` | the Ubisoft remainder | the Ubisoft provider and bridge; names stay, now accurate |
| `steam-core` | new | Steam provider over `steam-local`; generalised `steam-run`; feature `cm` adds `steam-vent` sign-in and later the depot installer |
| `epic-core` | new | Epic provider (STORE-EPIC section 4.3) |
| `gog-core` | new | GOG provider; a `galaxy` module supervises comet (STORE-GOG's `gog-galaxy` crate becomes this module) |
| `gog-dummy-service` | new, `x86_64-pc-windows-gnu` | GalaxyCommunication stub |
| `ea-core` | new | EA provider; an `lsx` module that quiverd serves |
| `meta-core` | new | Meta provider (library, downloader, title classifier) |
| `meta-runtime` | new, Windows side, later | Quiver's OAF client, compat-runtime placement |

Dependencies point one way: `quiver-api` ← `quiver-core` ← providers ← `quiverd`. Providers never
depend on each other; cross-store facts ("redeem onto Ubisoft", "this Steam app is Ubisoft product
N") travel as `GameRef` links through `quiverd`.

**Rename path.** (1) R0–R2 (section 8.1) need no renames. (2) Rename directories and package names
but keep import names with workspace aliases (`ubi-api = { path = "crates/quiver-api", package =
"quiver-api" }`), so everything compiles unchanged; update `tools/install-user.sh` and packaging in
the same commit. (3) Sweep imports (`ubi_api::` → `quiver_api::`) one crate per commit, then drop
the aliases. (4) Extract `quiver-core` and `steam-local` by moving modules, leaving re-exports in
`ubi-core` for one cycle. Steps 2–4 touch every file the open branches touch (`build/options`,
`build/mod-sdk`, `build/ubi-perf`, `build/gpui-bump`), so they wait for a quiet window after those
merge (steer X16).

---

## 6. Capability matrix

Each cell is approach · effort · risk. Ubisoft is today's state; the other columns come from the
store docs, which have the detail and prior art.

| Capability | Ubisoft (today) | Steam | Epic | GOG | EA | Meta |
|---|---|---|---|---|---|---|
| Sign-in | password + 2FA (built) | none, follows the client; QR later · M · Med | browser + pasted code, EGL client · M · Med | browser + pasted redirect, Galaxy client · M · Med | browser PKCE + pasted redirect, `pc_sign` · M · Med | Meta's client in runtime prefix, `oculus://` · M · Med |
| Several accounts | built | local users, one active · S · Low | yes · S · Low | yes; one plays at a time · S · Low | yes (phase 2); single-session rule unknown · S · Med | one per runtime prefix · M · Med |
| Library, ownership | built | local manifests; full list after sign-in · S/M · Low/Med | library-service + catalog · S–M · Low | galaxy-library + `user/data/games` · M · Med (drift) | GraphQL incl. EA Play · M · Low–Med | OAF cache, GraphQL later · S–M · Low/Med |
| Metadata, art | built | `appinfo.vdf` v29 + local art · M · Med | `keyImages`, attributes · S · Low | gamesdb (beats Ubisoft) · S · Low | GraphQL images · S · Low | Application covers · S · Low |
| Install | built (slices) | `steam://install`, manifest progress · S · Low (native: L · Med) | own chunk downloader · L · Med | content system v2 (v1 later) · L · Med | DiP zip by HTTP range + Touchup · L · Med | manifest zip + segments · M · Low |
| Update | built | Steam's; shows pending · S · Low | hash diff, delta manifests · M · Med | chunk diff · M · Low | build check, CRC diff (unverified) · M · Med | `versionCode` diff · S–M · Low |
| Verify, repair | built | `steam://validate` · S · Low | SHA-1 per file · M · Low | MD5 per chunk · S–M · Low | CRC32 · M · Low | SHA-256 file + segment · S · Low |
| Uninstall, import | built | `steam://uninstall`; no import · S · Low | Heroic/legendary/EGL, verified · M · Low | `goggame-*.info`, innoextract · M · Low–Med | `installerdata.xml` · M · Low | fork's `MetaGames` · S · Low |
| Move | not built | Steam's storage manager · S · Med | shared · S · Low | shared + registry fix-up · S · Low | shared + registry fix-up · S · Low | inside runtime library · M · Med |
| Prefix, Proton | per game (built) | `compatdata` + staged mapping · M · Med | per game + prereqs · S · Low | per game + redists, script, dummy · M · Med | per game + registry · S · Low | shared runtime prefix · M · Med |
| Licensed launch, runtime | bridge + broker (built) | real client, `rungameid`, `steam-run` · S–M · Low | exchange code + `.ovt` · S–M · Low | DRM-free; comet for SDK · S + M · Low/Med | LSX in quiverd, OOA, Denuvo relay · L · **High** | Meta stack + OAF, patches pending review · L · **High** |
| VR rendering | n/a | XR presenter only | n/a | n/a | n/a | `wineopenxr`; Revive-derived LibOVR runtime · L · **High** |
| Cloud saves | built (slots) | delegated; `remotecache.vdf` + backups · S–M · Low | EGS savesync; EOS in-game · M · Med | `cloudstorage` trees · L · Med | `cloudsync` with locks · M · Med | Meta runtime (unverified) + backups · M · Med |
| Save stores per profile | built (slot folders) | `steam-run` namespace · M · Med | tree locations · M · Med | tree locations · M · Med | tree locations · M · Med | known paths only · M · Med |
| Achievements: read | definitions only | stats cache; rarity public · S–M · Low | GraphQL · S–M · Med | unlock state from `gameplay` · S · Low–Med | GraphQL · S · Low | source unknown · M · Med |
| Unlocks, toasts | bridge logs; toast designed | game via client; inotify · S · Low | game via EOS; polled · S · Low | comet relays; event · S · Low–Med | local set; forwarding unknown · M · Med | Meta runtime; hypxrhud · S · Med |
| Play time | heartbeat (built) | Steam records; read · S · Low | PUT segments (unverified) · S · Med | POST after session · S · Low | read; report unknown · S/M · Med | local only · S · Low |
| DLC, entitlements | owned ids via bridge | `listofdlc`; owned after sign-in · M · Low | EOS checks; DLC apps · S · Low | DLC depots · M · Low | LSX from EA's list; packs later · M (L) · Med | IAP list; asset files · M · Med |
| Redemptions, subscriptions | n/a | n/a | Ubisoft key redemption · S–M · Med | n/a | EA Play claim · S–M · Med | n/a |
| Mods, Options | built / designed | `steam-run` namespace · M · Med | shared · S · Low | shared · S · Low | shared; Frosty unverified · S · Low | shared; OVRPlugin fix mod · S · Low |
| Sight, HUD | designed | layer in pressure-vessel; coexists with overlay · S · Low | shared; EOS overlay off · S · Low | shared; answer comet overlay · S–M · Med | map LSX IGO · S–M · Low | XR presenter + producer · M · Med |
| Exports | built | pinned profiles only · S–M · Low | key form · S · Low | key form · S · Low | key form · S · Low | key form · S · Low |
| Anti-cheat | n/a | client's runtimes | EAC/BattlEye runtimes; v2 out · S · **High** | rare | Javelin "Not playable" · S | n/a |
| First run | built | grow the Steam step · S · Low | Stores step + activation · S · Low | Stores step + imports · S–M · Low | Stores step + anti-cheat note · S · Low | XR check + runtime setup · M · Med |

**Totals, roughly:** Steam M (local) then M (around its launch). Epic M (phase 1), L (installs), M
(saves and progress). GOG about L for parity. EA L+ at high risk. Meta M for Track A, L+ for Track B
behind spikes. The refactor is M across R0–R4.

---

## 7. Legitimacy policy (every store)

This consolidates each store doc's line. The store docs keep their concrete allowed/not-allowed
tables (Epic section 4.5, EA section 4.4, Meta "The line"); this section wins where they differ.

1. **The store's real backend decides ownership, for the user's own account. Quiver relays and
   never answers "owned" itself.** The Ubisoft bridge serves the ownership service's ids; EA's LSX
   answers `QueryEntitlements` only from EA's list; Epic exchange codes and ownership tokens are
   minted from the user's session for titles it owns; Steam's DRM and Steamworks go to Valve's
   client; Meta's entitlement is Meta's code (a future meta-bridge would ask Meta's backend); GOG is
   DRM-free and comet uses the user's real token.
2. **No ownership-bypassing emulators or entitlement stubs:** Steamworks emulators (Goldberg,
   `steam_api` replacements), DLC unlockers (CreamAPI, SmokeAPI, ScreamAPI, Koaloader, EA's DLC
   unlocker), EOS emulators, "entitlement always passes" stubs, a `LibOVRPlatform` replacement
   that defeats the signature check, SteamStub or CEG strippers, and placeholder auth codes
   (Quiver returns an LSX error, never `"invalid"`). The list is data; the built-in catalog and mod
   import refuse anything on it.
3. **Licence material is kept exactly as received.** EA `.dlf` files are written byte for byte,
   never decrypted or edited, and refreshed on Quiver's own clock. Epic `.ovt` files are used as
   issued and deleted at session end. Ubisoft tickets and Denuvo tokens are relayed. Meta's signed
   asset archives keep their `.sig`. Epic manifests are decrypted only with the secrets Epic
   returns for that entitlement. Steam's tickets are never read.
4. **Machine identity is true and stable.** Quiver never varies a machine hash (`machineHash`,
   `pc_sign`) or hardware fingerprint to dodge activation or Denuvo limits, and tells the user what
   is sent, as the vendor's client would.
5. **Compatibility patches only outside licensing.** Allowed: Wine workarounds (Meta's WiFi factory
   stub, TxF), API translation (LibOVR to OpenXR, like DXVK) and runtime or vendor-lock unlocks
   (OVRPlugin), shipped as audited fix mods mounted over the install, never written into it. Not
   allowed: any patch to a branch whose job is a licence, ownership, signature or trusted-caller
   check, in vendor software or a game, and anything in DRM code (`activation.dll`, Denuvo,
   SteamStub, CEG). A patch of unknown purpose doesn't ship until reviewed (Meta's
   `OAF_LAUNCH_CHECK` and `OVRServer` winefix: Meta S3).
6. **Anti-cheat is never bypassed, spoofed or disabled.** Only the vendors' Proton runtimes; say
   so when a title can't run.
7. **Honest client identification wherever the store allows it:** Quiver's own User-Agent where it
   works; Steam `platform_type = SteamClient` with an honest device name ("Quiver on <host>");
   never posing as a *mobile* app (Steam's account flags were tied to that). **Posing as the
   official client's OAuth id is unavoidable** where no third-party client exists: Ubisoft
   (Connect's AppId, existing), Epic (EGL's client), GOG (Galaxy's client, the only usable
   redirect), EA (`JUNO_PC_CLIENT` with `pc_sign`), and Meta (sign-in runs in Meta's own client;
   the CDN expects `Oculus Client`). Each case is documented in the README; request rates stay at
   or below the official client's; client configs are updatable without a code change (Epic
   steer 2).
8. **No reuse of another launcher's stored tokens.** Quiver never reads Heroic or legendary
   `user.json`, GOG `auth.json`, Steam's `config.vdf` ConnectCache, `local.vdf`, `ssfn*` or the
   tickets in `localconfig.vdf`, the EA app's or Maxima's `auth.toml`, or the Heroic fork's OAF
   database. Imports bring installs only, verified against manifests. The one narrow exception is
   a session **Quiver's own sign-in just created** in a runtime prefix Quiver owns: Meta's MVP
   reads that token from OAF once, right after the user signs in, into the keyring, until native
   sign-in exists. Handing Quiver's own tokens to a sidecar Quiver runs (comet) is not reuse.
9. **The user signs in on the vendor's own page.** Quiver sees no password except in Ubisoft's
   existing flow and Steam's optional fallback, never uses credential-leaking paths (EA's
   password-in-environment mode), and never links accounts, accepts terms, purchases, claims or
   redeems on its own.
10. **Agent and automation limits:**

    | Action | CLI (you) | Agents |
    |---|---|---|
    | Read library, status, progress | Allow | Allow |
    | Install, update, verify, play, stop (including through Steam) | Allow | Allow |
    | Settings, profiles, exports; Steam launch-option or Proton-mapping patches (staged, reversible) | Allow | Allow |
    | Restore saves or settings; resolve a save conflict | Allow | Ask |
    | Uninstall; delete prefixes, profiles or saves | Allow | Ask |
    | Restart Steam | Ask | Ask |
    | Redeem keys, claim subscription titles (lists exactly what goes onto which account) | Ask | Ask (pending approval) |
    | Sign in, sign out, add or remove accounts, link accounts | Ask | Deny |
    | Spikes that sign in, capture traffic or touch a real prefix | asked first, reverted after | never agent-initiated |

    Agents never read credential files or run captures, and are rate-limited on store APIs like
    any client.
11. **Code licences.** legendary, gogdl, Heroic, Maxima and RiftLift's wrapper (GPL) are references
    and test oracles only; no code is copied. MIT or Apache dependencies are fine after review:
    egs-api, comet (as a separately built binary or crate, with its LICENSE), origin-sdk, Revive's
    runtime.

---

## 8. Sequencing

### 8.1 The refactor first, with Ubisoft working throughout

Each step is a mergeable PR on its own, followed by the Ubisoft regression check and the usual
deploy (`tools/install-user.sh`).

| Step | What | Size | Ubisoft keeps working because |
|---|---|---|---|
| **R0** | `StoreId`, `GameRef`, `GameId` in `ubi-api` (serde from number or string, `key`, `dir_name`, `Display`); `game_dir(&GameRef)` and `mods_dir`; every `join(product_id.to_string())` routed through them. Ubisoft is the only store | S | output is byte-identical (tests pin `games/3539/`, `quiver:5059` and its crc) |
| **R1** | Requests and events take `game` (`alias = "product_id"`); responses carry both; persisted files per section 2.3; `ExportKey` holds a `GameRef`; the CLI resolver takes refs and searches every store by name. About 1,560 `product_id` uses, swept one crate per commit | M | golden tests: old request JSON parses; old `games.json`, `queue.json`, `exports.json` round-trip unchanged |
| **R2** | Provider traits, with `ubi-core` implementing them by wrapping today's code; `quiverd` routes by store; `LibraryEntry.records` (with `steam` still emitted for a cycle); `Accounts` sign-in steps; `accounts.json` `store` field; `Secret<T>` and launch redaction | M | no behaviour change: the Ubisoft path is the old code behind a trait |
| **R3** | Crate renames and the `quiver-core` / `steam-local` extraction (section 5), in a quiet window | S–M | mechanical; aliases first |
| **R4** | Generalisations, each landing with the first store that needs it: PieceInstaller (GOG); save locations and tree stores (GOG, Epic); host `RuntimeService` (comet, then LSX); `LaunchMaterial`; Move (any time); `XrRequirement` (Meta) | per store | the Ubisoft implementation stays until the new one has parity |

**Ubisoft regression check:** install and verify a small title; play Origins with the bridge and
Denuvo, with cloud pull and push; play a modded profile; launch a Steam copy through `steam-run`;
launch an existing Steam shortcut and `.desktop` entry *without re-exporting* (appids and keys
unchanged); restore a settings revision; confirm `--json` library output still has `product_id`.

### 8.2 Store order

| # | Step | Value | Effort | Risk | What it proves |
|---|---|---|---|---|---|
| 1 | **Steam, local mode** (STORE-STEAM phase 1) | high: every installed Steam game with art, play time, achievements and cloud state, no login | S–M | Low | `GameRef` end to end; merged records (replacing the Ubisoft-only Steam scan); a delegated `Installer`; `ClientManaged` accounts |
| 2 | **Epic accounts, library, activation of Ubisoft titles** (STORE-EPIC phase 1) | high here: five Ubisoft titles on Epic become activatable, two already run in Quiver | M | Med (client impersonation) | browser-paste sign-in; `<store>:<id>` keyring entries; `Redemptions` with approvals; link-based merging |
| 3 | **GOG MVP and parity** (STORE-GOG phases 1–2) | medium, depending on what you own; the cleanest full store | L | Med | `ContentSource` and PieceInstaller on a documented, DRM-free format; `SetupStep`s; the first host `RuntimeService` (comet); tree save locations; relayed unlocks reaching Sight |
| 4 | **Steam around Steam's launch** (STORE-STEAM phase 2) | high: mods, settings, Sight and HUD on any Steam game | M | Med | generalised `steam-run`; save stores in a namespace; runs parallel to 3 once Sight exists |
| 5 | **Epic native installs and launch, then saves, play time, achievements** (phases 2–3) | high: Epic-only games playable | L + M | Med | reuses PieceInstaller (cheaper after 3); exchange-code plans; anti-cheat runtimes |
| 6 | **EA** (STORE-EA phases 0–2) | potentially high, depending on your EA titles | L+ | **High** | an in-daemon `RuntimeService` (LSX), after comet; licence files; Denuvo activation safety |
| 7 | **Meta Track A, then Track B if spikes clear it** | niche (VR) but personal | M, then L+ | Med, then **High** | `SharedRuntime` prefixes; `XrRequirement`; the XR presenter |
| later | Steam sign-in (phase 3) and depot installer (phase 4); comet in-process; EA fallback runner; Epic EGL v2 fallback; Meta's meta-bridge | | | | |

**Why this order.** Steam first because it is the cheapest step with the most value, needs no
credentials, and exercises the identity change across API, CLI and app before any new network code
exists. Epic phase 1 next because it is mostly library work that pays off at once: its titles are
Ubisoft games Quiver already runs. GOG before Epic's installer because GOG's content system is
documented and DRM-free, so the generic installer can be proven with no licensing in play; Epic's
shared chunks and encrypted manifests are better second. **If you own few GOG games, swap steps 3
and 5** (steer X17) and build PieceInstaller on Epic. EA comes after the host-service seam is proven
on comet, and carries the most operational risk (Denuvo lockouts, frequent breakage, ToS). Meta is
last: Track B waits on the review of Meta's patches (S3) and on the XR presenter, for the smallest
catalogue.

### 8.3 Cheap first wins

- **Steam: local files need no login.** Libraries, `appinfo.vdf`, art (built), play time from
  `localconfig.vdf`, achievements from the stats cache, cloud state from `remotecache.vdf`; Play
  through `steam://rungameid` (built).
- **Epic: Ubisoft titles on Epic are just a key redemption.** Activate once, and the existing
  Ubisoft path does the rest (CDN, bridge, Denuvo, saves, play time). Before building anything,
  Epic S1 (read-only, on the existing Ubisoft session) may show that 3539 and 420 are already
  Epic-redeemed copies Quiver plays today.
- **GOG: DRM-free games launch from `goggame-*.info` with no service.** An imported install plays
  at once; comet only adds SDK features.
- **Shared:** Move, built once, closes a gap Ubisoft has today; the 0600
  `localconfig.vdf.quiver-backup` is a one-line fix to existing code.
- **Meta Track A:** import the fork's `MetaGames` as verified installs, with library, art and
  updates, before any play work.
- **EA has no cheap win:** its first playable result needs auth, content, OOA and LSX together.

### 8.4 Spikes that need you

Each is asked for first and reverted after (the real-prefix rule); agents never run the ones that
sign in or capture traffic. Grouped by what they touch, in roadmap order; details are in the store
docs.

| Kind | Spikes | What you'd be asked |
|---|---|---|
| **Read-only, no sign-in** | Steam S3 (parse `appinfo` and stats caches); Epic S1 (`quiver ownership 3539`/`420` on the existing session) | OK to read these caches, or make this one call? |
| **The real Steam client and its files** | Steam S1 (generalised `steam-run` on a non-Ubisoft game), S2 (do `LaunchOptions` sync?), S4 (`CompatToolMapping` edit with Steam closed), S6 (own-Proton launch; optional) | which game; a Steam restart |
| **Store sign-in with your account** | Epic S0 (code login; does Heroic stay signed in?); GOG G1 (paste login, refresh rotation); EA S1 (browser flow); Steam S5 (QR on a *throwaway* account); Meta sign-in through its client | which account; whether a sign-out elsewhere is acceptable |
| **Downloads and launches of owned titles** | Epic S3–S5; GOG G2–G7; EA S2–S5; Meta S1, S2, S4, S5, S7, S8 | which titles (steer X15); EA S5 risks a Denuvo activation, so start with a non-Denuvo OOA title |
| **Captures and disassembly** | Epic S2 (`sdk-proxy` log of Origins); EA S6 (EA app achievement and session traffic, scratch prefix); Meta S3 (what `OAF_LAUNCH_CHECK` and the winefix skip); Meta native sign-in (TLS tap, phase 3) | OK to capture in a scratch prefix with your account? |
| **Writes to a store account** | Epic S6 (one play-time PUT); GOG G5 (an unlock on gog.com), G7 (play-time POST); any redemption test | explicit OK for each write |

---

## 9. Steers

### 9.1 Cross-store steers

| # | Steer | Options | Recommendation | Replaces |
|---|---|---|---|---|
| X1 | Identity scheme | (a) qualified `GameRef`, Ubisoft bare in existing keys; (b) migrate all keys; (c) map other stores into u32 | **(a)**: no appid churn, no migration | GOG 9, Steam 6, EA 12 |
| X2 | Library presentation and merging | (a) one library, store badges and filter, merge on explicit links; (b) store switcher; (c) automatic fuzzy merging | **(a)**, plus a manual "Show as one" | Epic 5, Steam 7 |
| X3 | Browser sign-in UX | (a) open browser, paste code or redirect URL; (b) also a WebKitGTK helper; (c) system-wide scheme handlers | **(a)** for every MVP; **(b)** later as one shared helper; (c) only where required (Meta) or opt-in (EA `qrc:`); Steam uses QR; never borrowed device-code clients | GOG 3, Epic 1, EA 3, Steam 2 |
| X4 | Posing as official clients | (a) accept where unavoidable, documented, honest UA elsewhere; (b) only stores with third-party clients | **(a)**, never as a mobile app | Epic 2, GOG 12, EA ToS notes |
| X5 | Prior art and licences | (a) native MIT Rust, GPL as oracle, MIT/Apache deps after review; (b) shell out to GPL tools; (c) relicense | **(a)**: egs-api after a one-day review; comet sidecar then in-process; gogdl and Maxima as oracles; Revive runtime if separable | Epic 3, GOG 1–2, EA 2, Meta 5 |
| X6 | Several accounts per store | (a) full model from the start, limits shown as refusals; (b) one per store first | **(a)** for Epic, GOG, EA, Steam (local users); **(b)** for Meta until per-account runtime prefixes | GOG 5, Meta 6 |
| X7 | Reporting to stores | play time on or off by default; presence on or off | play time **on** under one `report_playtime` switch once each endpoint is confirmed; presence **off** | GOG 6, Epic 11 |
| X8 | Cloud-save conflicts | (a) back up, pull/push, newer per file, ask on two-sided conflict; (b) refuse to sync on conflict | **(a)**; Steam delegated; never upload to Steam Cloud | GOG 8, Epic 7, Steam 8 |
| X9 | Achievements and toasts | (a) show each store's unlock state, toast from each event source, local earned set where unknown; (b) definitions only until all stores report | **(a)**, suppressing Sight's toast when the vendor overlay shows its own | GOG 10, EA 11, Steam 9 |
| X10 | Vendor overlays | (a) Sight only; vendor overlay opt-in per game where the vendor's runtime loads it (EOS); (b) inject vendor overlays (Galaxy's) | **(a)**: Quiver never injects proprietary overlays | GOG 7, Epic 8 |
| X11 | Anti-cheat titles | (a) hide; (b) "Not playable on Linux: <reason>", Play disabled, `--force`; (c) let them fail | **(b)**, list kept as data; runtimes from Steam, else the Lutris list | EA 7, Epic 10 |
| X12 | Vendors' own Linux clients, fallback runners | (a) native now, per-game `runner=` fallback later; (b) wait for vendors; (c) hand stores to their clients | **(a)**: EGL v2 after preview, the EA app for pre-DiP/Battlelog, RiftLift or the fork for Meta; proceed with GOG despite a possible official Galaxy | Epic 9, EA 1 and 13, Meta 1b, GOG 14 |
| X13 | Agent policy for store actions | the table in section 7, item 10 | **as tabled**; corrects STORE-STEAM's "sign-out is Ask" to Deny | Epic 6, Steam 12, EA policy notes |
| X14 | Imports from other launchers | installs only vs. also sessions | **installs only**, verified against manifests | Epic 12, GOG import notes |
| X15 | Test accounts and titles | which games you own where, and whether spikes may sign in | needed before sign-in spikes: GOG one Galaxy-SDK title with achievements and cloud saves; EA one OOA and one Denuvo title, no Javelin; Epic one small free and one `OwnershipToken` title; Meta one OpenXR and one LibOVR title | GOG 13, EA 6 |
| X16 | When to rename crates | (a) now; (b) quiet window after `build/options`, `build/mod-sdk`, `build/ubi-perf` merge; (c) never | **(b)**; R0–R2 go ahead now without renames | new |
| X17 | Store order | (a) section 8.2; (b) Epic native installer before GOG; (c) EA earlier | **(a)**, switching to **(b)** if your GOG library is small | new |

### 9.2 Store-specific steers (kept in their docs)

| Doc | Steers that stay there |
|---|---|
| STORE-STEAM | 1 install engine; 3 which games use `steam-run`; 4 Proton mapping; 5 where native installs go; 10 library before sign-in; 11 own-Proton launches; 13 Flatpak Steam |
| STORE-EPIC | 3 egs-api review; 4 MVP scope (phase 1 first); 9 EGL v2 fallback timing |
| STORE-GOG | 1 comet reuse; 2 gogdl; 4 which builds (Windows or native); 11 DRM-free extras archive; 14 official Galaxy for Linux |
| STORE-EA | 1 native LSX vs the EA app; 2 Maxima licence; 4 licence files never decrypted; 5 machine identity; 8 EA Play; 9 Steam and Epic copies; 10 traffic capture; 13 old titles |
| STORE-META | 1 Track B at all; 2 modifying Meta's binaries; 3 prefix model; 4 OVRPlugin unlocks; 5 compat runtime source; 7 default XR runtime; 8 VR HUD in MVP; 9 pinning Meta updates; 10 native sign-in capture |

---

## 10. Contradictions between the store docs, and how they were resolved

The "Edit" column is the small correction made to the store doc in this commit.

| # | Conflict | Resolution | Edit |
|---|---|---|---|
| 1 | STEAM, EPIC, GOG and EA deferred the cross-store architecture to "STORE-META", which is the Meta (Oculus) store scope | This document owns it | references in all four now point to STORES.md; STORE-META's header too |
| 2 | Identity type: `GameRef { store, id: String }` (GOG, Steam, Meta), `GameKey { store, id }` (EA), `enum GameKey` (Epic) | `GameRef { store, id: GameId }`, typed ids inside providers (2.1) | notes in EPIC 4.1 and EA 3.1 |
| 3 | Ubisoft prefix: `ubi:3539` (Steam) vs `ubisoft:3539` (Epic) | `ubisoft`, with `ubi` accepted as input | STEAM 3.2 |
| 4 | Data layout: `games/<store>-<id>/` (GOG, Epic, Steam's `mods/steam-…`) vs `games/<store>/<id>/` with Ubisoft read as `ubisoft/<id>` (EA) | `<store>-<id>`, Ubisoft bare, nothing migrated (2.3) | EA 3.1 and 6 |
| 5 | Export keys: `quiver:gog/<id>` (GOG), `quiver:ea/<offer>/<profile>` (EA) vs `quiver:epic:<app>/<profile>` (Epic), `quiver:steam:<id>` (Steam) | `quiver:<store>:<id>[/<profile>]`: `/` already separates the profile, and today's parser splits on the first `/` | GOG 2.2, 3.2, 5, 6; EA 3.1 |
| 6 | Keyring entries: `gog-session:<id>`, `ea-session:<id>`, `epic:<id>`, `steam:<steamid64>` | `<store>:<account id>`; Ubisoft keeps `session:<userId>` | GOG 4.2; EA 4.3 |
| 7 | Accounts index: "an `epic` section" (Epic) vs a `store` field per entry (GOG, EA, Steam) | `store` field, absent = Ubisoft | EPIC section 5 table |
| 8 | A cross-store fuzzy name match "is the merged title" (Epic) vs records linked by vendor-declared ids (Steam; Epic's own `GameID` rule) | merge only on explicit links; unlinked matches ask (2.4) | EPIC 4.1 |
| 9 | Agent policy for Steam sign-out: Ask (Steam) vs Deny (PROPOSAL, EA, Epic) | Deny, as PROPOSAL decided | STEAM section 4 |
| 10 | "Quiver never reports an achievement" (Steam) vs comet reporting unlocks to GOG | no conflict once stated: Quiver relays only what the game asked for, as the store's own SDK service, and never initiates (3.8) | none |
| 11 | Meta's MVP reads the FRL token from OAF's database vs "never read another client's tokens" | allowed only for the session Quiver's own sign-in just created in Quiver's runtime prefix, read once into the keyring; never from the Heroic fork's prefix (7, item 8) | note in META section 6 |
| 12 | Prefix models: per game (most), `compatdata` (Steam), shared runtime prefix (Meta) | `PrefixPolicy` with three variants (4) | none |
| 13 | Effort scales: M is 1–2 weeks (Epic, Steam) vs 1–3 weeks (GOG, EA); L is "several weeks" vs "more than a month" | normalised in this document | none (store docs keep their own) |
| 14 | A separate `gog-galaxy` crate (GOG) vs one crate per store | a `galaxy` module in `gog-core`; `gog-dummy-service` stays a crate (Windows target) | none |
| 15 | Steam's crate: `store-steam` or `ubi-core::steam` | `steam-local` (files) plus `steam-core` (provider, `cm` feature): shortcut exports need the file layer without the provider | none |
| 16 | EA phase 1 "Ubisoft paths migrated by reading them in place" vs no migration | follows from #4 | EA section 6 |
