#!/bin/bash
# Boots a rescue image in QEMU, once the way an old PC does (BIOS) and once
# the way a newer one or an Intel Mac does (UEFI), as a USB stick, and
# checks that a marker text shows up on the serial port.
#
#   scripts/rescue_boot_check.sh reperio-rescue.iso [MARKER]
#
# The image must have been built with GRUB_SERIAL_DEBUG=1 (see
# make-rescue-usb.sh) so GRUB and the kernel talk on the serial port. The
# default marker is the first question of the menu. Needs qemu-system-x86_64
# and, for UEFI, OVMF (Debian/Ubuntu: apt install qemu-system-x86 ovmf).
# Runs without KVM, slowly.
set -uo pipefail
img=${1:?usage: rescue_boot_check.sh IMAGE [MARKER]}
marker=${2:-Which drive do you want to recover files from}
timeout_s=${BOOT_TIMEOUT:-240}

ovmf=${OVMF_CODE:-}
for f in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_CODE.fd; do
    [ -z "$ovmf" ] && [ -r "$f" ] && ovmf=$f
done
[ -n "$ovmf" ] || { echo "OVMF firmware not found (apt install ovmf), set OVMF_CODE" >&2; exit 2; }

accel=tcg
[ -w /dev/kvm ] && accel=kvm
fail=0
for mode in bios uefi; do
    log=$(mktemp)
    # A copy, so booting never changes the image under test.
    run=$(mktemp)
    cp --sparse=always "$img" "$run"
    args=(-m 1024 -accel "$accel" -display none -serial "file:$log" -no-reboot
          -drive "if=none,id=stick,format=raw,file=$run"
          -device qemu-xhci -device usb-storage,drive=stick,bootindex=0)
    [ "$mode" = uefi ] && args+=(-drive "if=pflash,format=raw,readonly=on,file=$ovmf")
    timeout "$timeout_s" qemu-system-x86_64 "${args[@]}" >/dev/null 2>&1 &
    pid=$!
    ok=0
    for ((i = 0; i < timeout_s; i++)); do
        if grep -aq -- "$marker" "$log"; then ok=1; break; fi
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    if [ "$ok" = 1 ]; then
        echo "ok: $mode boot reached \"$marker\" (${i}s, $accel)"
    else
        echo "FAIL: $mode boot never showed \"$marker\"; last output:"
        tail -n 25 "$log" | cut -c1-200
        fail=1
    fi
    rm -f "$log" "$run"
done
exit $fail
