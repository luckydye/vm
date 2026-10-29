#!/usr/bin/env bash
# Builds dist/varmint on Linux: bin/varmint with its kernel and base image in
# share/varmint, and the same as dist/varmint-linux-ARCH.tar.gz. virglrenderer,
# libx264, libopus, PulseAudio and passt come from the distribution.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$ROOT/dist/varmint"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/build/cargo}"

# shellcheck source=build-common.sh
source "$SCRIPT_DIR/build-common.sh"

KERNEL=""
INITRD=""
BASE_IMAGE=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --kernel) KERNEL="${2:?--kernel requires a path}"; shift 2 ;;
    --initrd) INITRD="${2:?--initrd requires a path}"; shift 2 ;;
    --base-image) BASE_IMAGE="${2:?--base-image requires a path}"; shift 2 ;;
    -h|--help) echo "usage: ./scripts/build-linux.sh --kernel PATH --initrd PATH --base-image PATH"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
for file in "$KERNEL" "$INITRD" "$BASE_IMAGE"; do
  need_file "$file"
done
require_commands cargo pkg-config tar
for library in virglrenderer x264 opus libpulse-simple sqlite3; do
  pkg-config --exists "$library" || die "missing development files for $library"
done

log "build varmint"
(cd "$ROOT" && CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --locked)

log "assemble $DIST"
runtime="$DIST/share/varmint"
rm -rf "$DIST"
mkdir -p "$DIST/bin" "$runtime/Resources/kernel" "$runtime/Resources/runtime"
install -m 755 "$CARGO_TARGET_DIR/release/varmint" "$DIST/bin/varmint"
install -m 644 "$KERNEL" "$runtime/Resources/kernel/Image"
install -m 644 "$INITRD" "$runtime/Resources/kernel/initrd"
install -m 644 "$BASE_IMAGE" "$runtime/Resources/runtime/base.raw.zst"

tarball="$ROOT/dist/varmint-linux-$(uname -m).tar.gz"
tar -C "$ROOT/dist" -czf "$tarball" varmint
log "done: $tarball"
