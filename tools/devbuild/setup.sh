#!/bin/bash
# One-time setup on a dev machine: route cargo in agent worktrees (.claude/worktrees/*) through
# cargo-wrapper, via a linked rustup toolchain and a rust-toolchain.toml in .claude/worktrees.
# The main checkout is untouched. Undo: rm .claude/worktrees/rust-toolchain.toml.
set -eu
repo="$(cd "$(dirname "$0")/../.." && pwd)"
stable="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
tc="$HOME/.local/share/quiver-dev/toolchain"
mkdir -p "$tc/bin"
for b in "$stable"/bin/*; do n=$(basename "$b"); [ "$n" = cargo ] || ln -sfn "$b" "$tc/bin/$n"; done
ln -sfn "$stable/lib" "$tc/lib"
ln -sfn "$repo/tools/devbuild/cargo-wrapper" "$tc/bin/cargo"
rustup toolchain link quiver-agents "$tc"
mkdir -p "$repo/.claude/worktrees"
printf '[toolchain]\nchannel = "quiver-agents"\n' > "$repo/.claude/worktrees/rust-toolchain.toml"
echo "agent worktrees now build through $repo/tools/devbuild/cargo-wrapper"
