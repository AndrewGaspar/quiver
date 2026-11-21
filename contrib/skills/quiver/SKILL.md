---
name: quiver
description: Quiver, this machine's game launcher (Ubisoft, Epic and Steam games under Proton). Use it to install, update, launch, stop, or configure a game; to manage mods, mod profiles, options, Tweaks (frame rate, DLSS, frame generation), Proton builds, saves, play time, achievements, the performance HUD or the in-game Sight panel; to work out why a game won't start; and to start modding a game in a disposable headless lab instance (`quiver lab`). Triggers: quiver, game launcher, play/launch/install a game, Proton for a game, mods, mod profile, DLSS, frame generation, HUD, Sight, Epic game, Ubisoft game, Steam game, steam-run, game saves, play time, modding a game, lab instance.
---

# Quiver

Quiver is a native Linux game launcher. `quiverd`, a user daemon, holds the store logins, the download queue, play sessions and settings. The `quiver` CLI and the `quiver-app` GUI are clients of the daemon. Ask the CLI rather than reading Quiver's files: `quiver <command> --help` is accurate for the installed build, and most commands take `--json`.

Source and design docs: https://github.com/AndrewGaspar/quiver. A checkout may be at `~/code/ubisoft-linux`, with `design/*.md`. Start with `STORES.md`, `MODS-V2.md`, `TWEAKS.md`, `SIGHT-HUD.md` and `LAB.md`.

## Game refs

Every game has a ref:
- `ubisoft:<product id>` (a bare number also works: `3539` is Assassin's Creed Origins)
- `epic:<app name>`
- `steam:<app id>`

Most commands also take a title, e.g. `quiver play "Alan Wake Remastered"`. `quiver library` lists every owned game with its ref. Add `--updates` for pending updates and `--json` for scripts.

## Common tasks

| Want | Command |
|---|---|
| What's running, signed in, queued | `quiver status`, `quiver sessions`, `quiver queue` |
| Install / update / verify / uninstall | `quiver install GAME [--add-to-steam] [--desktop-entry]`, `quiver update GAME [--preview]`, `quiver verify GAME [--repair]`, `quiver uninstall GAME` |
| Download queue order | `quiver queue move GAME up/down/top`, `quiver queue now GAME` (switches the running download for it; that one keeps its progress), `quiver queue after GAME --steam on --desktop on` (done by quiverd when the install finishes) |
| Launch / stop | `quiver play GAME [--profile P] [--detach]`, `quiver stop GAME` |
| Launch settings (Proton, env, DLLs, args, wrappers) | `quiver get GAME`, `quiver set GAME key=value` (`key=` unsets), `quiver history GAME`, `quiver restore GAME REV`; `quiver protons` lists the Proton builds |
| Profiles (named setups: mods + game settings files, Options, Tweaks, launch overrides, save store) | `quiver profiles GAME` (list), `show P`, `create NAME [--from P \| --blank] [--mods a,b] [--shared-settings]` (copies the default profile unless told otherwise; a profile needs no mods), `set P k=v`, `settings [P own\|shared]`, `default P` |
| Mods (the library profiles pick from) | `quiver mods GAME list / catalog / install … / version … / plan`; `quiver mods GAME profile …` still works |
| Tweaks (fps cap, vsync, Reflex, DLSS, frame gen) | `quiver tweaks …` (`explain`, `plan GAME` to preview) |
| HUD and in-game panel | `quiver hud status GAME`, `quiver hud enable/disable GAME` (the Vulkan layer and the panel's input preload, together: the per-game off switch, e.g. for anti-cheat), `quiver hud show/hide GAME`; `quiver sight toggle/open/close`, `quiver sight status` (`drawn:` says whether the panel is in the game, taking the game's keyboard and mouse while open, or on the desktop, and why) |
| Performance data | `quiver perf sessions`, `quiver perf watch GAME` |
| Saves, play time, achievements | `quiver saves GAME`, `quiver playtime`, `quiver achievements GAME`, `quiver progress GAME` |
| Epic specifics | `quiver epic saves GAME`, `quiver epic playtime`, `quiver epic extras GAME` |
| Steam games through Quiver | `quiver steam …`; Steam launch options `quiver steam-run %command%` |
| Desktop entry / Steam shortcut | `quiver export desktop GAME`, `quiver export steam GAME` |
| Who changed what | `quiver activity`; settings changes are versioned (`quiver history`) |
| Daemon | `quiver daemon status`; it restarts itself onto a new build once no game needs it. It runs as the systemd user unit `quiverd.service` (in `quiver.slice`; games get their own scopes there). Never `systemctl --user stop/restart` it or change lingering yourself: `quiver remote boot on` or `off` is the person's switch for starting at boot |

Logs: `journalctl --user -u quiverd` for the unit (`~/.local/share/quiver/quiverd.log` for a quiverd started outside it). A launch's game log is in its prefix, under `quiver-run/game.log`. Read these; don't edit anything under `~/.local/share/quiver` by hand.

A game Quiver launched has `QUIVER_GAME=<ref>` and `QUIVER_SESSION=<id>` in its environment. Read `/proc/<pid>/environ` to tell whether a window belongs to a Quiver game.

## Rules for agents

- **Say who you are.** Pass `--source agent:<your-name>` (or set `QUIVER_SOURCE`), so `quiver activity` and settings history show who made a change. Game-level changes (settings, mods, options, Tweaks) apply exactly as if the person made them. There is no confirmation gate, so be deliberate and say what you changed.
- **These are the person's, never yours:**
  - signing in to any store (`quiver login`, `quiver epic login`);
  - the first Epic play-time report and the first Epic cloud-save upload;
  - resolving an Epic save conflict;
  - re-sending Ubisoft achievements;
  - restarting Steam (`quiver steam restart`).

  Never read token or credential files or keyring entries.
- **Ubisoft has one live login per account, and quiverd holds it.** Never run Ubisoft Connect alongside Quiver, and never start a second quiverd against the real data dir. To run a Ubisoft game somewhere else (a container, another prefix), use `quiver context` or `quiver lab`, which borrow quiverd's login.
- **The person's play comes first.** Don't stop, restart or relaunch a game they are playing.
- Licensing stays legitimate: real ownership through Quiver, no entitlement stubs or ownership bypasses.
- Tests or scratch daemons from a Quiver checkout: set `QUIVER_SHM_ROOT` to a scratch dir and use a scratch data dir. Never take X displays `:0`–`:9`.

## Modding a game: `quiver lab`

A lab instance is a disposable headless copy of an installed game: a reflink clone of its prefix and install, in podman with gamescope headless and the GPU. It runs with the person's real licence from quiverd (Ubisoft bridge, Epic exchange codes; DRM-free games need none). Store logins never enter it, saves are never uploaded, and play time, achievements and stats are never reported. In every instance `api.epicgames.dev` (and Epic's other names) resolve to the instance's EOS proxy, which terminates TLS with a CA trusted only in the instance's cloned prefix. It forwards only sign-in, the SDK's config and ownership checks, and answers EOS cloud saves, achievements, stats and telemetry with a 403. A game that doesn't trust the lab CA just has EOS offline (fail closed). A game shipping `EOSSDK-*.dll` won't launch unless the proxy answers from inside its container. `quiver lab logs NAME` gives the verdict from the run's `eos-filter.log`: whether the proxy saw the game's TLS and requests, what it blocked, and any `!!! BYPASS`/`!!! OUTSIDE` connection that went around it (a bypass to Epic kills the run). If sign-in needs a blocked path, extend `ALLOWLIST` in `crates/quiver-core/src/lab/eos.rs` (design/LAB.md 3.1), never with anything that writes to the account.

Start here. `brief` prints everything a new agent needs for that game: store, exe, graphics API, engine hints, config and save files, anti-cheat, commands and rules.

```sh
quiver lab doctor                          # what's missing (podman, CDI, image, reflinks, bridge, EOS proxy)
quiver lab brief GAME                      # the onboarding brief for that game
quiver lab create GAME --name NAME --display 1920x1080@60
quiver lab list                            # instances and their owners (codex:pharos-xreal, claude:pharos)
quiver lab up NAME                         # records you as the owner (--owner NAME / QUIVER_LAB_OWNER to name yourself)
quiver lab launch NAME [--mod DIR] --wait frame
quiver lab shot NAME [--settle S] [--metrics]; quiver lab key NAME …; quiver lab mouse NAME DX DY
quiver lab set NAME FILE:[section]key=value --wait   # live config edit, waits for the reload line
quiver lab bench …   quiver lab gpu -- CMD   quiver lab fixture …   quiver lab logs NAME
quiver lab kill NAME && quiver lab down NAME
quiver lab edit NAME clock=virtual:62.5    # fixed-step virtual clock (clock=real: off); or launch --clock virtual:62.5
quiver lab clock NAME pause|step [K]|resume|status   # with the clock on: hold the game, let exactly K frames through
quiver lab key NAME --down Right; quiver lab key NAME --up Right   # keys held across steps
```

**Instances are shared.** Other sessions (Claude, Codex, muse, the person) run instances in the same lab. Before taking down, killing or relaunching an instance you didn't start, check its owner in `quiver lab list` (or `quiver lab status NAME`, which says whether the owner's process is still alive) and ask that session or the person. A `(gone)` owner is a hint the instance is abandoned, not permission. `quiver lab whoami` shows what you'd be recorded as.

Pause and frame stepping (virtual clock on): `quiver lab clock NAME pause` holds the game at the end of the frame in flight (that frame stays on screen; game time stops however long you take, and never times out unless you pass `--auto-resume SECS`); `quiver lab clock NAME step 12` lets exactly 12 more frames through, each exactly 1/rate s, and holds again, printing the frame number and virtual time; `resume` runs free. Each waits for the hold, so `quiver lab clock NAME step 12 && quiver lab shot NAME` shoots that frame (`shot --hdr` works while held too). Input sent while held is read when the game next steps: to hold a key over frames, `quiver lab key NAME --down Right`, `step 12`, `quiver lab key NAME --up Right`, `step 1`; a plain `key` while held puts the press and release in one frame. `quiver lab logs NAME` lists the pauses and steps.

Sight in an instance (the HUD and the in-game panel): `quiver lab create GAME --sight` (or `quiver lab edit NAME sight=on`, then down/up). Then `quiver lab key NAME shift+F2` opens the panel drawn in the game (the game keeps focus), `quiver lab key NAME Down Return Escape` drives and closes it (the game gets none of it while open), `quiver lab key NAME shift+F3` shows the HUD, and `quiver lab sight NAME status` and `quiver lab logs NAME` show where it's drawn, whether input is captured, and failsafe releases. Its quiver-sight never talks to quiverd: panel changes to settings are refused, nothing is reported.

Game controllers in an instance: `quiver lab create GAME --pads` (or `quiver lab edit NAME pads=1`, then down/up; `--pads 2` for player 2) gives the instance virtual Xbox 360 pads that only its container can open (a udev rule hides them from the host; `quiver lab doctor` says whether it's installed, and Quiver refuses to make a pad without it). Then `quiver lab pad NAME press a [--for 500ms|12f]`, `hold lb` / `release lb`, `stick left 0 1 [--for 2s]` (Y up, -1..1), `trigger rt 1`, `neutral`, `seq "a 100ms; wait 200ms; lstick=0,1 1s; +rt=1; x; neutral"`, and `--pad 2` for player 2. With the virtual clock on, durations are frames and scripts land on the same frames every run. `quiver lab pad NAME status` shows what the game sees (XInput slot, DirectInput, HID, raw input), who has the pad open in the container, and any rumble the game asked for.

HDR in an instance: `quiver lab create GAME --hdr` (or `quiver lab edit NAME display.hdr=on`, which turns Sight on too, then down/up). gamescope's headless output is forced to HDR10 PQ and the game gets gamescope's WSI layer; turn the game's own HDR setting on. `quiver lab hdr NAME` shows the swapchain's format and colour space (HDR10 PQ or scRGB?), the HDR metadata the game set (max CLL/FALL, mastering), and whether gamescope sees the game as HDR. `quiver lab shot NAME --hdr` captures the game's frame in its own format: `.raw` (exact), `.png` (16-bit PQ) and `.json` with max/average/p99 nits and the fraction above 203 and 1000 nits. Plain `quiver lab shot` is gamescope's 8-bit tonemapped screenshot (highlights clip), so use `--hdr` to judge HDR. The in-game panel and HUD can't draw on HDR swapchains yet (in an instance the panel then stays closed and the game keeps its input). One output only.

The **virtual clock** tells the game every frame took exactly 1/N s, however long it really took. Presentation is uncapped, so a game with cheap frames runs faster than real time (a small game at 800 fps is ~13x at 62.5 Hz). On a busy machine the game runs slower than real time but behaves exactly as at N Hz, so runs repeat; a speed-up below 1.0x is normal. Pick a rate with a whole-millisecond period (125, 100, 62.5, 50, 40 Hz). It hooks only the game's own imports (the exe, plus `clock.modules=Engine.dll`), through a proxy DLL the lab puts in the instance's clone. `quiver lab status NAME` shows engaged or not, frames, virtual vs real seconds and the speed-up. `quiver lab logs NAME` adds the census of steps the game read (one value means exact) and the counters: creeping frames (one thread busy-waiting on the clock, still repeatable), escapes (a frame over `clock.stuck_ms`, 1000 ms real, went to real time, so the run is NOT FULLY DETERMINISTIC), and overruns. Limits: audio drifts; timers, audio pacing and other clocks stay real; multiplayer and DRM (Denuvo) timing checks may object, so use it on DRM-free, offline, small games first. Only the presenting thread's sleeps are shortened (`clock.sleeps=all|none`). A mod's own frame limiter caps the speed-up: turn it off yourself. Uncapped presentation boots gamescope at 1000 Hz, so `down`/`up` after turning it on.

**Lab limits.** The person can limit what lab instances use (`quiver lab limits`: CPU cores, a CPU quota, memory high/max, a GPU share; `always`, and `while_playing` while one of their own games runs, by default gpu 20% and 6 cores). They're the person's: don't change them. They're enforced quietly on the instance (its cgroup, CPU affinity, and the game's frames paced to the GPU share at low Vulkan queue priority), never on the person's games. Under a limit, wall-clock frame rates and timings in an instance measure the limit, and `quiver lab launch`, `status`, `bench` and `gpu` say `THROTTLED`. So don't depend on having the GPU to yourself: test behaviour and image quality on the fixed-step virtual clock (`clock=virtual:62.5`, `quiver lab clock NAME step K`), whose results are the same throttled or not. `quiver lab status NAME` and `quiver lab host` show what's in force.

Lab rules: use `--bg` and `quiver lab job wait` for long runs instead of sleeping. Every run has its own log folder; read that run's, not an older one. Steamworks games can't run in the lab (they'd need the person's Steam client); DRM-free Steam games can, with `--exe`. Full design: `design/LAB.md` in the Quiver repo.
