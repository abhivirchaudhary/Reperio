#!/bin/bash
# Builds the Reperio rescue USB image: a small Linux that starts straight
# into `reperio menu`, for PCs and Intel Macs whose own system no longer
# starts. Write the result to a USB stick with balenaEtcher or dd; see
# docs/rescue-usb.md.
#
#   sudo scripts/make-rescue-usb.sh            # makes reperio-rescue.iso
#
# Run it on an x86_64 Linux computer (a Linux VM, or a container, will do).
# It needs root (to set up the system in a chroot), internet access and:
#   curl tar xorriso mtools grub-mkrescue, and GRUB's BIOS and UEFI files
#   (Debian/Ubuntu: apt install curl tar xorriso mtools grub-common
#    grub-pc-bin grub-efi-amd64-bin)
# and Rust (https://rustup.rs) to build the program, unless REPERIO_BIN
# points to a finished static build.
#
# What it does, in order:
#   1. builds a static `reperio` for x86_64-unknown-linux-musl (it runs on
#      any Linux, with nothing installed);
#   2. downloads Alpine Linux's mini root file system and checks its
#      SHA-256 against the checksum Alpine publishes;
#   3. in that root, installs the kernel, the tools for mounting Windows,
#      Mac and Linux drives (ntfs-3g, exfat, FAT, ext4) and smartmontools,
#      builds the boot ramdisk, and adds Reperio and the boot scripts from
#      scripts/rescue/;
#   4. writes a hybrid ISO that boots from a USB stick on BIOS and UEFI
#      computers and Intel Macs. The system is read straight from it, so
#      nothing is installed on the stick and nothing is written to it.
#
# Settings (environment variables):
#   OUT=reperio-rescue.iso   the image to write
#   ALPINE_BRANCH=v3.22      which Alpine release to use
#   ALPINE_MIRROR=https://dl-cdn.alpinelinux.org/alpine
#   ALPINE_SHA256=...        expected checksum of the mini root file system
#                            (default: the one in the release's
#                            latest-releases.yaml)
#   REPERIO_BIN=path         use this static reperio instead of building one
#   ROOTFS_DIR=path          use this ready root file system (with
#                            /boot/vmlinuz-lts and /boot/initramfs-lts)
#                            instead of downloading Alpine; for testing
#   COMPRESS=1               also write OUT.gz
#   GRUB_SERIAL_DEBUG=1      make the image talk on the serial port too, for
#                            watching a virtual machine boot it (see
#                            scripts/rescue_boot_check.sh); not for sticks
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

OUT=${OUT:-reperio-rescue.iso}
ALPINE_BRANCH=${ALPINE_BRANCH:-v3.22}
ALPINE_MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
TARGET=x86_64-unknown-linux-musl

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "this builds a Linux image and must run on Linux (use a Linux VM or a --privileged container on other systems)"
[ "$(uname -m)" = x86_64 ] || die "run this on an x86_64 computer: the image's tools are built for it"
if [ "$(id -u)" -ne 0 ]; then
    exec sudo -E "$0" "$@"
fi

MKRESCUE=$(command -v grub-mkrescue || command -v grub2-mkrescue || true)
missing=()
for t in curl tar xorriso mformat; do
    command -v "$t" >/dev/null || missing+=("$t")
done
[ -n "$MKRESCUE" ] || missing+=("grub-mkrescue")
[ ${#missing[@]} -eq 0 ] || die "missing tools: ${missing[*]} (Debian/Ubuntu: apt install curl tar xorriso mtools grub-common grub-pc-bin grub-efi-amd64-bin)"
for d in i386-pc x86_64-efi; do
    [ -d "/usr/lib/grub/$d" ] || [ -d "/usr/lib/grub2/$d" ] ||
        die "GRUB's $d files are missing (Debian/Ubuntu: apt install grub-pc-bin grub-efi-amd64-bin)"
done

WORK=$(mktemp -d /var/tmp/reperio-rescue.XXXXXX)
ROOT="$WORK/rootfs"
cleanup() {
    set +e
    for m in "$ROOT/proc" "$ROOT/sys" "$ROOT/dev"; do
        mountpoint -q "$m" && umount -R "$m"
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- 1. The program -------------------------------------------------------
# Runs a command as the person who ran sudo, in a login shell (so their
# cargo and rustup are found), or as ourselves when not run through sudo.
as_builder() {
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        sudo -u "$SUDO_USER" -i sh -c 'cd "$1" && shift && exec "$@"' sh "$REPO" "$@"
    else
        (cd "$REPO" && "$@")
    fi
}

build_reperio() {
    if [ -n "${REPERIO_BIN:-}" ]; then
        [ -x "$REPERIO_BIN" ] || die "REPERIO_BIN=$REPERIO_BIN is not an executable file"
        BIN=$REPERIO_BIN
        return
    fi
    say "Building a static reperio ($TARGET)"
    as_builder sh -c 'command -v cargo' >/dev/null ||
        die "Rust is not installed (https://rustup.rs); install it, or set REPERIO_BIN to a static build"
    # The musl standard library; harmless if already there.
    as_builder sh -c 'command -v rustup >/dev/null && rustup target add "$1"' sh "$TARGET"
    as_builder cargo build --release -p reperio --target "$TARGET"
    BIN="$REPO/target/$TARGET/release/reperio"
    [ -x "$BIN" ] || die "the build did not produce $BIN"
    if file "$BIN" 2>/dev/null | grep -qE 'dynamically linked'; then
        die "$BIN is not static; the target $TARGET needs the musl Rust standard library"
    fi
}

# --- 2 and 3. The root file system ---------------------------------------
# The mini root file system's name in Alpine's latest-releases.yaml.
alpine_field() {
    awk '$1 ~ /^(file|iso):$/ && $2 ~ /^alpine-minirootfs-.*-x86_64\.tar\.gz$/ { print $2; exit }' "$1"
}

# Its SHA-256: the first one listed after its name.
alpine_sha256() {
    awk -v f="$2" '$2 == f { found = 1 } found && $1 == "sha256:" { print $2; exit }' "$1"
}

fetch_alpine() {
    say "Downloading Alpine Linux ($ALPINE_BRANCH)"
    local base="$ALPINE_MIRROR/$ALPINE_BRANCH/releases/x86_64"
    curl -fsSL "$base/latest-releases.yaml" -o "$WORK/releases.yaml" ||
        die "could not download $base/latest-releases.yaml (is the network up? set ALPINE_MIRROR to another mirror)"
    # The yaml lists, for each file, its name and checksums.
    local file sum
    file=$(alpine_field "$WORK/releases.yaml")
    [ -n "$file" ] || die "no mini root file system in $base/latest-releases.yaml"
    sum=${ALPINE_SHA256:-$(alpine_sha256 "$WORK/releases.yaml" "$file")}
    # Older mirrors also publish FILE.sha256 next to the file.
    [ -n "$sum" ] || sum=$(curl -fsSL "$base/$file.sha256" | awk '{ print $1; exit }')
    [ -n "$sum" ] || die "no SHA-256 for $file; set ALPINE_SHA256"
    curl -fSL "$base/$file" -o "$WORK/$file"
    echo "$sum  $WORK/$file" | sha256sum -c - || die "$file does not match its published SHA-256"
    ROOTFS_TAR="$WORK/$file"
}

provision_alpine() {
    say "Installing the system into the root file system"
    mkdir -p "$ROOT"
    tar -xzf "$ROOTFS_TAR" -C "$ROOT"
    cp /etc/resolv.conf "$ROOT/etc/resolv.conf"
    printf '%s\n' "$ALPINE_MIRROR/$ALPINE_BRANCH/main" "$ALPINE_MIRROR/$ALPINE_BRANCH/community" \
        >"$ROOT/etc/apk/repositories"
    mount --bind /proc "$ROOT/proc"
    mount --bind /sys "$ROOT/sys"
    mount --bind /dev "$ROOT/dev"
    # linux-lts: kernel and its drivers (including exfat, ntfs3, usb
    # storage, NVMe, SATA). util-linux: a mount that knows every file
    # system. kbd-bkeymaps: keyboard layouts. No firmware package: drives and a
    # text console need none.
    chroot "$ROOT" apk add --no-cache \
        linux-lts mkinitfs kmod util-linux e2fsprogs dosfstools \
        exfatprogs ntfs-3g ntfs-3g-progs smartmontools kbd-bkeymaps
    # The ramdisk must be able to find the stick by its label: USB and
    # disk drivers and the ISO file system, and nothing else.
    mkdir -p "$ROOT/etc/mkinitfs"
    echo 'features="base ata scsi usb cdrom nvme mmc virtio"' >"$ROOT/etc/mkinitfs/mkinitfs.conf"
    local kver
    kver=$(ls "$ROOT/lib/modules" | head -n1)
    [ -n "$kver" ] || die "no kernel modules were installed"
    chroot "$ROOT" mkinitfs -o /boot/initramfs-lts "$kver"
    umount "$ROOT/dev" "$ROOT/sys" "$ROOT/proc"
    # Smaller image: package caches, docs, header files, firmware-less.
    rm -rf "$ROOT"/var/cache/apk/* "$ROOT"/usr/share/man "$ROOT"/usr/share/doc \
        "$ROOT"/usr/include "$ROOT"/etc/resolv.conf
}

install_overlay() {
    say "Adding Reperio and the boot scripts"
    install -D -m 0755 "$BIN" "$ROOT/usr/local/bin/reperio"
    install -d "$ROOT/etc/reperio"
    install -m 0755 "$REPO"/scripts/rescue/boot.sh "$REPO"/scripts/rescue/session.sh \
        "$REPO"/scripts/rescue/shutdown.sh "$ROOT/etc/reperio/"
    install -m 0644 "$REPO/scripts/rescue/inittab" "$ROOT/etc/inittab"
    install -m 0644 "$REPO/scripts/rescue/mdev.conf" "$ROOT/etc/mdev.conf"
    echo reperio >"$ROOT/etc/hostname"
    # No password: the only account is the one running the menu on a
    # console that is physically in front of the person.
    mkdir -p "$ROOT/mnt" "$ROOT/run" "$ROOT/tmp" "$ROOT/var"
    [ -e "$ROOT/boot/vmlinuz-lts" ] || die "the root file system has no /boot/vmlinuz-lts"
    [ -e "$ROOT/boot/initramfs-lts" ] || die "the root file system has no /boot/initramfs-lts"
}

# --- 4. The bootable image -------------------------------------------------
# A hybrid ISO: it boots from a USB stick on old BIOS PCs and on UEFI PCs
# and Intel Macs alike, whether it is written with balenaEtcher or dd. The
# system is read straight from it (read-only), so nothing needs to be
# installed or partitioned, and nothing is ever written to the stick.
make_image() {
    say "Writing $OUT"
    mkdir -p "$ROOT/boot/grub"
    install -m 0644 "$REPO/scripts/rescue/grub.cfg" "$ROOT/boot/grub/grub.cfg"
    [ "${GRUB_SERIAL_DEBUG:-0}" = 1 ] && touch "$ROOT/boot/grub/serial-debug"
    rm -f "$OUT"
    # grub-mkrescue builds the BIOS (El Torito + MBR) and UEFI (appended
    # EFI system partition) boot records around the file system. The
    # volume label REPERIO is how the boot finds the stick. Rock Ridge
    # (xorriso's default) keeps permissions and symbolic links.
    "$MKRESCUE" -o "$OUT" "$ROOT" -- -volid REPERIO >"$WORK/mkrescue.log" 2>&1 ||
        { cat "$WORK/mkrescue.log" >&2; die "grub-mkrescue failed"; }
}

build_reperio
if [ -n "${ROOTFS_DIR:-}" ]; then
    [ -d "$ROOTFS_DIR" ] || die "ROOTFS_DIR=$ROOTFS_DIR is not a folder"
    say "Using the root file system in $ROOTFS_DIR"
    cp -a "$ROOTFS_DIR" "$ROOT"
else
    fetch_alpine
    provision_alpine
fi
install_overlay
make_image

(cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" | tee "$(basename "$OUT").sha256")
if [ "${COMPRESS:-0}" = 1 ]; then
    gzip -kf "$OUT"
    echo "Also wrote $OUT.gz"
fi
[ -n "${SUDO_UID:-}" ] && chown "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$OUT" "$OUT.sha256" 2>/dev/null || true
cat <<EOF

Done: $OUT

Write it to a USB stick (8 GB or more; everything on the stick is erased):
  balenaEtcher: choose $OUT, choose the stick, Flash.
  or on Linux/Mac:  sudo dd if=$OUT of=/dev/sdX bs=4M status=progress conv=fsync
See docs/rescue-usb.md.
EOF
