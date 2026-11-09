#!/bin/bash
# Makes an image of a memory card on a Mac, for testing Reperio or sharing
# a problem card. Read-only: the card is unmounted (never ejected, erased
# or written), copied sector by sector into a file, and can be mounted
# again afterwards.
#
#     scripts/make_card_image.sh [OUTPUT.img]
#
# Lists the external physical disks, asks which one, refuses internal and
# virtual disks, and asks again before unmounting. Uses `reperio image`
# when available (it skips unreadable sectors on a failing card), else dd.
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "This helper is for macOS. On Linux: sudo reperio image /dev/sdX card.img" >&2
    exit 1
fi

out="${1:-card-$(date +%Y%m%d-%H%M%S).img}"
if [[ -e "$out" ]]; then
    echo "$out already exists; choose another name." >&2
    exit 1
fi

echo "External physical disks:"
echo
diskutil list external physical
echo
read -r -p "Disk to image (for example disk4): " disk
disk="${disk#/dev/}"
disk="${disk#r}"
if [[ ! "$disk" =~ ^disk[0-9]+$ ]]; then
    echo "Give a whole disk such as disk4, not a partition such as disk4s1." >&2
    exit 1
fi

info="$(diskutil info "/dev/$disk" 2>/dev/null)" || {
    echo "/dev/$disk does not exist." >&2
    exit 1
}
field() { awk -F': *' -v k="$1" '$1 ~ "^ *"k"$" {print $2; exit}' <<<"$info"; }
internal="$(field 'Device Location')"
virtual="$(field 'Virtual')"
protocol="$(field 'Protocol')"
size_bytes="$(field 'Disk Size' | sed -E 's/.*\(([0-9]+) Bytes\).*/\1/')"
media="$(field 'Device / Media Name')"

removable="$(field 'Removable Media')"
boot_disk="$(diskutil info / | awk -F': *' '$1 ~ "^ *Part of Whole$" {print $2; exit}')"
# APFS boots from a synthesized disk; its physical store is the real drive.
boot_store="$(diskutil apfs list 2>/dev/null | awk -v d="$boot_disk" '
    /APFS Container Reference/ {c = $NF}
    /APFS Physical Store Disk/ {s = $NF}
    c == d && s != "" {sub(/s[0-9]+$/, "", s); print s; exit}')"

# Never touch the Mac's own drives, or a disk image / APFS container that
# only looks external. A Mac's built-in SD slot reports itself internal,
# so a removable Secure Digital disk is allowed too.
sd_slot=false
[[ "$protocol" == "Secure Digital" && "$removable" == "Removable" ]] && sd_slot=true
if [[ "$disk" == "$boot_disk" || "$disk" == "$boot_store" ]]; then
    echo "/dev/$disk holds the running system. Refusing." >&2
    exit 1
fi
if [[ "$virtual" == "Yes" ]] || { [[ "$internal" != "External" ]] && ! $sd_slot; }; then
    echo "/dev/$disk is not an external physical disk ($internal, virtual: ${virtual:-No}). Refusing." >&2
    exit 1
fi
if [[ -z "$size_bytes" || ! "$size_bytes" =~ ^[0-9]+$ ]]; then
    echo "Could not read the size of /dev/$disk." >&2
    exit 1
fi

free_kb="$(df -k "$(dirname "$out")" | awk 'NR==2 {print $4}')"
if (( free_kb * 1024 < size_bytes )); then
    echo "Not enough space for a $((size_bytes / 1000000)) MB image next to $out." >&2
    exit 1
fi

echo
echo "  Disk:     /dev/$disk ($media, $protocol)"
echo "  Size:     $((size_bytes / 1000000)) MB"
echo "  Image:    $out"
echo
echo "The card will be unmounted (not ejected or changed) and read in full."
read -r -p "Type yes to continue: " ok
[[ "$ok" == "yes" ]] || { echo "Nothing done."; exit 1; }

diskutil unmountDisk "/dev/$disk"

# The raw device (rdisk) is much faster than the buffered one.
raw="/dev/r$disk"
if command -v reperio >/dev/null; then
    echo "Copying with reperio image (unreadable sectors become zeros)..."
    sudo reperio image "$raw" "$out"
else
    echo "Copying with dd (Ctrl-T shows progress)..."
    # conv=noerror,sync keeps going past unreadable sectors, filling them
    # with zeros so everything after stays at the right offset.
    sudo dd if="$raw" of="$out" bs=1m conv=noerror,sync status=progress
fi
sudo chown "$(id -un)" "$out" 2>/dev/null || true

image_bytes="$(stat -f %z "$out")"
if (( image_bytes != size_bytes )); then
    echo "Warning: image is $image_bytes bytes, card is $size_bytes." >&2
fi

echo "Computing SHA-256..."
sum="$(shasum -a 256 "$out" | awk '{print $1}')"
echo "$sum  $(basename "$out")" > "$out.sha256"
{
    echo "device: /dev/$disk ($media, $protocol)"
    echo "size: $size_bytes"
    echo "sha256: $sum"
    echo "made: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    diskutil list "/dev/$disk"
} > "$out.txt"

echo
echo "Done: $out"
echo "  SHA-256 $sum (also in $out.sha256; details in $out.txt)"
echo
echo "The card is still unmounted. To use it again:"
echo "  diskutil mountDisk /dev/$disk"
echo "or unplug it and plug it back in."
echo
echo "To try recovery on the image (read-only):"
echo "  reperio scan \"$out\" --list"
