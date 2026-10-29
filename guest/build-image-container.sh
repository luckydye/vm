#!/usr/bin/env bash
set -euo pipefail

SOURCE_IMAGE="${1:-}"
OUTPUT_DIR=/output
SCRIPT_DIR=/opt/varmint
PUBLISH_DIR=""

[ -n "$SOURCE_IMAGE" ] || { echo "error: source image path is required" >&2; exit 1; }
[ -f "$SOURCE_IMAGE" ] || { echo "error: missing source image: $SOURCE_IMAGE" >&2; exit 1; }

work_dir="$(mktemp -d /tmp/varmint-guest.XXXXXX)"
trap 'rm -rf "$work_dir"; [ -z "$PUBLISH_DIR" ] || rm -rf "$PUBLISH_DIR"' EXIT
work_image="$work_dir/varmint-debian.raw"
stage="$work_dir/stage"
mkdir -p "$OUTPUT_DIR" "$stage"

qemu-img convert -p -O raw "$SOURCE_IMAGE" "$work_dir/source.raw"
# The cloud image's 3 GB root cannot hold the desktop.
truncate -s 8G "$work_image"
virt-resize --expand /dev/sda1 "$work_dir/source.raw" "$work_image"
rm -f "$work_dir/source.raw"

virt-customize --network -a "$work_image" \
    --copy-in "$SCRIPT_DIR/fex-install:/tmp" \
    --copy-in "$SCRIPT_DIR/hyprland-debs:/tmp" \
    --copy-in "$SCRIPT_DIR/rootfs:/tmp" \
    --run-command 'cp -a /tmp/fex-install/. / && cp -a /tmp/rootfs/. / && rm -rf /tmp/fex-install /tmp/rootfs' \
    --run-command '/usr/local/libexec/varmint-provision' \
    --run-command 'missing=""; for command in Hyprland start-hyprland Xwayland cc chromium curl e2fsck flock foot fsck git greetd growpart mise newgidmap newuidmap pasta podman podman-compose resize2fs pkg-config sgdisk sha256sum sshd stdbuf sudo glxinfo pactl pulseaudio python3 udevadm unsquashfs update-initramfs vulkaninfo wl-copy wl-paste; do command -v "$command" >/dev/null 2>&1 || missing="$missing $command"; done; [ -z "$missing" ] || { echo "missing required guest commands:$missing" >&2; exit 1; }' \
    --run-command 'for initrd in /boot/initrd.img-*; do listing="$(lsinitramfs "$initrd")"; printf "%s\n" "$listing" | grep -Eq "(^|/)sbin/fsck$" && printf "%s\n" "$listing" | grep -Eq "(^|/)sbin/e2fsck$" && printf "%s\n" "$listing" | grep -Eq "(^|/)sbin/fsck.ext4$" || { echo "initramfs is missing ext4 fsck tools: $initrd" >&2; exit 1; }; done' \
    --run-command '[ "$(dpkg --print-architecture)" != arm64 ] || { command -v FEXBash >/dev/null && test -f /usr/share/fex-emu/GuestThunks/libvulkan-guest.so && test -f /usr/lib/aarch64-linux-gnu/fex-emu/HostThunks/libvulkan-host.so && test -f /usr/lib/aarch64-linux-gnu/fex-emu/HostThunks_32/libvulkan-host.so; }' \
    --run-command '[ "$(dpkg --print-architecture)" != arm64 ] || { test -x /usr/local/bin/FEX && test "$(readlink /usr/local/bin/FEXBash)" = FEX && test "$(readlink /usr/local/bin/FEXRootFSFetcher)" = FEX && test ! -e /home/varmint/.local/share/fex-emu/RootFS; }' \
    --run-command '[ "$(dpkg --print-architecture)" != amd64 ] || for initrd in /boot/initrd.img-*; do lsinitramfs "$initrd" | grep -q "/virtio_mmio.ko" || { echo "initramfs is missing virtio_mmio: $initrd" >&2; exit 1; }; done' \
    --run-command 'test -f /home/varmint/.config/hypr/hyprland.conf' \
    --run-command 'grep -q "^varmint:" /etc/subuid && grep -q "^varmint:" /etc/subgid' \
    --run-command 'systemctl enable varmint-grow-root.service' \
    --run-command 'passwd -l root' \
    --run-command 'printf "varmint\n" > /etc/hostname' \
    --run-command 'truncate -s 0 /etc/machine-id' \
    --run-command 'rm -f /var/lib/dbus/machine-id /etc/ssh/ssh_host_* /usr/local/libexec/varmint-provision' \
    --run-command 'rm -rf /var/lib/cloud/* /etc/systemd/system/getty@tty1.service.d /etc/systemd/system/serial-getty@*.service.d' \
    --run-command 'apt-get clean && rm -rf /var/lib/apt/lists/* /var/log/*.log /var/log/journal/*'

if virt-cat -a "$work_image" /etc/passwd | awk -F: '$3 >= 1000 && $3 < 65534 && $1 != "varmint" { found = 1 } END { exit !found }'; then
    echo "error: image contains a regular user besides varmint; expected a clean nocloud image" >&2
    exit 1
fi

mapfile -t roots < <(guestfish --ro -a "$work_image" <<'EOF_GUESTFISH'
run
inspect-os
EOF_GUESTFISH
)
[ "${#roots[@]}" -eq 1 ] || { echo "error: expected one root filesystem, found ${#roots[@]}" >&2; exit 1; }
root_device="${roots[0]}"
root_type="$(guestfish --ro -a "$work_image" <<EOF_GUESTFISH
run
vfs-type $root_device
EOF_GUESTFISH
)"
[ "$root_type" = ext4 ] || { echo "error: expected ext4 root at $root_device, found: $root_type" >&2; exit 1; }

guestfish --rw -a "$work_image" <<EOF_GUESTFISH
run
set-label $root_device varmint-root
EOF_GUESTFISH

boot_files="$(virt-ls -a "$work_image" /boot)"
kernel_name="$(printf '%s\n' "$boot_files" | grep '^vmlinuz-' | sort -V | tail -n 1)"
[ -n "$kernel_name" ] || { echo "error: no kernel found in /boot" >&2; exit 1; }
kernel_version="${kernel_name#vmlinuz-}"
initrd_name="initrd.img-$kernel_version"
printf '%s\n' "$boot_files" | grep -Fx "$initrd_name" >/dev/null \
    || { echo "error: missing matching initrd in /boot: $initrd_name" >&2; exit 1; }

virt-copy-out -a "$work_image" "/boot/$kernel_name" "$stage"
virt-copy-out -a "$work_image" "/boot/$initrd_name" "$stage"

PUBLISH_DIR="$(mktemp -d "$OUTPUT_DIR/.partial.XXXXXX")"
install -m 0644 "$stage/$kernel_name" "$PUBLISH_DIR/Image"
install -m 0644 "$stage/$initrd_name" "$PUBLISH_DIR/initrd"

zstd -T0 -19 -f "$work_image" -o "$PUBLISH_DIR/varmint-debian.raw.zst"
sha256sum \
    "$PUBLISH_DIR/Image" \
    "$PUBLISH_DIR/initrd" \
    "$PUBLISH_DIR/varmint-debian.raw.zst" \
    | sed "s#$PUBLISH_DIR/##" \
    > "$PUBLISH_DIR/SHA256SUMS"
printf 'url=%s\nsha512=%s\n' "${SOURCE_URL:-unknown}" "${SOURCE_SHA512:-unknown}" > "$PUBLISH_DIR/SOURCE"

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
    chown "$HOST_UID:$HOST_GID" \
        "$PUBLISH_DIR/Image" \
        "$PUBLISH_DIR/initrd" \
        "$PUBLISH_DIR/varmint-debian.raw.zst" \
        "$PUBLISH_DIR/SHA256SUMS" \
        "$PUBLISH_DIR/SOURCE" 2>/dev/null || true
fi

for artifact in Image initrd varmint-debian.raw.zst SHA256SUMS SOURCE; do
    mv -f "$PUBLISH_DIR/$artifact" "$OUTPUT_DIR/$artifact"
done

printf '\nGuest runtime ready in %s\n' "$OUTPUT_DIR"
