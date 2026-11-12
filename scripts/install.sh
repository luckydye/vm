#!/usr/bin/env bash
# Installs vm as PREFIX/bin/vm with its runtime (helpers, libraries,
# kernel and base image) in PREFIX/share/vm, on macOS and Linux.
# usage: scripts/install.sh [PREFIX]   (default: ~/.local)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${1:-$HOME/.local}"
DIST="$ROOT/dist/vm"
SHARE="$PREFIX/share/vm"
BIN="$PREFIX/bin/vm"

[ -x "$DIST/bin/vm" ] || { echo "error: $DIST is missing; run: make bundle" >&2; exit 1; }

staging="$PREFIX/share/.vm.partial.$$"
binary="$PREFIX/bin/.vm.partial.$$"
trap 'rm -rf "$staging" "$binary"' EXIT
mkdir -p "$PREFIX/share" "$PREFIX/bin"

# Clones on APFS and reflink filesystems, so the base image costs no extra space or time.
case "$(uname -s)" in
  Darwin) clone=(-c) ;;
  *) clone=(--reflink=auto) ;;
esac
cp -R "${clone[@]}" "$DIST/share/vm" "$staging"
cp "${clone[@]}" "$DIST/bin/vm" "$binary"

rm -rf "$SHARE"
mv "$staging" "$SHARE"
mv "$binary" "$BIN"
echo "installed $BIN"
