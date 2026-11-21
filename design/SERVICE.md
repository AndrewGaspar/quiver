# quiverd as a systemd user service (2026-11-21)

quiverd used to be started by whichever client found no daemon listening, as a plain child of
that client. After a reboot, `quiver status` in a kitty terminal started it, so it lived in
`app.slice/app-graphical.slice/kitty-….scope`, and so did every game and stream host it
started. systemd-oomd kills whole scopes under `app.slice` when memory is under pressure (it
happened twice in one day), and closing the terminal could take the daemon too. A headless
remote-play server also needs quiverd up after boot, before anyone signs in (REMOTE.md §3).

## 1. The unit

`contrib/quiverd.service`, installed into `~/.config/systemd/user` by `tools/install-user.sh`,
or into `/usr/lib/systemd/user` by the package. `packaging/layout.sh` fills in `ExecStart`.
It is never enabled by an install.

| | |
|---|---|
| `Type=notify` | quiverd sends `READY=1` once it listens (`unit.rs`). The notify socket is taken out of its environment, so games never inherit it, and is passed to the next build across a self-restart. |
| `ExecStart=%h/.local/bin/quiverd` | |
| `Slice=quiver.slice` | `contrib/quiver.slice`: a sibling of `app.slice`. oomd watches `app.slice`, not this slice, and terminals' scopes are elsewhere. |
| `Restart=on-failure`, `RestartSec=2`, 5 starts a minute | |
| `SuccessExitStatus=75`, `RestartPreventExitStatus=75` | 75 means another quiverd already serves the socket: not a failure, and nothing to restart. |
| `KillMode=process` | Stopping or restarting the unit stops quiverd alone. Games are in scopes of their own (§3). Anything else quiverd started (the HUD renderer, a game that couldn't get a scope) keeps running, as it did before the unit. |
| `WantedBy=default.target` | Enabled, it starts when the user manager does: at sign-in, or at boot with lingering on (§5). No `PartOf=graphical-session.target`: a server has no graphical session. |

The name stays `quiverd.service`. Install scripts, the package and people's habits use it.

**Self-restart under the unit.** The restart onto a new build (lifecycle.rs) execs in place,
so the pid systemd knows stays the main pid. The default `NotifyAccess=main` holds, and there
is no watchdog. The new build sends `READY=1` again, which does nothing. The live test checks
that `MainPID` is unchanged and the unit stays `active`.

**Logs** go to the journal: `journalctl --user -u quiverd`. A quiverd started outside the
unit (a scope or a plain child, §2) still writes `~/.local/share/quiver/quiverd.log`.

## 2. Starting it (`quiver_api::service::start_daemon`)

When nobody is listening, a client (the CLI, the app) starts quiverd, never inside its own
cgroup:

1. **The person's default daemon** (the socket is the default one, and the environment is the
   one the unit's quiverd gets: `HOME` from passwd, `XDG_RUNTIME_DIR=/run/user/<uid>`, the
   XDG base folders unset or at their defaults, no `QUIVER_*` setting):
   - the unit is installed: `systemctl --user start quiverd.service` (it waits for `READY=1`);
   - no unit file: `systemd-run --user --unit=quiverd --slice=quiver.slice` with the unit's
     properties and the caller's session variables;
   - the unit won't start (start limit hit, a broken file): a scope, as in step 2.
2. **Anyone else's daemon** (a scratch one with its own folders): `systemd-run --user --scope
   --slice=quiver.slice`, which keeps the caller's environment, so the daemon keeps its scratch
   folders and socket. `XDG_RUNTIME_DIR` is set back with `--setenv`.
3. **No systemd user manager**: a plain child in its own process group, as before.

A scratch daemon never starts, joins or moves into `quiverd.service`. Tests in
`service.rs` use a fake `systemctl` and `systemd-run` on a `PATH`.

## 3. Games and stream hosts in their own scopes

- **Launches** (`quiver_core::scope`): the bridge and the game each start as `systemd-run
  --user --scope --collect --slice=quiver.slice --unit=quiver-{bridge,game}-<game>-<tag>.scope
  -- …`. systemd-run execs in place, so the pid, the environment, the descriptors and the
  overlay namespace joined in `pre_exec` are unchanged. A 1:1-mapped user namespace reaches
  the bus fine; this was checked with `unshare --user --map-current-user --mount`. The
  wineserver and everything Wine starts stay in the first process's scope. Scopes are used
  only for the person's own runtime folder (a scratch daemon's games stay its children), only
  after a probe scope worked once, and not with `QUIVER_GAME_SCOPE=0`.
- **Stream hosts** were already in scopes (`quiver-stream-<id>`). They now go in
  `quiver.slice` too, since a scope with no slice lands in `app.slice`.
- **Steam**, when quiverd starts it, gets a scope (`steamlaunch.rs`) in the default slice. It is
  the person's client, not Quiver's.

So a game survives quiverd stopping, crashing or restarting, the terminal closing, and oomd
acting on `app.slice`. A restart onto a new build still waits until no game runs.

## 4. Moving a running quiverd into the unit

A quiverd started outside the unit can't change its own cgroup, and an exec keeps it where it
is. So the first build with this code, run outside the unit, moves itself:

1. **Decide** at startup (`unit::relocation_target`): it is outside its unit, auto-restart is
   on, it is the person's default daemon (§2), and `quiverd.service` is loaded. Then the
   lifecycle wants it to move. `Request::RemoteBoot { enabled: true }` asks for the move too.
2. **Wait until idle**, as for a restart onto a new build: no game session, launch, job, perf
   subscription, HUD, 2FA sign-in, bridge connection, hold or request in flight, and, for
   this move only, **no stream** (kept ones included). The move hands over to another
   process, which an exec doesn't. `quiver daemon status` shows "runs outside
   quiverd.service (in kitty-….scope)" and "moving into quiverd.service once nothing needs
   it".
3. **Commit** as a restart does: requests are refused with `restarting`, connections get
   `Reply::Closing` ("moving into quiverd.service"), and nothing is accepted from then on.
4. **Offer**: it binds `$XDG_RUNTIME_DIR/quiver/handover.sock` (0600, in the 0700 folder), runs
   `systemctl --user start --no-block quiverd.service`, and waits up to 20 s for a
   connection. It ignores any connection whose peer isn't our uid or whose cgroup isn't the
   unit's.
5. **Take**: the unit's quiverd, first thing in `main` (no inherited sockets, and it is
   running as the unit), connects. It receives the listening sockets (Unix socket, broker,
   remote) with `SCM_RIGHTS` and their names in a JSON line, replies `ok`, and then **waits
   for the old pid to exit** (pidfd, 15 s) before doing anything else, so two daemons never
   serve, re-adopt streams or bind Moonlight's ports at once.
6. The old one gets `ok` and exits 0. The new one carries on exactly as after an exec. It uses
   the inherited listeners, so connections that were waiting in the backlog are served and
   the broker keeps its port. It sends `READY=1`, and the activity log says "quiverd moved
   into quiverd.service (it ran in kitty-….scope)".

**If it fails** (the unit doesn't come, the start fails, no `ok`), the old daemon still holds
its sockets. It takes requests again and records that the move failed (it won't retry until
its next restart). If the new one took the sockets but the old one doesn't exit, the new one
closes them and starts as usual, finds the socket served, and exits 75.

**On the next install** (`tools/install-user.sh`, run by the main session), step by step:
1. The script installs the new binaries, `quiverd.service` and `quiver.slice`, and runs
   `systemctl --user daemon-reload` because the unit files changed. Nothing is started or
   enabled.
2. The running quiverd (the previous build, in the kitty scope) sees its binary replaced and
   restarts onto the new build once idle, by exec, still in the kitty scope.
3. The new build sees it is outside the unit and that the unit is loaded, and waits until
   nothing needs it, streams included.
4. It hands over (§4 steps 3–6). quiverd now runs as `quiverd.service` in `quiver.slice`, and
   launches from then on get their own scopes.

A game or stream never ends because of this: every step waits for idle. Until then, a game
started by the old build stays in the kitty scope it started in.

## 5. Boot

- `quiver remote enable --at-boot`, `quiver remote boot on|off` and Settings → Machines →
  **Start at boot** send `Request::RemoteBoot { enabled }`. The daemon runs `systemctl --user
  enable|disable quiverd.service`. When turning it on with lingering off, it also runs
  `loginctl --no-ask-password enable-linger <user>`. Lingering is changed only when the
  person asks this way, never in tests and never by agents. polkit's `set-self-linger`
  allows this for one's own user on most systems (on this machine, for any session). Where
  it doesn't, nothing prompts and nothing uses sudo: the reply carries `sudo loginctl
  enable-linger <user>` for the person to run, and the CLI and the toast show it. Turning
  boot off leaves lingering on (other services may need it) and says so.
- `quiver remote status` shows "starts at boot: yes (linger on)", "at sign-in only (linger
  off)", "no (linger …)", or "no: quiverd.service isn't installed" (`RemoteStatus::boot`).
- **The display.** quiverd started at boot has no `WAYLAND_DISPLAY`. Under the unit, every Proton
  launch, Steam launch and HUD renderer takes the session variables (`WAYLAND_DISPLAY`,
  `DISPLAY`, `XAUTHORITY`, `XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE`,
  `HYPRLAND_INSTANCE_SIGNATURE`, …) from `systemctl --user show-environment` as they are at
  that moment (cached 5 s). Most compositors import them when someone signs in. A streamed
  launch then sets its own display over these.

## 6. Tests

- `quiver-api` `service::tests`: cgroup parsing; the default-instance rules; the autostart
  choice; the unit, transient-service and scope paths with a fake `systemctl` and
  `systemd-run` on a `PATH` (a scratch daemon never runs `systemctl start`); the unit file's
  directives; `systemd-analyze --user verify` on the rendered unit and slice.
- `quiverd` `lifecycle::tests::moving_into_the_unit`: wanted without a staged build, held up by
  games and streams (an exec isn't held up by streams), committed like a restart, and not
  retried after failing. `unit::tests`: the decision, the handover over real sockets (a
  stranger can't take them, a failed start leaves nothing behind, the taker waits for a real
  exit), and boot on and off with a fake `systemctl` and `loginctl`, including the
  needs-root command.
- `quiverd/tests/service_live.rs` (`--ignored`): a scratch daemon outside a scratch unit
  `quiver-test-<pid>.service` (a runtime unit file, removed after) moves in. The old process
  exits 0, the same client keeps working, the new pid is the unit's `MainPID` and the unit is
  `active` (so `READY=1` arrived). A second daemon exits 75. A new build restarts in place
  under the unit with the same `MainPID`. A launch scope lands in `quiver.slice`.

## 7. Not done / notes

- An in-process Moonlight stream with no game (a desktop or test pattern) isn't counted as
  busy, by exec restarts or by the move. Its game launches are counted, as game sessions.
  The fix belongs in `moonlight.rs` (another agent's file).
- `quiver-builds.slice` (agent builds, `tools/devbuild`) is a child of `quiver.slice` by
  systemd's dash rule, so builds and games share the parent slice. Neither is under oomd.
- Settings shows the daemon log path; under the unit, the log is the journal.
