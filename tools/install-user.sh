#!/usr/bin/env bash
# Build Quiver from this checkout and install it into your home directory (no root):
#
#   ~/.local/bin/{quiver,quiverd,quiver-app,ubi}
#   ~/.local/lib/quiver/ubi-bridge.exe
#   ~/.local/lib/quiver/sight/{x86_64,i686}/libquiver_sight_layer.so  (the Sight Vulkan layer)
#   ~/.local/share/vulkan/implicit_layer.d/VK_LAYER_QUIVER_sight.{x86_64,i686}.json
#                                             (loaded only into launches with QUIVER_SIGHT=1)
#   ~/.local/share/applications/quiver.desktop, ~/.local/share/icons/hicolor/scalable/apps/quiver.svg
#   ~/.config/systemd/user/quiverd.service   (installed, not enabled)
#   ~/.local/share/doc/quiver/                (licenses)
#
# Every installed file and its hash goes in ~/.local/state/quiver/installed.json. Running it
# again upgrades: changed files are staged next to their destination and renamed into place
# (a new inode, so a running quiver-app sees the update and relaunches itself), unchanged
# files are left alone, and files the previous install had but this one doesn't are removed.
# Files not in the manifest are never overwritten, except ~/.local/bin/{quiver,quiverd,
# quiver-app,ubi} symlinks into a checkout's target/release (the old dev setup).
#
# Usage: tools/install-user.sh [--allow-dirty] [--no-daemon-check]
#        tools/install-user.sh --uninstall
#
#   --allow-dirty      build the working tree even with uncommitted changes
#   --no-daemon-check  don't look at a running quiverd afterwards (no `quiver sessions`)
#   --uninstall        remove exactly the files in the manifest
#
# Environment: QUIVER_BUILD_DIR (default ~/.cache/quiver-build) holds the cargo target dir,
# kept between runs so rebuilds are incremental.
set -euo pipefail

allow_dirty=0 daemon_check=1 uninstall=0
for arg in "$@"; do
    case $arg in
        --allow-dirty) allow_dirty=1 ;;
        --no-daemon-check) daemon_check=0 ;;
        --uninstall) uninstall=1 ;;
        -h | --help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"; exit 0 ;;
        *) echo "install-user.sh: unknown option '$arg' (see --help)" >&2; exit 2 ;;
    esac
done

: "${HOME:?HOME is not set}"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bindir=$HOME/.local/bin
libdir=$HOME/.local/lib/quiver
datadir=$HOME/.local/share
unitdir=$HOME/.config/systemd/user
docdir=$HOME/.local/share/doc/quiver
manifest=$HOME/.local/state/quiver/installed.json

# The install/uninstall bookkeeping (hashes, manifest, staged renames).
manage() {
    python3 -I - "$@" <<'PY'
import hashlib, json, os, shutil, sys, time

def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()

def load(manifest):
    try:
        with open(manifest) as f:
            return json.load(f)
    except FileNotFoundError:
        return None

def prune_dirs(paths, roots):
    """Remove directories left empty under our own roots (deepest first)."""
    dirs = set()
    for p in paths:
        d = os.path.dirname(p)
        while any(d == r or d.startswith(r + "/") for r in roots):
            dirs.add(d)
            d = os.path.dirname(d)
    for d in sorted(dirs, key=len, reverse=True):
        try:
            os.rmdir(d)
        except OSError:
            pass

def dev_symlink(path, bindir):
    """A ~/.local/bin symlink of the old dev setup: <checkout>/target/release/<same name>."""
    name = os.path.basename(path)
    if os.path.dirname(path) != bindir or name not in ("quiver", "quiverd", "quiver-app", "ubi"):
        return None
    if not os.path.islink(path):
        return None
    target = os.readlink(path)
    return target if target.endswith("/target/release/" + name) else None

def install(stage, manifest, bindir, roots, version, commit, source):
    old = load(manifest)
    old_files = {f["path"]: f["sha256"] for f in (old or {}).get("files", [])}
    new = []
    for dirpath, _, names in os.walk(stage):
        for n in names:
            src = os.path.join(dirpath, n)
            dest = "/" + os.path.relpath(src, stage)
            new.append((src, dest, sha256(src), os.stat(src).st_mode & 0o7777))
    new.sort(key=lambda f: f[1])

    # Decide everything before touching anything.
    conflicts, plan, notes = [], [], []
    for src, dest, digest, mode in new:
        if not os.path.lexists(dest):
            plan.append((src, dest, digest, mode, "added"))
            continue
        link = dev_symlink(dest, bindir)
        if link:
            notes.append(f"replaced the symlink {dest} -> {link} with the installed binary")
            plan.append((src, dest, digest, mode, "replaced symlink"))
            continue
        if os.path.islink(dest) or not os.path.isfile(dest):
            conflicts.append(dest)
            continue
        same = sha256(dest) == digest
        if dest not in old_files and not same:
            conflicts.append(dest)
        elif same:
            if (os.stat(dest).st_mode & 0o7777) != mode:
                os.chmod(dest, mode)
            plan.append((src, dest, digest, mode, None))
        else:
            plan.append((src, dest, digest, mode, "updated"))
    if conflicts:
        print("install-user.sh: these exist and weren't installed by this script; move them away first:", file=sys.stderr)
        for c in conflicts:
            print(f"  {c}", file=sys.stderr)
        sys.exit(1)

    # Stage every changed file next to its destination, then rename them all into place.
    staged = []
    try:
        for src, dest, digest, mode, change in plan:
            if change is None:
                continue
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            tmp = os.path.join(os.path.dirname(dest), f".{os.path.basename(dest)}.quiver-new.{os.getpid()}")
            shutil.copyfile(src, tmp)
            os.chmod(tmp, mode)
            with open(tmp, "rb") as f:
                os.fsync(f.fileno())
            staged.append((tmp, dest))
    except BaseException:
        for tmp, _ in staged:
            try:
                os.unlink(tmp)
            except OSError:
                pass
        raise
    for tmp, dest in staged:
        os.replace(tmp, dest)

    new_paths = {dest for _, dest, _, _, _ in plan}
    removed = []
    for path in sorted(set(old_files) - new_paths):
        if os.path.isfile(path) and not os.path.islink(path):
            os.unlink(path)
            removed.append(path)
    prune_dirs(removed, roots)

    record = {
        "version": version,
        "commit": commit,
        "source": source,
        "installed_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "files": [{"path": dest, "sha256": digest, "mode": oct(mode)} for _, dest, digest, mode, _ in plan],
    }
    os.makedirs(os.path.dirname(manifest), exist_ok=True)
    tmp = manifest + ".new"
    with open(tmp, "w") as f:
        json.dump(record, f, indent=2)
        f.write("\n")
    os.replace(tmp, manifest)

    previous = (old or {}).get("version")
    print(f"Installed Quiver {version}" + (f" (was {previous})" if previous and previous != version else ""))
    changed = [(dest, change) for _, dest, _, _, change in plan if change]
    for dest, change in changed:
        print(f"  {change:<16} {dest}")
    for path in removed:
        print(f"  {'removed':<16} {path}")
    if not changed and not removed:
        print("  no files changed")
    for n in notes:
        print(f"Note: {n}")
    # For the shell: which binaries changed.
    with open(os.environ["QUIVER_CHANGED_OUT"], "w") as f:
        f.write("\n".join(dest for dest, _ in changed))

def uninstall(manifest, roots):
    old = load(manifest)
    if old is None:
        print(f"Nothing to uninstall ({manifest} doesn't exist)")
        return
    removed = []
    for f in old.get("files", []):
        path = f["path"]
        if os.path.isfile(path) and not os.path.islink(path):
            modified = sha256(path) != f["sha256"]
            os.unlink(path)
            removed.append(path)
            print(f"  removed{' (had local changes)' if modified else ''} {path}")
        else:
            print(f"  already gone {path}")
    prune_dirs(removed, roots)
    os.unlink(manifest)
    prune_dirs([manifest], roots)
    print(f"Uninstalled Quiver {old.get('version', '?')}")

cmd = sys.argv[1]
if cmd == "install":
    stage, manifest, bindir, roots, version, commit, source = sys.argv[2:9]
    install(stage, manifest, bindir, roots.split(":"), version, commit, source)
else:
    manifest, roots = sys.argv[2:4]
    uninstall(manifest, roots.split(":"))
PY
}

# Directories that are ours alone, removed once empty.
owned_roots="$libdir:$docdir:$HOME/.local/state/quiver"

if ((uninstall)); then
    manage uninstall "$manifest" "$owned_roots"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$datadir/applications" >/dev/null 2>&1 || true
    fi
    for pid in $(pgrep -u "$(id -u)" -x quiverd 2>/dev/null); do
        case $(readlink "/proc/$pid/exe" 2>/dev/null) in
            "$bindir/quiverd" | "$bindir/quiverd (deleted)")
                echo "quiverd (pid $pid) from this install is still running; stop it when no game is running (kill $pid)." ;;
        esac
    done
    exit 0
fi

repo=$(git -C "$here/.." rev-parse --show-toplevel)
cd "$repo"
dirty=$(git status --porcelain)
if [[ -n $dirty ]] && ((!allow_dirty)); then
    echo "install-user.sh: $repo has uncommitted changes (pass --allow-dirty to build them anyway):" >&2
    echo "$dirty" | sed 's/^/  /' >&2
    exit 1
fi
commit=$(git rev-parse HEAD)
semver=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
version="$semver.r$(git rev-list --count HEAD).g$(git rev-parse --short=7 HEAD)"
[[ -n $dirty ]] && version+=".dirty"

build=${QUIVER_BUILD_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/quiver-build}
export CARGO_TARGET_DIR=$build/target
mkdir -p "$build"
echo "Building Quiver $version (target dir $CARGO_TARGET_DIR)"
cargo build --release --locked
cargo build --release --locked -p ubi-bridge --target x86_64-pc-windows-gnu
# The Sight layer for 32-bit games, when Rust's i686 target is there (rustup target add
# i686-unknown-linux-gnu; also needs 32-bit glibc, lib32-glibc on Arch).
sight_i686=0
if rustup target list --installed 2>/dev/null | grep -qx i686-unknown-linux-gnu; then
    if cargo build --release --locked -p ubi-sight-layer --lib --target i686-unknown-linux-gnu; then
        sight_i686=1
    else
        echo "Note: the 32-bit Sight layer didn't build; installing without it."
    fi
else
    echo "Note: no 32-bit Sight layer (rustup target add i686-unknown-linux-gnu to build it)."
fi

stage=$(mktemp -d "$build/stage.XXXXXX")
changed_out=$(mktemp "$build/changed.XXXXXX")
trap 'rm -rf "$stage" "$changed_out"' EXIT
SIGHT_LAYER_I686=$sight_i686 \
BINDIR=$bindir LIBDIR=$libdir DATADIR=$datadir UNITDIR=$unitdir LICENSEDIR=$docdir \
    EXEC_START='%h/.local/bin/quiverd' "$repo/packaging/layout.sh" "$CARGO_TARGET_DIR" "$stage"
QUIVER_CHANGED_OUT=$changed_out manage install "$stage" "$manifest" "$bindir" "$owned_roots" "$version" "$commit" "$repo"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$datadir/applications" >/dev/null 2>&1 || true
fi
if command -v systemctl >/dev/null 2>&1 && grep -qxF "$unitdir/quiverd.service" "$changed_out" \
    && systemctl --user is-enabled quiverd.service >/dev/null 2>&1; then
    systemctl --user daemon-reload || true
fi

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "Note: $bindir isn't on PATH; the desktop entry works regardless." ;;
esac

# Running processes: never restarted from here (a game may be running).
uid=$(id -u)
for pid in $(pgrep -u "$uid" -x quiver-app 2>/dev/null); do
    exe=$(readlink "/proc/$pid/exe" 2>/dev/null) || continue
    case $exe in
        "$bindir/quiver-app" | "$bindir/quiver-app (deleted)") ;;
        *) echo "quiver-app (pid $pid) runs from $exe, so it won't relaunch into this install; quit it and start Quiver again." ;;
    esac
done
((daemon_check)) || exit 0
sock=${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/quiver/quiverd.sock}
sock=${sock:-${TMPDIR:-/tmp}/quiver-${USER:-user}/quiverd.sock}
daemon=$(pgrep -u "$uid" -x quiverd 2>/dev/null | head -n1 || true)
if [[ -z $daemon || ! -S $sock ]]; then
    echo "quiverd isn't running; the next quiver command starts this build."
    exit 0
fi
exe=$(readlink "/proc/$daemon/exe" 2>/dev/null || echo "?")
if [[ $exe == "$bindir/quiverd" ]] && ! grep -qxF "$bindir/quiverd" "$changed_out"; then
    echo "quiverd is already running this build."
    exit 0
fi
echo "quiverd is running the previous build ($exe); restart it when no game is running: quiver daemon stop"
if running=$(timeout 10 "$bindir/quiver" sessions 2>&1); then
    if [[ $running == "nothing running" ]]; then
        echo "No game is running right now."
    else
        echo "Games running now:"
        echo "$running" | sed 's/^/  /'
    fi
else
    echo "Couldn't ask quiverd which games are running: $running"
fi
