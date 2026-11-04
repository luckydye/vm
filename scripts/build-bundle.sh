build_varmint() {
  local cargo_target_dir="${CARGO_TARGET_DIR:-$BUILD_ROOT/cargo}"
  local cargo_args=(build --release)
  local dylib

  require_commands cargo pkg-config
  for dylib in "${RUNTIME_DYLIBS[@]}"; do
    need_file "$PREFIX/lib/$dylib"
  done
  [ ! -f "$ROOT/Cargo.lock" ] || cargo_args+=(--locked)

  log "build varmint"
  (
    cd "$ROOT"
    # MACOSX_DEPLOYMENT_TARGET would also apply to proc-macro dylibs, which
    # rustc then fails to load.
    PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}" \
    RUSTFLAGS="-L native=$PREFIX/lib ${RUSTFLAGS:-}" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
      cargo "${cargo_args[@]}"
  )
  BUILT_BINARY="$cargo_target_dir/release/varmint"
}

find_vmnet_helper() {
  local prefix helper

  if [ -n "$VMNET_HELPER" ]; then
    need_file "$VMNET_HELPER"
    printf '%s\n' "$VMNET_HELPER"
    return
  fi

  need_cmd brew
  prefix="$(brew --prefix vmnet-helper 2>/dev/null)" \
    || die "vmnet-helper is not installed; run: brew tap nirs/vmnet-helper && brew trust nirs/vmnet-helper && brew install vmnet-helper"
  helper="$prefix/libexec/vmnet-helper"
  need_file "$helper"
  printf '%s\n' "$helper"
}

# DIST is an install prefix: bin/varmint and its runtime in share/varmint.
assemble() {
  log "assemble $DIST"
  local runtime="$DIST/share/varmint"
  local helper file

  for file in "$BUILT_BINARY" "$KERNEL" "$INITRD" "$BASE_IMAGE"; do
    need_file "$file"
  done
  helper="$(find_vmnet_helper)"
  need_file "$PREFIX/libexec/virgl_render_server"

  rm -rf "$DIST"
  mkdir -p \
    "$DIST/bin" \
    "$runtime/Helpers" \
    "$runtime/Frameworks" \
    "$runtime/Resources/kernel" \
    "$runtime/Resources/runtime"
  install -m 755 "$BUILT_BINARY" "$DIST/bin/varmint"
  install -m 755 "$helper" "$runtime/Helpers/vmnet-helper"
  install -m 755 "$PREFIX/libexec/virgl_render_server" "$runtime/Helpers/virgl_render_server"
  install -m 644 "$KERNEL" "$runtime/Resources/kernel/Image"
  install -m 644 "$INITRD" "$runtime/Resources/kernel/initrd"
  install -m 644 "$BASE_IMAGE" "$runtime/Resources/runtime/base.raw.zst"
}

is_system_dependency() {
  case "$1" in
    /usr/lib/*|/System/Library/*) return 0 ;;
    *) return 1 ;;
  esac
}

list_rpaths() {
  otool -l "$1" | awk '
    $1 == "cmd" && $2 == "LC_RPATH" { in_rpath = 1; next }
    in_rpath && $1 == "path" {
      line = $0
      sub(/^[[:space:]]*path[[:space:]]+/, "", line)
      sub(/[[:space:]]+\(offset [0-9]+\)$/, "", line)
      print line
      in_rpath = 0
    }
  '
}

list_dependencies() {
  otool -L "$1" | sed -nE 's/^[[:space:]]+(.+)[[:space:]]+\(compatibility version.*$/\1/p'
}

# Leaves exactly one rpath, WANTED, in a thin or universal Mach-O file.
normalize_rpath() {
  local file="$1"
  local wanted="$2"
  local archs arch thin tmp
  local rpath found

  archs="$(lipo -archs "$file" 2>/dev/null || true)"
  if [ "$(printf '%s\n' "$archs" | wc -w | tr -d ' ')" -gt 1 ]; then
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/varmint-rpath.XXXXXX")"
    for arch in $archs; do
      thin="$tmp/$(basename "$file").$arch"
      lipo "$file" -thin "$arch" -output "$thin"
      normalize_rpath "$thin" "$wanted"
    done

    # shellcheck disable=SC2086
    lipo -create "$tmp"/"$(basename "$file")".* -output "$tmp/universal"
    mv "$tmp/universal" "$file"
    rm -rf "$tmp"
    return
  fi

  found=0
  while IFS= read -r rpath; do
    [ -n "$rpath" ] || continue
    if [ "$rpath" = "$wanted" ]; then
      found=1
    else
      install_name_tool -delete_rpath "$rpath" "$file"
    fi
  done < <(list_rpaths "$file" | awk '!seen[$0]++')

  [ "$found" = 1 ] || install_name_tool -add_rpath "$wanted" "$file"
}

resolve_dependency() {
  local dependency="$1"
  local basename="${dependency##*/}"

  case "$dependency" in
    "$PREFIX"/*) printf '%s\n' "$dependency" ;;
    @rpath/*|@loader_path/*|@executable_path/*) printf '%s\n' "$PREFIX/lib/$basename" ;;
    /*)
      [ -f "$PREFIX/lib/$basename" ] \
        || die "non-system dependency is outside build prefix: $dependency"
      printf '%s\n' "$PREFIX/lib/$basename"
      ;;
    *) printf '%s\n' "$PREFIX/lib/$basename" ;;
  esac
}

copy_runtime_library() {
  local source="$1"
  local destination="$DIST/share/varmint/Frameworks/$(basename "$source")"

  need_file "$source"
  [ -f "$destination" ] && return
  cp -L "$source" "$destination"
  chmod u+w "$destination"
  MACHO_QUEUE+=("$destination")
}

# Copies FILE's non-system dependencies into Frameworks, points FILE at them
# through @rpath and gives it the single rpath RPATH.
relink() {
  local file="$1"
  local rpath="$2"
  local dependency source basename

  while IFS= read -r dependency; do
    [ -n "$dependency" ] || continue
    is_system_dependency "$dependency" && continue
    source="$(resolve_dependency "$dependency")"
    basename="$(basename "$source")"
    copy_runtime_library "$source"
    install_name_tool -change "$dependency" "@rpath/$basename" "$file"
  done < <(list_dependencies "$file")
  normalize_rpath "$file" "$rpath"
}

bundle_libraries() {
  log "bundle runtime libraries"
  local dylib stem index=0

  require_commands otool install_name_tool lipo
  MACHO_QUEUE=()
  for dylib in "${RUNTIME_DYLIBS[@]}"; do
    copy_runtime_library "$PREFIX/lib/$dylib"
  done
  relink "$DIST/bin/varmint" '@executable_path/../share/varmint/Frameworks'
  relink "$DIST/share/varmint/Helpers/virgl_render_server" '@executable_path/../Frameworks'
  while [ "$index" -lt "${#MACHO_QUEUE[@]}" ]; do
    dylib="${MACHO_QUEUE[$index]}"
    install_name_tool -id "@rpath/$(basename "$dylib")" "$dylib"
    relink "$dylib" '@loader_path'
    index=$((index + 1))
  done

  # UTM's ANGLE and epoxy load ANGLE from framework paths like EGL.framework/Versions/Current/EGL.
  for stem in EGL GLESv2; do
    mkdir -p "$DIST/share/varmint/Frameworks/$stem.framework/Versions/Current"
    ln -s "../../../lib$stem.dylib" "$DIST/share/varmint/Frameworks/$stem.framework/Versions/Current/$stem"
  done
}

sign() {
  log "sign"
  local runtime="$DIST/share/varmint"
  local dylib

  require_commands codesign
  need_file "$ENTITLEMENTS"
  need_file "$VMNET_HELPER_ENTITLEMENTS"
  for dylib in "$runtime"/Frameworks/*.dylib; do
    codesign_file "$dylib"
  done
  codesign_file "$runtime/Helpers/virgl_render_server"
  codesign --force --sign - --timestamp=none \
    --entitlements "$VMNET_HELPER_ENTITLEMENTS" "$runtime/Helpers/vmnet-helper"
  codesign --force --sign - --timestamp=none --entitlements "$ENTITLEMENTS" "$DIST/bin/varmint"
}

# Every non-system dependency must resolve inside Frameworks through the one
# allowed rpath.
verify_macho() {
  local file="$1"
  local rpath="$2"
  local frameworks="$DIST/share/varmint/Frameworks"
  local dependency

  lipo -archs "$file" | tr ' ' '\n' | grep -qx "$ARCH" || die "$file does not contain $ARCH"
  [ "$(list_rpaths "$file")" = "$rpath" ] || die "$file has rpaths other than $rpath"
  while IFS= read -r dependency; do
    [ -n "$dependency" ] || continue
    is_system_dependency "$dependency" && continue
    case "$dependency" in
      @rpath/*) need_file "$frameworks/${dependency#@rpath/}" ;;
      *) die "$file contains forbidden dependency: $dependency" ;;
    esac
  done < <(list_dependencies "$file")
  codesign --verify --strict "$file" || die "$file is not signed"
}

verify() {
  log "verify"
  local runtime="$DIST/share/varmint"
  local dylib stem

  verify_macho "$DIST/bin/varmint" '@executable_path/../share/varmint/Frameworks'
  verify_macho "$runtime/Helpers/virgl_render_server" '@executable_path/../Frameworks'
  for dylib in "$runtime"/Frameworks/*.dylib; do
    verify_macho "$dylib" '@loader_path'
    [ "$(otool -D "$dylib" | sed -n '2p')" = "@rpath/$(basename "$dylib")" ] \
      || die "$dylib has an invalid install name"
  done
  for stem in EGL GLESv2; do
    need_file "$runtime/Frameworks/$stem.framework/Versions/Current/$stem"
  done
  codesign -d --entitlements :- "$runtime/Helpers/vmnet-helper" 2>&1 \
    | grep -q '<key>com.apple.security.virtualization</key>' \
    || die "vmnet-helper is missing the Virtualization entitlement"
  codesign -d --entitlements :- "$DIST/bin/varmint" 2>&1 \
    | grep -q '<key>com.apple.security.hypervisor</key>' \
    || die "varmint is missing the Hypervisor entitlement"
}

build_bundle() {
  build_varmint
  assemble
  bundle_libraries
  sign
  verify
}
