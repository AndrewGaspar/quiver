# ubisoft-linux

A native Linux client for a Ubisoft library: login, library, install and one Proton prefix per game, without running `UbisoftConnect.exe`. For personal use.

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
| `terminal_1_uplay_aux_ipc_pipe_` | `uplayauxdll` | init, then Denuvo activation: forwards the request token to `ubi broker` |
| `terminal_1_uplay_api_process_ipc_pipe_` | `uplaydll` | `InitProcessReq` handshake |
| `terminal_1_uplay_ipc_pipe_` | `uplaydll` | `InitReq` (account, ticket, owned IDs, CD keys), then the SDK API |

- **`UPLAY_ARGUMENTS`:** the game must be started with this set to
  `-upc_uplay_id <id> -upc_game_version 1 -upc_exe_path <b64> -upc_working_directory <b64>`.
  Otherwise the aux DLL relaunches it through `UbisoftGameLauncher.exe`.
- **`ubi broker`:** runs on the host and holds the Ubisoft session. For a Denuvo token it fetches
  an `ownership_service` ownership token, then calls `denuvo_service.GetGameTokenReq`.

## Debugging a game against the bridge

- `tools/recon/sdk-proxy`: a logging stand-in for `uplay_r1_loader64.dll` (`install.sh install|remove`).
  Diffing its log from a real Connect session against a bridge session is the fastest way to find
  what the bridge gets wrong. That's how the missing account email (which stops Origins from
  listing saves) was found.
- `tools/proto-extract/extract.py <binary>`: dump the protobuf schemas embedded in Ubisoft binaries.
- `UBI_TRACE=+file tools/instance.sh run ...` then `tools/instance.sh trace-grep <name> <regex>`.

## Usage

```sh
# Build the Windows-side bridge (once) and everything else
rustup target add x86_64-pc-windows-gnu
cargo build --release -p ubi-bridge --target x86_64-pc-windows-gnu
cargo build --release

./target/release/ubi login                 # email, password, 2FA; session kept in the keyring
./target/release/ubisoft-linux             # the GPUI app: library, install/update, play
```

The CLI does the same things as the app:

```sh
ubi library [--all]           # owned games
ubi install <id> <dir>        # download/update (resumable), registers the game
ubi games                     # installed games
ubi play <id>                 # run it: own prefix, bridge, cloud-save sync around the session
ubi status <id>=<dir>...      # update check for existing installs
ubi saves pull|push ...       # manual cloud-save sync
```

State lives in `~/.local/share/ubisoft-linux/` (`config.json`, `games.json`, save backups).
Defaults: games in `~/Games/ubi-library`, prefixes in `~/Games/ubi-prefixes`, newest GE-Proton
from `~/.local/share/Steam/compatibilitytools.d`. Ubisoft's SDK DLLs are copied from an
existing Connect install (`sdk_source` in `config.json`).

## Roadmap

1. Core and CLI: auth, library, manifests (done)
2. Launch bridge: AC Origins (uplay R1 SDK) reaches the main menu with Denuvo and DLC working (done)
3. Bridge coverage: Origins loads saves and reaches gameplay menus. Still to do: real achievements
   and Club actions (UbiServices), cloud save sync, newer `upc_r2` games, ticket refresh on long
   sessions
4. Installer: done (zstd `slices_v3` and legacy zlib `slices` layouts; updates in place)
5. Cloud saves: done (cloudsave_service V2, pull before / push after each session)
6. GPUI front end: library, install/update with progress, play
7. Next: fetch the SDK DLLs from Ubisoft instead of a local Connect install, real
   achievements/Club, playtime reporting, LZHAM manifests, `upc_r2` games
