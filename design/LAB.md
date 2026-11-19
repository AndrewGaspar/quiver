# Quiver lab: disposable game instances for agentic modding

Status: built 2026-11-18 (`quiver lab`, `quiver_core::lab`, `Request::LaunchMaterial`, lab
sessions in the broker). Not yet run against a real game: section 9 is the first live run.

The Pharos mod (AC Origins DLSS, Reflex, frame generation) was built over a week by an agent
driving the game through a private harness (`ac-dlss/scripts/aco` and its `tools/`):
reflink clones of a "gold" prefix in podman, gamescope headless, NVIDIA through CDI, and the
licence through Quiver's bridge and broker. `quiver lab` makes that a standard, store-agnostic
Quiver capability, with every lesson of that week designed in, so that **starting an agent on
modding a new game is one step**:

```sh
quiver lab brief "<game>" -o brief.md      # paste it into a new agent session
```

## 1. What it is

An **instance** is a disposable, headless copy of a game the user has installed, running in a
container, launched with the user's real licence, that an agent drives from the command line:
launch, wait for a real readiness signal, press keys, move the mouse, take screenshots, edit
config files live, measure, kill, relaunch. Nothing an instance does reaches the user's
accounts, saves, play time or achievements, and the user's own play always wins.

```
quiver lab create <game> [--name N] [--display 2560x1600@165] [--proton P] [--mod DIR] [--graphics G]
quiver lab up N | down N | rm N | list | show N | edit N key=value…
quiver lab launch N [--mod DIR] [--wait frame]     quiver lab kill N
quiver lab status N | wait N STATE [--stable]       quiver lab logs N [--tail 40]
quiver lab shot N [--settle S] [--crop C] [--metrics]
quiver lab key N KEY… | type N TEXT | mouse N DX DY | click N FX FY [--expect-change]
quiver lab set N FILE:[section]key=value… [--wait] | profile N [NAME]
quiver lab fixture save|install|list … | call N VERB … | where N | teleport N LOC
quiver lab gpu [--for N] -- CMD… | bench N --variant A=… --variant B=… … | host
quiver lab brief <game> | doctor | image build|status | job list|show|wait
quiver lab … --bg      (any command, as a background job)
```

Game arguments are refs as everywhere in Quiver (`ubisoft:3539`, `epic:<app>`, `steam:<appid>`,
or a name), `--json` where output is data, and `--source` labels the caller in quiverd's
activity log (launch material is logged as "issued lab launch material … session lab-…").

## 2. The model

**Folders** (`$QUIVER_LAB_ROOT`, default `quiver-lab` next to Quiver's prefixes folder, so on
the same filesystem as the installs):

```
gold/<game dir>/{prefix,install,gold.json}     the game as the user plays it, Connect's login removed
instances/<name>/lab.toml                      the manifest
                 prefix/ install/              reflink clones of the gold copy
                 lab/                          mounted at /lab (Z:\lab): the lab channel, runs, shots
                 lab/runs/r<N>/                one folder per launch: game.log, bridge.log, DXVK/vkd3d
                                               logs, host.json, the mod's own log (QUIVER_LAB_RUN_DIR)
                 state.json events.jsonl       the supervisor's view; one event line per change
                 orig/ mod-files.txt           install files a mod replaced, put back before the next launch
fixtures/<game dir>/<name>/                    save fixtures (read-only), locations.txt
profiles/<game dir>/<name>.set                 graphics profiles (config-set lines)
jobs/<id>/                                     background jobs
gpu.lock gpu.holder gpu.log                    the GPU-exclusive lock
```

**The gold copy** is made once per game (`create`, or `create --regold`): `cp -a
--reflink=always` of the game's prefix and install (refused while anything runs in that
prefix; refused on a filesystem without reflinks rather than copying a whole game), then the
Connect guard (section 4). Instances are reflink clones of the gold copy: free, and nothing in
an instance points back at the user's folders.

**The manifest** (`lab.toml`, `quiver_core::lab::manifest`, `deny_unknown_fields`) pins what
a launch runs: the display (`width`, `height`, `refresh`; every tool reads the size from here,
clicks are fractions of it), the Proton build (default: the one the user plays the game with)
with the DXVK, vkd3d-proton and dxvk-nvapi versions it ships, the original install and prefix
paths (the clones are **mounted at those paths** in the container, so paths the game or its
registry recorded keep working), the exe and its arguments, environment and DLL overrides
(the game's Quiver settings when made), the mod folder, the graphics profile, the save folders
fixtures copy, file aliases for `set` (`pharos = "install:pharos.ini"`), `throttle` lines for
the GPU lock, and the mod's `reload_pattern` and `log_file`. Change it with `quiver lab edit`.

**The container** (`quiver_core::lab::podman`, image `localhost/quiver-lab:1` from
`contrib/lab/Containerfile`, built with `quiver lab image build`):

| Choice | Why (Pharos lesson) |
|---|---|
| gamescope `--backend headless` as the display | Xvfb makes NVIDIA's Vulkan present a ~70 ms CPU copy per frame (1). Xvfb isn't in the image. |
| its own network namespace (`--network=pasta:-T,<broker port>`), never `--network host` | with host networking the container shares the host's abstract X socket namespace and once took `:0`, capturing the desktop's clients (2). pasta's `-T` forwards the container's `127.0.0.1:<port>` to the host's broker. Verified: from inside, `:0`, `:1` and the host's other displays are unreachable, and the forward works. |
| `:0`–`:9` reserved inside the container | belt and braces for (2): lock files naming gamescope's pid; the display script refuses a single-digit display. Every `exec` reads the display gamescope reported (`/tmp/display`), never a default. |
| CDI `nvidia.com/gpu=all` plus `/usr/lib32` NVIDIA libraries mounted read-only, lib32 Vulkan in the image | CDI injects only the 64-bit driver (3) |
| `--init` | gamescope doesn't reap orphaned Wine processes (4) |
| `--userns=keep-id`, the host's hostname and `/etc/machine-id` | files keep the user's ownership; the machine looks the same to Ubisoft and Denuvo |
| `/usr/lib/nvidia/wine` mounted | NVIDIA's NGX DLLs (DLSS) |
| per-instance NVIDIA shader cache (`lab/cache/nv`) | relaunches don't recompile |

## 3. Licensing, per store

The rule is Quiver's (STORES.md §7): the store's real backend decides, for the user's own
account, through Quiver; no emulators, no entitlement stubs, no second login.
`Request::LaunchMaterial { game, session, … }` (quiverd `lab.rs`; STORES.md §3.6 planned it as
`LaunchMaterial`) mints one launch's licence from quiverd's own session. The instance never
sees a refresh token, a password or a store login.

- **Ubisoft** (`licence = "ubisoft-bridge"`): the bridge's launch context (ownership ticket,
  owned ids, CD keys, the broker's address), the same as `quiver context` makes, with the lab
  session id set. The supervisor writes it 0600 into the run folder, installs the current
  bridge in the instance's prefix (`C:\quiver\quiver-bridge.exe`) and starts it in launcher
  mode (`--run context -- game.exe`), which owns the SDK pipes, starts the game with
  `UPLAY_ARGUMENTS`, and deletes the context once read. Denuvo goes through quiverd's broker
  (`127.0.0.1:47810`, forwarded). One login for everything, held by quiverd. A bridge built
  before lab sessions (no `quiver_ipc::LAB_PROTOCOL_MARKER` in it) is refused: it wouldn't tell
  the broker the session is a lab one.
- **Epic** (`licence = "epic-exchange-code"`): EGL's arguments with a fresh single-use
  exchange code from quiverd (`epic_core::Epic::launch_material`), and the ownership token when
  the game wants one, written 0600 into the run folder (`-epicovt=Z:\lab\runs\rN\<ns><item>.ovt`)
  and deleted at the next kill. The refresh token stays in quiverd's keyring. EOS then talks to
  Epic itself over the container's network. **Caveat:** a game that syncs its saves through
  EOS Player Data Storage does that itself; nothing in Quiver can stop it without touching the
  game's own code. The brief tells the agent to check and to keep such games' cloud sync off.
- **DRM-free** (`licence = "none"`): nothing to mint. Today that is Steam games without
  Steamworks or SteamStub (below). GOG has no provider in Quiver yet (not built).
- **Steam**: see section 7.

## 4. Safety invariants

Each is enforced in code, not by convention, and has a test.

| Invariant | Enforced by |
|---|---|
| Connect never runs in an instance; no copied login or ticket | `lab::guard`: credentials (`ConnectSecureStorage.dat`, `user.vdat`) and launch material Quiver's own launches left in the prefix (`quiver-run/context.json`, `*.ovt`) deleted and executables renamed `*.quiver-lab-off` in the gold copy and every instance, re-checked before every launch (`guard::enforce` fails the launch if anything is left); Connect's executables disabled in every launch's `WINEDLLOVERRIDES`; a Connect process seen in an instance kills the run and is recorded under `violations` (`procs::connect_in`). |
| The user's real prefix and install are never mounted or linked | instances are reflink clones; `podman::run_args` mounts only the instance's folders (test `never_host_networking_and_mounts_the_clones`) |
| The user's play wins | every launch first checks for Windows programs outside every podman container (`procs::host_games`, by cgroup and `WINEPREFIX`) and for quiverd sessions of the user's (launcher, Steam, Epic local sessions); it refuses with what is running. Programs in a prefix under a `…/target/…` build folder (a checkout's own Wine tests) are not the user's. |
| Instance processes are identified by container, never by name alone | `procs`: the libpod cgroup of each process, then `WINEPREFIX`; kill-all is `wineserver -k`, then SIGKILL of what is left in the container's cgroup. No `pkill -f` anywhere. |
| Kill everything before every launch | the supervisor's launch starts with kill-all (a stale wineserver or bridge hangs the next game before its SDK answers) |
| One live login per account | launch material comes from quiverd; no store client runs in an instance; no credential file is read |
| **Nothing is reported** (section 5) | quiverd, by session kind |

## 5. Nothing is reported

Lab launches carry a **lab session id**, `lab-<instance>-r<run>` (`quiver_api::lab::is_lab_session`).
quiverd decides by that id, never by what a script remembers to pass:

| What | Normal launch | Lab session |
|---|---|---|
| Ubisoft play time (UbiServices `playsession` POST/PUT/DELETE) | the broker's `PlaySession` lease reports it | the bridge passes its context's session with `PlaySession`; the broker holds it as the `lab` holder; `PlaySessions` never starts, beats or ends a session only lab holders hold (`playsession::reportable`). If the user later joins the same game, their share is reported from then. |
| Quiver's play history (`games/<id>/sessions`, `on_finished`) | recorded | not recorded while only lab holders held it (`playsession::finished`) |
| Ubisoft achievements (`AchievementEarned`: local earned set, report journal, UbiServices) | recorded and reported | the bridge passes its session; the broker answers `Ok` and records nothing (`broker::lab_answer`) |
| Mod channel (Options, perf metrics, mod revisions) | served | refused for lab sessions (`broker::lab_answer`, and `ModRegistry::serve` again); the perf hub never files a lab session's metrics under the user's session (`PerfHub::with_session`). A lab mod is configured through its files (`quiver lab set`). |
| Ubisoft cloud saves | pulled before, pushed after | never: the bridge never uploads, Connect's `syncsavegames` is forced off, and quiverd's save sync is only around its own launches |
| Epic play time | queued after the session, reported | `Epic::queue_session` refuses a lab session id (never queued, held or sent); lab launches never go through quiverd's Epic session (`epicgame::begin/end`: no save sync, no achievement polling) |
| Epic cloud saves (EGS savesync) | around quiverd's launches | never (see the EOS caveat in section 3) |
| Steam play time, achievements, cloud | Steam's | a lab never talks to Steam (section 7), so nothing reaches it |
| `quiver sessions` | the game, holders `launcher`/`bridge`/`steam` | shown with the holder `lab`, so lab runs are visibly not the user's |

Tests: `playsession::tests::lab_bridges_are_lab_holders` (no report for lab-only holders;
reported once the person joins), `…::sessions_without_a_launcher_are_handed_over` (no history),
`broker::lab_tests` (earns not recorded, mod channels refused, play sessions held as lab),
`perf::tests::lab_metrics_never_land_in_the_persons_session`,
`epic-core phase3_tests::lab_sessions_are_never_queued_or_sent`, and the bridge's
`lab_sessions_travel_with_play_sessions_and_earns`.

## 6. The supervisor and readiness

One supervisor per instance (`quiver lab supervise`, started by `up`, detached, a lock file
so there is never a second; talked to over `supervisor.sock`) owns launches and kills, so
agents never assemble pkill/sleep sequences:

- **launch**: the user-playing check; kill-all; the Connect guard; the mod folder's files over
  the install (the previous launch's replaced files put back first); the graphics profile;
  a new run folder with `host.json`; launch material; `podman exec -d … proton run …` (never
  `waitforexitandrun`). The game's environment carries `QUIVER_LAB=Z:\lab`,
  `QUIVER_LAB_RUN=rN`, `QUIVER_LAB_RUN_DIR`, `QUIVER_SESSION`, `QUIVER_GAME`, DXVK and Proton
  log paths into the run folder, and the manifest's env.
- **watch** (every 500 ms): the readiness ladder **process** (the exe runs in the container),
  **window** (gamescope's `GAMESCOPE_FOCUSED_WINDOW` is set), **frame** (a screenshot isn't
  blank), then **exited** (with how: `exited`, `killed`, `violation`, `failed to start`,
  `container stopped`); the mod's state; Connect.
- `state.json` holds the current run, when each rung was reached, the mod's state, pids,
  exit and violations; `events.jsonl` has one line per change, tagged with the run id, so an
  agent never mistakes the previous run's lines for this one's.

`quiver lab wait N STATE` waits for `process|window|frame|exited` or **a state the mod
reports** (`menu`, `loading`, `in-world`…), and fails at once if it can't happen any more (the
game exited). `--stable` then waits until consecutive frames stop changing.

**The lab channel** (store-agnostic: works without the bridge, for Epic and DRM-free games):

- `%QUIVER_LAB%\state`: the mod writes one word (and `key=value` lines) and renames it into
  place. Only writes after the run started count.
- `%QUIVER_LAB%\req\<id>.json` → `%QUIVER_LAB%\rsp\<id>.json`: `quiver lab call N VERB ARGS…`
  (`{"id","verb","args"}` → `{"ok":true,"result":…}`). `where` and `teleport` are calls;
  `teleport LOCATION` reads `fixtures/<game>/locations.txt` (`name x y z [yaw [pitch]]`). This
  is the generic form of Pharos's `where`/`teleport` files: any mod can fill it.
- Live config: the mod re-reads its file; `quiver lab set … --wait` waits for the manifest's
  `reload_pattern` in its `log_file` instead of sleeping.

## 7. Steam

**Findings.** A Steamworks game (`steam_api*.dll`) calls into Proton's `lsteamclient`, which
loads the native `steamclient.so` and talks to the **running Steam client** over per-user IPC:
POSIX shared memory and semaphores in `/dev/shm` (`u<uid>-Shm_*`, `u<uid>-ValveIPCSharedObj-*`),
`~/.steam/steam.pid` and `steam.pipe`, and checks on the calling process. Making that work from
a container means sharing `/dev/shm`, `~/.steam`, the IPC and **PID namespaces** with the host
and mounting Steam's runtime: the container would no longer be isolated. And it would disturb
the user's Steam exactly as playing the game does: the user shows as "Playing X" to friends,
the session counts as play time, stats and achievements go to the account, **Steam Cloud
uploads the instance's saves at exit**, and Steam refuses a second launch of the same app.
SteamStub (`.bind` section) needs the client too. A second Steam login is not acceptable, and
emulators are out (STORES.md §7).

**So, built:** DRM-free Steam games only. `create` reads the install (`lab::facts`): no
`steam_api*.dll`, no import of it, no `.bind` section → `licence = "none"`; Steam doesn't say
which exe to run outside it, so `--exe` is required. The container has no `~/.steam` and a
private `/dev/shm`, so such a game can't reach the user's Steam even if it tries (a game that
calls `SteamAPI_RestartAppIfNecessary` just exits). **Refused:** Steamworks and SteamStub titles,
with this explanation (`quiver lab brief` says so too). Nothing a lab supports counts toward
Steam play time.

**Not built (would need the user):** a Steamworks lab would have to run inside the user's real
Steam session (e.g. `quiver steam-run` launching into a container), accepting play time,
presence, achievements and cloud uploads. That contradicts section 5, so it isn't offered.

## 8. Tools (generalised from the Pharos harness)

| Pharos tool | Lab |
|---|---|
| `aco shot` | `quiver lab shot` (gamescope's own screenshot of what the game presents; `--settle` for TAA/upscaler convergence; `--crop`, `--metrics`) |
| `aco keys` (xdotool) | `quiver lab key` (keydown, hold, keyup), `type` |
| `inject_mouse.py` | `quiver lab mouse DX DY` (XTEST relative motion via `xdotool mousemove_relative`); `--repeat --gap --log` runs the timed injector in the container, logging CLOCK_MONOTONIC_RAW send times (Wine's QPC base) for latency work |
| clicks at 2560×1600 pixels | `quiver lab click FX FY`: fractions of the manifest's display; `--expect-change` fails when the screen didn't change (clicks missed silently before) |
| `ini-set` | `quiver lab set FILE:[section]key=value` (INI and XML through `confpatch`, byte for byte except the value; Unreal section names with dots work; `{width}`/`{height}`/`{refresh}`), `--wait` for the reload line |
| `profiles/<p>.ini` | `quiver lab profile` (`profiles/<game>/<name>.set`, applied before every launch when it is the manifest's) |
| `fixture` / `fixture-save` | `quiver lab fixture save|install|list` (the manifest's save folders; read-only once saved; the instance's own saves kept once before the first install; refused while the game runs) |
| `where`, `teleport`, `locations.txt` | `quiver lab call|where|teleport` over the lab channel |
| `gpu-exclusive` | `quiver lab gpu [--for N] -- CMD` (a lock others queue on; other running instances throttled through their manifests' `throttle` lines and restored after; host state logged before and after to `gpu.log`) |
| `bench`, `abtest` | `quiver lab bench`: variants `LABEL=assign;assign`, `--rounds` interleaved, `--samples` shots over `--crop` (Laplacian sharpness, temporal noise, mean level), `--log-metric NAME=REGEX` numbers from the log written meanwhile, host state per variant, `--relaunch --ready STATE` for settings that need a restart, `--gpu`; originals restored; `bench.json` and a table |
| `userplaying.sh` | built into every launch, and `quiver lab host` |
| `framediff.py`, `perframe.py`, `latency.py` | not built: they read Pharos's own captures. The image metrics (`lab::image`) cover frame differences over crops; a latency analysis needs a mod that logs camera changes per frame (Pharos does) |

Long runs go in the background: `quiver lab … --bg` prints a job id; `quiver lab job wait ID`
returns with the job's output and exit code.

## 9. First live run (for the main session)

Needs the merged build installed (`tools/install-user.sh`: the lab refuses a bridge without
lab sessions) and quiverd restarted onto it (it restarts itself when idle). Then, with the user
not playing:

```sh
quiver lab doctor                                    # all ok (bridge, image, CDI, reflinks, quiverd)
quiver lab image build                               # if the image is missing (~1.4 GB)
quiver lab create ubisoft:3539 --name origins --display 1920x1080@60   # gold copy + instance (reflinks)
quiver lab up origins
quiver lab launch origins --wait frame --timeout 300
quiver sessions                                      # Origins with holder "lab"; nothing reported
quiver lab shot origins --metrics
quiver lab kill origins && quiver lab down origins
```

Check afterwards: `quiver activity` shows "issued lab launch material … session lab-origins-r1";
quiverd's log has "bridge reports game running holder=lab" and no "play session started";
no new entry in Origins' play history. Then the same for Epic: `quiver lab create "Alan Wake
Remastered" --name awr && quiver lab up awr && quiver lab launch awr --wait frame`.

## 10. Not built yet

- GOG (no provider), EA, Meta: refused with a reason.
- Steamworks titles (section 7).
- A Quiver mod profile (Mods v2 stack) as an instance's mod: today a mod is a folder copied
  over the install; the profile route (overlays, config patches) is the next step.
- The mod SDK channel for lab sessions (refused on purpose; would need a lab-scoped Options
  store so a lab mod's options never reach the user's).
- The app: instances aren't shown in Quiver's app.
- Analysis tools that read a specific mod's captures (frame dumps, per-frame draw counts).

## 11. Open questions for the user

1. Lab sessions no longer report Ubisoft play time (the Pharos containers did, as plain bridge
   sessions). Built as the coordinator asked; Pharos's own `aco` harness is unchanged until it
   moves to `quiver lab`.
2. Should the user's current saves come along in the gold copy (they do: it is a clone of the
   prefix), or should gold copies start without saves and use fixtures only?
3. The user-playing check refuses on *any* Windows program outside containers (except Wine
   tests in build folders). Too strict for other host Wine tools?
