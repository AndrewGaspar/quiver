# Installing Quiver

Two ways to install a build of this checkout. Both lay out the same files
(`packaging/layout.sh`) and both keep a cargo target dir in `~/.cache`, so a rebuild after a
merge only recompiles what changed.

| | Per-user (usual) | Arch package |
|---|---|---|
| Command | `tools/install-user.sh` | `tools/package.sh`, then `sudo pacman -U …` |
| Root | no | for the install |
| Binaries | `~/.local/bin` | `/usr/bin` |
| Bridge | `~/.local/lib/quiver/ubi-bridge.exe` | `/usr/lib/quiver/ubi-bridge.exe` |
| Desktop entry, icon | `~/.local/share/applications`, `~/.local/share/icons/hicolor` | `/usr/share/…` |
| systemd user unit (not enabled) | `~/.config/systemd/user/quiverd.service` | `/usr/lib/systemd/user/quiverd.service` |
| Licenses | `~/.local/share/doc/quiver` | `/usr/share/licenses/quiver` |
| Tracking | `~/.local/state/quiver/installed.json` | pacman |
| Build cache | `~/.cache/quiver-build` | `~/.cache/quiver-pkg` |

Use one or the other: `~/.local/bin` comes before `/usr/bin` on PATH, so a per-user install
hides the package.

Both refuse to build a checkout with uncommitted changes unless given `--allow-dirty`
(`install-user.sh` then builds the working tree; `package.sh` always packages the committed
HEAD). Building needs `cargo` with the `x86_64-pc-windows-gnu` target (rustup:
`rustup target add x86_64-pc-windows-gnu`; Arch's `rust` package has no Windows target),
`mingw-w64-gcc`, `protobuf` and `git`.

## Per-user install

```sh
tools/install-user.sh                # build and install or upgrade
tools/install-user.sh --uninstall    # remove exactly what the manifest lists
```

Changed files are staged beside their destination and renamed into place, so running
binaries are replaced atomically; unchanged files are left alone, and files an earlier
install had that this one doesn't are removed. It never overwrites a file it didn't install,
except the dev setup's `~/.local/bin/{quiver,quiverd,quiver-app,ubi}` symlinks into a
checkout's `target/release`, which it replaces and says so. At the end it prints the version,
the files that changed, and whether `quiverd` is running an older build and any game is
running (`quiver sessions`).

## Arch package

```sh
tools/package.sh                     # prints the package path and the install command
sudo pacman -U ~/.cache/quiver-pkg/pkg/quiver-git-<version>-1-x86_64.pkg.tar.zst
```

`packaging/arch/PKGBUILD` is a VCS package (`quiver-git`, provides and conflicts with
`quiver`, version `<semver>.r<commits>.g<hash>`). `package.sh` copies it to
`~/.cache/quiver-pkg` and runs `makepkg -f` there with `QUIVER_REPO` (this checkout, cloned
through `git+file://`), `QUIVER_COMMIT` (HEAD) and `QUIVER_CARGO_TARGET_DIR` set. Run on its
own, the PKGBUILD builds the default branch from GitHub. It keeps the newest three packages.

## Switching from the dev symlinks

If `~/.local/bin` has `quiver`, `quiverd`, `quiver-app` and `ubi` symlinked into the repo's
`target/release`:

- **Per-user install:** nothing to do; `install-user.sh` replaces those symlinks.
- **Arch package:** after `pacman -U`, remove them, or they keep shadowing `/usr/bin`:

  ```sh
  rm ~/.local/bin/{quiver,quiverd,quiver-app,ubi}
  ```

Then, either way:

1. **Restart the daemon** when no game is running (`quiver sessions` says what's running).
   It keeps running the old build until then:

   ```sh
   quiver daemon stop      # the next quiver command (or the app) starts the installed one
   ```

2. **Restart the app** if it was started from the repo build: it only relaunches itself when
   the file it was started from changes. From then on it follows upgrades on its own.
3. **Desktop and Steam shortcuts** exported earlier run the `quiver` they were exported from
   (the repo build). Export them again to point them at the install, e.g.
   `quiver export desktop <game>`; `quiver export list` shows what exists.

`config.json` needs no change. It used to store the bridge path of the time (the repo's
`target/x86_64-pc-windows-gnu/release/ubi-bridge.exe`); that value now reads as "find it",
so `quiverd` uses `<its dir>/../lib/quiver/ubi-bridge.exe` (installed) or, for a dev build,
the checkout's cross build. Set `bridge_exe` only to force a particular bridge.

## Upgrades

After each merge, run `tools/install-user.sh` again (or `tools/package.sh` and `pacman -U`).
Nothing restarts the daemon on its own, since a game may be running: restart it with
`quiver daemon stop` when convenient. A `quiver-app` started from the installed binary
relaunches itself onto the new build.

The systemd user unit is installed but not enabled; `quiverd` is started on demand by any
client. To run it as a service instead: `systemctl --user enable --now quiverd`.
