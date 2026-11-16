# Steam copies of Ubisoft games (design notes)

Status: library, launch and play-session parts built on `feat/steam-library`; launch
settings (env, DLL overrides, args, wrappers) applied by `steam-run`. Mod overlays, config
patches and cloud saves for Steam copies are designed here and not built yet.

## 1. Why Black Flag Resynced wasn't in the library

`quiverd` builds the library from `ownership_service`'s `InitializeRsp.owned_games` and kept
only products that were owned, `product_type == 0` (a game), not locked by a subscription,
**and had a `latest_manifest`** (`Product::is_installable`: a PC build Ubisoft can download).

A game bought on Steam is activated on the Ubisoft account, so it is in the ownership list
under its own product id (66088 for Black Flag Resynced), with its configuration, space id
and app id, its add-ons (28 associated products, owned) and ownership tokens and tickets that
work (Denuvo activation and play sessions succeed). But Ubisoft distributes no PC build of it:
Steam does. So `latest_manifest` is empty (40 spaces) and the filter dropped it.

Evidence from the running daemon (read-only requests on its existing session, 2026-11-16):

- `game_info 66088`: found in the ownership list. Name from its config, space id
  `ee27f633-…`, app id `c7ddd369-…`, cloud saves and achievements on, 28 add-ons owned, no
  `installer.publisher` (configs of downloadable games have an `installer` section).
- `install_preview 66088`: `product has no PC build` (`builds::Latest::fetch`, raised when
  `latest_manifest` is empty). `not in your library` would mean the product isn't listed
  at all.
- `steam-run` got a launch context (`launch::build` needs the product in the ownership list)
  and the broker got an ownership token and a Denuvo token for 66088.

`quiver ownership 66088` (new, `Request::Ownership`) prints the raw fields (owned, type,
state, PC build, platform, activation type, target partner, …) and why a product is or
isn't listed. It needs the new quiverd, so the exact `state`, `platform` and
`activation_type` values for a Steam activation aren't recorded yet.

## 2. Steam copies in the library (built)

- The daemon's cached library (`owned_library`) keeps every owned, unlocked game, with
  `installable: false` for those without a PC build. Internal users (launch account choice,
  Steam games' "owned" check) see all of them.
- The library front ends get (`library`, i.e. `Request::Library`): every installable game,
  plus the non-installable ones Steam has installed, each with `steam: Option<SteamGame>`
  (app id, install folder, launch options, whether it starts through Quiver). A Steam install
  of a product no signed-in account has is listed too (`owned_by` empty), named and drawn
  from its product configuration (fetched once by product id on the default account's
  existing demux session). Steam installs come from the same scan as `SteamGames`.
- Everything keyed by product id works unchanged: art, game info, progression, achievements,
  challenges, Ubisoft play time, add-ons, links. The Saves tab reads Steam's compatibility
  folder (`<library>/steamapps/compatdata/<appid>/pfx/…/savegames/<user>/<product>`).
- **Play**: for a game only Steam has installed, `Request::Play` asks Steam to start it with
  `steam steam://rungameid/<appid>` (`exports::steamlaunch`). With Steam closed and quiverd a
  systemd service, Steam is started in its own scope (`systemd-run --user --scope`), so a
  quiverd restart doesn't take Steam down. It returns a finished Play job ("Asked Steam to
  start it"); the session appears when the bridge reports in.
- **Install, update, verify, uninstall**: Steam's. The app shows "Installed via Steam" and,
  without a Ubisoft build, "Owned on Steam"; its install dialog refuses Steam-only games.
  A product that has a Ubisoft build and a Steam install shows both; Quiver can still install
  its own copy.
- **Use Quiver**: the game page's Steam button switches an unpatched copy (the existing
  `SteamPatch`); once it's switched the button says "Starts through Quiver" and opens the
  menu, which can undo it. The Overview's "Steam copy" row stays as before.
- The app reloads the library on `SteamGamesChanged` (install, patch) and when a session
  involves a Steam copy, which shows as Playing while Steam runs it.

### Art (built)

A game bought on Steam has no art in its product config (no `thumb_image`,
`background_image`, `logo_image`). Every front end now takes art from the first source of
`ubi_api::art::candidates` that loads:

1. the product config (file names on Ubisoft's asset CDN);
2. UbiServices' games API, `GET /v1/spaces/global/ubiconnect/games/api?spaceIds=…` (what
   Connect uses; seen in its HTTP cache): `highBoxArt`/`lowBoxArt` for tiles, `background`
   for heroes, `gameLogoColor`/`gameLogoFlat` for logos, `gameAvatar` as icon. quiverd asks
   for the spaces of games whose config leaves a slot empty, 20 per request on the default
   account's session, and keeps answers (and misses) a day in
   `$XDG_CACHE_HOME/quiver/spaces.json`; after a failure it waits 5 minutes. It also fills
   game info's release date, description and extra links. Not yet exercised against
   Ubisoft (no session in development): `quiver info 66088` shows `art from  tile: Ubisoft
   games API, …` and a `released` row when it works, `quiver library --json` the URLs (`art.space`).
3. Steam's local library cache (`<Steam>/appcache/librarycache`, read-only;
   `exports::steamart`), in `SteamGame.art`: current hashed layout
   (`<appid>/<sha1>/library_capsule.jpg` 300×450, `library_hero.jpg` 1920×620, `logo.png`,
   `library_header.jpg`, and `<appid>/<sha1>.jpg`, the 32×32 icon), flat per app
   (`<appid>/library_600x900.jpg`, …) and the old `<appid>_library_600x900.jpg`;
4. Steam's CDN (`https://shared.steamstatic.com/store_item_assets/steam/apps/<appid>/
   library_600x900.jpg`, `library_hero.jpg`, `logo.png`), cached by the app like Ubisoft's.
   Only older apps are served at these flat paths: newer ones (3751950 among them) live
   under a content hash (`<appid>/<sha1>/library_hero.jpg`, the same hashes as the local
   cache's folders) and 404 here, so for them the local cache is what counts.

The user's own files (custom art) win over all of them. The per-game setting `art=steam`
(`quiver set <game> art=steam`, or the game page's "Art: … ▾" menu, offered for games with
a Steam copy) moves 3 and 4 ahead of 1 and 2. Logos painted into Ubisoft's hero art
(`BAKED_LOGO`) are only assumed when the hero shown is the config's.

## 3. Sessions (built)

- `quiver steam-run` sends its Steam app id with `LaunchContext`; quiverd notes it
  (`PlaySessions::from_steam`), and the bridge's leases for that product in the next 10 min
  (and for as long as that session lasts) are held as `steam`. `ActiveSession.steam_app_id`
  carries the app id. The app's Now Playing says "From Steam, through Quiver"; name, art and
  "Open game" come from the library entry, which now exists.
- **Three leases**: Black Flag Resynced keeps three SDK connections open on
  `uplay_ipc_pipe_` (conns 3, 4, 5 in its bridge log; two of `api_version 33559296`, one of
  `33558786`, the R1 and R2 DLLs), and opens and closes about fifteen more. The bridge took a
  lease per connection, so the broker had three TCP leases for one upstream session (Ubisoft
  saw one: leases share a session). Now the bridge holds one lease per process, shared by its
  SDK connections, released 30 s after the last one closes. quiverd also lists each holder
  once, for older bridges.

## 4. Quiver owning the launch of Steam copies

`steam-run` runs in front of Steam's own command (`<steam-run> steam-launch-wrapper -- reaper
SteamLaunch … -- _v2-entry-point --verb=waitforexitandrun -- proton waitforexitandrun
game.exe`), on the host, before the reaper and pressure-vessel. So it can change the
command, the environment and the filesystem view of everything Steam starts.

### Launch settings (built)

`Request::SteamLaunchPlan { product_id }` returns the game's current settings revision as it
applies to Steam: `env`, `dll_overrides`, `args`, `wrappers`; `proton` and `language` are
reported as not applied (Steam picks Proton and the language). `steam-run`:

- adds `args` after the game's own arguments (inside the bridge's command line);
- puts `wrappers` in front of Steam's whole command, as if typed before `%command%`;
- merges `dll_overrides` into `WINEDLLOVERRIDES` (after the Connect guard), replacing entries
  for the same DLL;
- sets `env` (except `WINEDLLOVERRIDES`).

`--no-settings` skips all of it; an older quiverd without the request just logs and goes on.
The settings are edited in the app's Launch options tab, which is now open for Steam copies,
with a note saying what Steam decides.

Unverified: which variables pressure-vessel passes into the container. It keeps most of
them, but resets some (`LD_*`, `PATH`, `XDG_*` are rewritten), so a few settings may not
reach the game. Wrappers outside the container (`gamemoderun`, `mangohud`) are what users
already put before `%command%`.

### Mod profiles, overlays and config patches (designed, not built)

Which profile: a Steam launch has no picker. Proposal: a per-game setting
`steam_profile=<id|vanilla>` (default vanilla, plus enabled fixes when fixes-on-vanilla is
on), shown on the game page next to the Play profile chip; `quiver steam-run --profile X`
overrides it from the launch options.

Plan: a Steam copy is a `Game` with `install_dir` = Steam's folder and `prefix` = Steam's
compatibility folder (`compatdata/<appid>/pfx`). `mods::launch_plan(&game, profile, source)`
then gives overlays (install folder, per-profile prefix folders), save forks and config
patches, exactly as for a Quiver install of the same product (mods are stored per product,
so both share them). A new request (`SteamPrepare { app_id, profile }`) would apply config
patches to Steam's prefix (Quiver records originals the same way) and return the plan.

Overlays: `steam-run` can't `unshare(CLONE_NEWUSER)` itself (it's a multi-threaded tokio
process, and `unshare`/`setns` into a user namespace need a single thread). The existing
`overlay::Namespace` already works around that: a holder process creates the namespace and
mounts; a child joins it with `setns` in `pre_exec` (after `fork`, single-threaded). So
`steam-run` would create the namespace, spawn Steam's command as a child that joins it, wait
for it and exit with its status, instead of `exec`. Steam's reaper and pressure-vessel then
run inside that namespace.

Checked here: nested user namespaces work (`unshare -U -m` inside another), and a `bwrap
--unshare-user` started inside a user+mount namespace with an overlay mounted over a folder
sees the overlay's files and writes go to its upper layer (scratch test, no Steam). Not
checked: pressure-vessel's own bind setup (it binds the Steam library and home with `--bind`,
which is recursive in bwrap, so the overlay mounted at the install path should come along),
Steam's "Stop" (it signals the reaper's process group; `steam-run` waiting as parent must
pass the exit through and not outlive it), and the Steam overlay and Steam Input (they ride
on `LD_PRELOAD` and the reaper, not on the filesystem; probably unaffected). Steam's
"verify integrity" must never run inside the namespace (it won't: Steam runs it, not the
launch).

Saves: a profile with its own saves would point the bridge's `savegame_path` at the
profile's folder in Steam's prefix (`steam-run` already writes `savegame_path` into the
context).

Local play history: a session no quiverd launch held (a Steam copy started by
`steam-run`, a bridge in a container) is recorded in the game's `playtime.jsonl` when its
last lease ends (`PlaySessions` hands it over as `Finished`), with the current settings
revision. quiverd's own launches are still recorded by their launch job.

### Cloud saves (gap)

Connect syncs a Steam copy's cloud saves; `steam-run` doesn't yet. Proposal: `steam-run`
asks quiverd to pull before the launch (`SavesSync`-like, on a folder in Steam's prefix),
and quiverd pushes when the Steam session's last lease ends. Black Flag Resynced writes
named files (`ACBlackFlag[AutoSave01].save`, the R2 layout) rather than Quiver's `<slot>.save`,
so slot listing and sync need the R2 mapping first; unverified how Connect names those in the
cloud.

## 5. Open questions

- Should owned games without a PC build and without a Steam install (other platforms,
  delisted builds) show too, e.g. greyed out with "Not on PC through Ubisoft"? Today they
  don't.
- Should Quiver offer to install its own copy of a game that has a Ubisoft build when Steam
  already has one? Today the Play button plays Steam's copy and Install isn't offered there.
- Should Play for an unpatched Steam copy ask first ("it opens Ubisoft Connect; use Quiver
  instead?") rather than starting Connect's flow?
