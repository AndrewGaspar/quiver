# Dev builds with several agents at once

Agents build in git worktrees under `.claude/worktrees/`, often several at a time, with the
vendored GPUI and lld at 1–3 GB per process. Five unrestrained builds once drove memory pressure
high enough that systemd-oomd killed the terminal (and everything in it).

`setup.sh` (once per machine) routes `cargo` in agent worktrees through `cargo-wrapper`, via a
linked rustup toolchain (`quiver-agents`) and `.claude/worktrees/rust-toolchain.toml`. The main
checkout is untouched. The wrapper:

- joins one shared jobserver, a FIFO with `QUIVER_BUILD_TOKENS` (16) tokens held by
  `jobserver-keeper` (a transient user unit, `quiver-build-jobserver`), so all agent builds
  together run at most that many compile jobs: one build alone gets all of them;
- runs each build in its own scope in `quiver-builds.slice`, with `MemoryHigh=18G` /
  `MemoryMax=22G` on the slice (`QUIVER_BUILD_MEMORY_HIGH` / `_MAX`): builds are throttled
  under pressure, and oomd picks a build, not the terminal.

Check it: `systemctl --user status quiver-builds.slice`; a build script sees
`CARGO_MAKEFLAGS=… --jobserver-auth=fifo:$XDG_RUNTIME_DIR/quiver-build-jobserver`.
Undo: `rm .claude/worktrees/rust-toolchain.toml`.
