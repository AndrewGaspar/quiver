# Quiver

A third-party launcher for your Ubisoft library on Linux. Not affiliated with or endorsed by
Ubisoft.

A native Linux client for a Ubisoft library: sign-in, library, installs and updates, one Proton
prefix per game, cloud saves, play time, mod overlays and Steam/desktop shortcuts, all without
running `UbisoftConnect.exe`. A daemon (`quiverd`) owns everything; the desktop app, the
`quiver` CLI and scripts or agents all drive it the same way.

![Library](docs/screenshots/library.png)

| Game page (controller) | Launch options with history | Light theme (follows Omarchy) |
|---|---|---|
| ![Game page](docs/screenshots/game-controller.png) | ![Launch options](docs/screenshots/launch-options.png) | ![Light theme](docs/screenshots/light-theme.png) |

<sub>Screenshots use the app's demo data (`QUIVER_DEMO=1`); box art is Ubisoft's, shown from
their asset CDN.</sub>

## Layout

- `proto/`: Ubisoft Connect protobuf definitions, vendored (see `proto/README.md`)
- `crates/ubi-proto/`: generated protobuf types
- `crates/ubi-core/`: UbiServices auth, the demux client, ownership/library and manifest parsing
- `crates/ubi-cli/`: the `ubi` command
- `crates/ubi-ipc/`: types shared by the native side and the bridge
- `crates/ubi-bridge/`: a Windows binary that runs inside the game's prefix in place of Connect
- `tools/instance.sh`: per-game reflinked prefixes, run headless in podman
- `tools/recon/`: SDK harness and the offline recon prefix scripts

## How a game runs without Connect

The game's `uplay_r1_loader64.dll` loads Ubisoft's own `uplay_r164.dll` and `uplay_aux_r164.dll`
from the Connect install directory, unmodified. They reach Connect through named pipes
(4-byte big-endian length + protobuf), and `ubi-bridge` owns those pipes:

| Pipe | Protocol | What the bridge does |
|---|---|---|
| `terminal_1_uplay_aux_ipc_pipe_` | `uplayauxdll` | init, then Denuvo activation: forwards the request token to the broker in `quiverd` |
| `terminal_1_uplay_api_process_ipc_pipe_` | `uplaydll` | `InitProcessReq` handshake |
| `terminal_1_uplay_ipc_pipe_` | `uplaydll` | `InitReq` (account, ticket, owned IDs, CD keys), then the SDK API |

- **`UPLAY_ARGUMENTS`:** the game must be started with this set to
  `-upc_uplay_id <id> -upc_game_version 1 -upc_exe_path <b64> -upc_working_directory <b64>`.
  Otherwise the aux DLL relaunches it through `UbisoftGameLauncher.exe`.
- **Broker (in `quiverd`):** runs on the host and holds the Ubisoft session. For a Denuvo token it fetches
  an `ownership_service` ownership token, then calls `denuvo_service.GetGameTokenReq`.

## Debugging a game against the bridge

- `tools/recon/sdk-proxy`: a logging stand-in for `uplay_r1_loader64.dll` (`install.sh install|remove`).
  Diffing its log from a real Connect session against a bridge session is the fastest way to find
  what the bridge gets wrong. That's how the missing account email (which stops Origins from
  listing saves) was found.
- `tools/proto-extract/extract.py <binary>`: dump the protobuf schemas embedded in Ubisoft binaries.
- `UBI_TRACE=+file tools/instance.sh run ...` then `tools/instance.sh trace-grep <name> <regex>`.

## Architecture

```
 quiver-app (GPUI)   quiver (CLI)   scripts / agents
            \              |              /
             +---- ubi-api: JSON lines over $XDG_RUNTIME_DIR/quiver/quiverd.sock
                            |
                        quiverd  (daemon; started on demand by any client)
            session · library · installs · launches · settings history · activity log
                 · Denuvo token broker for every running game's ubi-bridge
                            |
                         ubi-core (Ubisoft protocols, installer, saves, launcher)
```

Front ends hold no state. Every request carries a source label (`gui`, `cli`, or whatever
`--source`/`QUIVER_SOURCE` says), so the activity log and each game's settings history show who
changed what.

## Usage

```sh
rustup target add x86_64-pc-windows-gnu
cargo build --release -p ubi-bridge --target x86_64-pc-windows-gnu
cargo build --release
export PATH=$PWD/target/release:$PATH

quiver login                                   # email, password, 2FA (or sign in from the app)
quiver login                                   # again: adds a second account (first stays default)
quiver account list                            # signed-in accounts; * marks the default
quiver account default family                  # change the default (by name or user id)
quiver logout family                           # sign one account out (default: the default)
quiver-app                                  # the app
quiver library                                 # owned games, install state, owners (2+ accounts)
quiver install "chronicles china"              # by name or product id; shows progress
quiver play rabbids                            # runs in its own prefix; syncs cloud saves
quiver set origins proton=GE-Proton10-34 env.DXVK_HUD=fps -m "test HUD"
quiver history origins                         # every revision, with who made it
quiver restore origins 2                       # make revision 2 current again (0 = defaults)
quiver --source agent set origins wrappers=gamemoderun,mangohud
quiver activity                                # recent actions across GUI/CLI/agents
quiver watch                                   # live daemon events (JSON)
quiver export desktop origins [--remove]       # ~/.local/share/applications/quiver-<id>.desktop
quiver export steam origins [--user NAME]      # non-Steam shortcut with Ubisoft art (staged while Steam runs)
quiver export list                             # exports and whether they're still as written
quiver steam users                             # Steam users; which one gets shortcuts
quiver steam restart                           # asks first; writes staged shortcuts while Steam is closed
quiver dev …                                   # low-level tools that bypass the daemon

quiver mods odyssey layer add ReShade ~/mods/reshade   # copy a folder laid out like the install
quiver mods odyssey profile create "Visual overhaul" --layers hd-textures,ReShade
quiver mods odyssey profile set "Visual overhaul" dll.dxgi=n,b cloud_saves=on
quiver mods odyssey list                       # layers, profiles, file conflicts between layers
quiver play odyssey --profile "Visual overhaul"  # or --profile-id p7f3k2q; vanilla by default
quiver mods odyssey profile history "Visual overhaul"
quiver mods odyssey profile delete "Visual overhaul"   # restorable; saves are kept
```

## Mods

A **layer** is a folder of files laid out like the install folder; `layer add` copies it into
`~/.local/share/quiver/mods/<product>/layers/<id>/` (reflinks on btrfs/xfs) and matches file
names to the install's case. A **profile** stacks layers bottom to top, can override launch
settings (`proton`, `env.*`, `dll.*`, `args`, `wrappers`) and has its own saves
(`savegames/<user>/<product>-<profile id>` in the prefix), synced with the cloud only with
`cloud_saves=on`. Vanilla is implicit and never mounted.

A modded launch never changes the install folder. `quiverd` starts a holder process that calls
`unshare(CLONE_NEWUSER | CLONE_NEWNS)`, maps your uid and gid to themselves and mounts
overlayfs over the install path (lower: the layers, then the install; upper:
`mods/<product>/profiles/<id>/upper`, where everything the game writes lands). The bridge,
Proton, its wineserver and the game join that namespace with `setns`; the daemon never does.
Profile changes are revisions in `games/<product>/profiles/<id>.jsonl` (`profile history`,
`profile restore`), and deleting a profile is a revision too. Agents (`--source` other than
`cli`/`gui`) need a person's confirmation to delete or restore profiles and remove layers.

Settings keys: `proton`, `language`, `account`, `args`, `wrappers`, `env.NAME`, `dll.NAME`
(`key=` unsets). A game launches (and installs, syncs cloud saves, reports play time) with its
`account` if set, else the default account if it owns the game, else the first account that does.
State lives in `~/.local/share/quiver/` (`config.json`, `games.json`, `accounts.json`,
`games/<id>/history.jsonl`, `activity.jsonl`, save backups, `quiverd.log`); a pre-Quiver
`~/.local/share/ubisoft-linux/` and keyring entry are moved there automatically. `ubi` is kept
as an alias for `quiver`.

## Roadmap

1. Core and CLI: auth, library, manifests (done)
2. Launch bridge: AC Origins (uplay R1 SDK) reaches the main menu with Denuvo and DLC working (done)
3. Bridge coverage: Origins loads saves and reaches gameplay menus. Still to do: real achievements
   and Club actions (UbiServices), cloud save sync, newer `upc_r2` games, ticket refresh on long
   sessions
4. Installer: done (zstd `slices_v3` and legacy zlib `slices` layouts; updates in place)
5. Cloud saves: done (cloudsave_service V2, pull before / push after each session)
   Play time: reported to Ubisoft like Connect does (UbiServices playsession start, heartbeat
   every 330 s, end; `report_playtime` in config.json turns it off)
6. GPUI front end: library, install/update with progress, play
7. Next: fetch the SDK DLLs from Ubisoft instead of a local Connect install, real
   achievements/Club, LZHAM manifests, `upc_r2` games

## License and credits

Quiver is MIT-licensed (see [LICENSE](LICENSE)). It is an independent project, not affiliated
with or endorsed by Ubisoft; "Ubisoft" and game names are trademarks of their owners. Quiver
needs your own Ubisoft account and only runs games you own.

- `proto/`: protobuf definitions of Ubisoft Connect's protocols, originally vendored from
  [YoobieRE/ubisoft-demux-node](https://github.com/YoobieRE/ubisoft-demux-node) and refreshed
  from the client's embedded descriptors with `tools/proto-extract` (see `proto/README.md`).
- `crates/ubi-app/src/text_input.rs`: adapted from GPUI's `examples/input.rs` (Apache-2.0,
  Copyright Zed Industries).
- `crates/ubi-app/assets`: Geist and Geist Mono (OFL), Archivo (OFL) and Lucide icons (ISC),
  with their license files.
- `third_party/xattr-0.2.3`: the `xattr` crate (MIT/Apache-2.0) with a one-line patch GPUI
  needs; its license files are included.
