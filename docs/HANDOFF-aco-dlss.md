# Running AC Origins in containers without Ubisoft Connect (for the ac-dlss session)

Problem: every Ubisoft Connect you start (in each container) is another live login on the
account, and Ubisoft disconnects the others ("multiple login"). The ubisoft-linux daemon
(`ubid`) on the host now holds the account's single login. Containers stop running Connect
entirely; a small stand-in (`ubi-bridge.exe`) answers the game's Ubisoft SDK, and Denuvo/ticket
requests go to `ubid` on the host. One login, nothing to kick.

Verified: Origins boots to its menus with DLC and saves in a podman container this way
(`tools/instance.sh run` in this repo is a working reference).

## One-time

```sh
export PATH=$HOME/code/ubisoft-linux/target/release:$PATH
ubi status            # starts ubid if needed; shows "broker: 127.0.0.1:47810"
```

Then, in each instance prefix, **never start Connect again**. To be sure the SDK can't fall
back to it, rename Connect's executables in the instance's
`drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/`:
`upc.exe UbisoftConnect.exe UbisoftGameLauncher.exe UbisoftGameLauncher64.exe UplayService.exe
UplayWebCore.exe UbisoftExtension.exe` → append `.off`. Keep the `uplay_*.dll` files; the game
loads them.

## Per launch

1. **Launch context** (fresh ticket, valid for a few hours; uses ubid's login, no new session):
   ```sh
   ubi --source aco-dlss context origins -o <inst>/out/context.json \
     --install-dir "<host path of the instance's Assassin's Creed Origins folder>" \
     --savegame-path 'C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\savegames\<account_id>\3539'
   ```
   `<account_id>` is the `account_id` field in the written JSON (or the existing folder name under
   `savegames/`). The bridge adds the trailing backslash the SDK needs.
2. **Bridge binary**: copy `~/code/ubisoft-linux/target/x86_64-pc-windows-gnu/release/ubi-bridge.exe`
   to `<prefix>/drive_c/ubi/ubi-bridge.exe`.
3. **Container networking**: run the container with `--network host` so the bridge reaches
   `127.0.0.1:47810`. Host networking shares the abstract X11 socket namespace: never run an X
   server on `:0` (or any low display) in such a container, or desktop X clients connect to it
   instead of the real Xwayland. Use a high display like `:97`, or occupy `/tmp/.X0-lock`…
   `/tmp/.X9-lock` first so the server picks `:10`+.
4. **Start the bridge first** (inside the container, same prefix/wineserver as the game):
   ```sh
   UBI_BRIDGE_LOG='Z:\out\bridge.log' python3 "$PROTON/proton" run 'C:\ubi\ubi-bridge.exe' 'Z:\out\context.json' &
   # wait until /out/bridge.log contains "uplay_ipc_pipe"
   ```
5. **Start the game with Connect's launch arguments** (required, or the game relaunches itself
   through UbisoftGameLauncher.exe and exits):
   ```sh
   b64() { printf %s "$1" | base64 -w0; }
   EXE='C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\games\Assassin'"'"'s Creed Origins\ACOrigins.exe'
   export UPLAY_ARGUMENTS="-upc_uplay_id 3539 -upc_game_version 1 -upc_exe_path $(b64 "$EXE") -upc_working_directory $(b64 "${EXE%\\*}")"
   python3 "$PROTON/proton" run "$EXE"
   ```
   Use `proton run`, not `waitforexitandrun` (that waits for the bridge's wineserver).

## Gotchas

- **Exit detection**: `proton run` for the game may not return while the bridge lives
  (Proton's steam.exe waits for every process in the prefix). Watch for `ACOrigins.exe`
  disappearing, then kill the bridge / `wineserver -k`.
- **Saves**: the bridge never uploads to the cloud, so instance saves stay local (good for
  experiments). The SDK reads/writes `<savegame-path>\N.save` directly.
- **Content check**: "Checking for additional content…" should take a few seconds; if it
  takes minutes, the context is missing the email (regenerate it with a current `ubi`).
- **Logs**: `/out/bridge.log` shows every SDK request/answer and Denuvo activation.
- If `ubi context` fails with a login error, run `ubi login` once on the host.
