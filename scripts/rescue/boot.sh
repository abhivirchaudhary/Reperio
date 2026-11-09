#!/bin/sh
# Brings the rescue system up. The USB's own file system is read-only, so
# everything that is written lives in memory and nothing is left on the
# stick (or on any internal drive).
export PATH=/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts 2>/dev/null
mount -t tmpfs -o mode=1777 tmpfs /tmp
mount -t tmpfs -o mode=755 tmpfs /run
mount -t tmpfs -o mode=755 tmpfs /var
mount -t tmpfs -o mode=755 tmpfs /mnt
mkdir -p /var/log /var/tmp /run/reperio /mnt/reperio
hostname reperio

# Load a driver for every device the firmware found: disk controllers,
# NVMe, USB. There is no udev, so this is done by hand with each device's
# modalias.
find /sys/devices -name modalias -type f 2>/dev/null -exec cat {} + |
    sort -u | xargs modprobe -b -a 2>/dev/null
# File systems and character sets for the drives people save onto, and for
# USB storage plugged in later.
modprobe -a usb_storage uas sd_mod vfat exfat ntfs3 ext4 \
    nls_cp437 nls_iso8859_1 nls_utf8 fuse 2>/dev/null
# Drivers for devices plugged in later (a USB drive to save onto).
echo /sbin/mdev >/proc/sys/kernel/hotplug 2>/dev/null
mdev -s 2>/dev/null

# The stick we started from, so the menu never offers it as a drive to
# recover from or to save onto.
dev=$(findfs LABEL=REPERIO 2>/dev/null) && echo "$dev" >/run/reperio/rescue-dev

# Options on the kernel command line: reperio.keymap=de, reperio.serial=ttyS0
for opt in $(cat /proc/cmdline); do
    case "$opt" in
    reperio.keymap=*)
        k="${opt#reperio.keymap=}"
        zcat "/usr/share/bkeymaps/$k/$k.bmap.gz" 2>/dev/null | loadkmap
        ;;
    reperio.serial=*)
        tty="/dev/${opt#reperio.serial=}"
        setsid sh -c "exec /etc/reperio/session.sh <$tty >$tty 2>&1" &
        ;;
    esac
done
