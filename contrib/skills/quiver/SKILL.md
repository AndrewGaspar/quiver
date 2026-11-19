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
| Install / update / verify / uninstall | `quiver install GAME`, `quiver update GAME [--preview]`, `quiver verify GAME [--repair]`, `quiver uninstall GAME` |
| Launch / stop | `quiver play GAME [--profile P] [--detach]`, `quiver stop GAME` |
| Launch settings (Proton, env, DLLs, args, wrappers) | `quiver get GAME`, `quiver set GAME key=value` (`key=` unsets), `quiver history GAME`, `quiver restore GAME REV`; `quiver protons` lists the Proton builds |
| Mods and profiles | `quiver mods GAME list / catalog / install … / profile … / plan` |
| Tweaks (fps cap, vsync, Reflex, DLSS, frame gen) | `quiver tweaks …` (`explain`, `plan GAME` to preview) |
| HUD and in-game panel | `quiver hud status GAME`, `quiver hud enable/disable GAME` (the Vulkan layer itself), `quiver hud show/hide GAME`; `quiver sight status` |
| Performance data | `quiver perf sessions`, `quiver perf watch GAME` |
| Saves, play time, achievements | `quiver saves GAME`, `quiver playtime`, `quiver achievements GAME`, `quiver progress GAME` |
| Epic specifics | `quiver epic saves GAME`, `quiver epic playtime`, `quiver epic extras GAME` |
| Steam games through Quiver | `quiver steam …`; Steam launch options `quiver steam-run %command%` |
| Desktop entry / Steam shortcut | `quiver export desktop GAME`, `quiver export steam GAME` |
| Who changed what | `quiver activity`; settings changes are versioned (`quiver history`) |
| Daemon | `quiver daemon status`; it restarts itself onto a new build once no game needs it |

Logs: `~/.local/share/quiver/quiverd.log`. A launch's game log is in its prefix, under `quiver-run/game.log`. Read these; don't edit anything under `~/.local/share/quiver` by hand.

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

A lab instance is a disposable headless copy of an installed game: a reflink clone of its prefix and install, in podman with gamescope headless and the GPU. It runs with the person's real licence from quiverd (Ubisoft bridge, Epic exchange codes; DRM-free games need none). Store logins never enter it, saves are never uploaded, and play time, achievements and stats are never reported.

Start here. `brief` prints everything a new agent needs for that game: store, exe, graphics API, engine hints, config and save files, anti-cheat, commands and rules.

```sh
quiver lab doctor                          # what's missing (podman, CDI, image, reflinks, bridge)
quiver lab brief GAME                      # the onboarding brief for that game
quiver lab create GAME --name NAME --display 1920x1080@60
quiver lab up NAME
quiver lab launch NAME [--mod DIR] --wait frame
quiver lab shot NAME [--settle S] [--metrics]; quiver lab key NAME …; quiver lab mouse NAME DX DY
quiver lab set NAME FILE:[section]key=value --wait   # live config edit, waits for the reload line
quiver lab bench …   quiver lab gpu -- CMD   quiver lab fixture …   quiver lab logs NAME
quiver lab kill NAME && quiver lab down NAME
```

Lab rules: use `--bg` and `quiver lab job wait` for long runs instead of sleeping. Every run has its own log folder; read that run's, not an older one. Steamworks games can't run in the lab (they'd need the person's Steam client); DRM-free Steam games can, with `--exe`. Full design: `design/LAB.md` in the Quiver repo.
