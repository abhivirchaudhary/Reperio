# Reperio rescue USB

A USB stick that starts a small Linux straight into Reperio, for getting files
off a computer whose own system no longer starts: a Windows PC stuck in a boot
loop, a Linux machine that lost its bootloader, an Intel Mac that shows a
folder with a question mark. You boot the stick instead of the broken system,
choose the computer's internal drive, and Reperio finds the files and copies
them to a second drive.

The internal drive is only ever read. Nothing is installed on it, and nothing
is written to the stick either: the system runs from memory.

**What it cannot do**

- **Apple silicon Macs (M1 and later) cannot boot it.** They only start
  software signed by Apple, and the stick holds an x86 Linux. For those Macs
  use the Reperio app on a working Mac, or a Time Machine backup.
- **Macs with an Apple T2 chip (most 2018 to 2020 Intel Macs) can boot it,
  but Linux cannot see their internal SSD.** The drive is hidden behind the
  T2 chip. The stick still helps there for external drives and for images,
  but it will list no internal drive to recover from.
- **Secure Boot must be turned off** on PCs that have it on. The stick's boot
  loader is not signed with Microsoft's key.
- Encrypted drives (BitLocker, FileVault, LUKS) show up but cannot be read
  without unlocking them first, which this menu does not do.
- Drives in Windows Dynamic Disks, LVM or Linux software RAID are not
  assembled. Plain partitions on any internal drive are.
- The keyboard is a US layout unless you pick another (see below), and the
  screen is a plain text console.

## Building the stick's image

You need an x86_64 Linux computer (a Linux VM or container is fine), with
internet access, root, and:

```
sudo apt install curl tar xorriso mtools grub-common grub-pc-bin grub-efi-amd64-bin
```

and Rust from <https://rustup.rs>. Then, in this repository:

```bash
sudo scripts/make-rescue-usb.sh        # makes reperio-rescue.iso (a few hundred MB)
```

The script:

1. builds a static `reperio` for `x86_64-unknown-linux-musl` (it adds that
   Rust target with `rustup target add` if needed), which runs on any Linux
   with nothing installed;
2. downloads Alpine Linux's mini root file system for the release in
   `ALPINE_BRANCH` and checks it against the SHA-256 that Alpine publishes
   (set `ALPINE_SHA256` to pin your own);
3. installs the Linux kernel, `ntfs-3g`, exFAT and FAT support, `e2fsprogs`,
   `smartmontools` and the keyboard maps in it, and builds the boot ramdisk;
4. adds Reperio and the boot scripts from `scripts/rescue/`;
5. writes a hybrid ISO with GRUB for BIOS and for UEFI, plus
   `reperio-rescue.iso.sha256`.

Settings are environment variables, listed at the top of the script:
`OUT`, `ALPINE_BRANCH`, `ALPINE_MIRROR`, `ALPINE_SHA256`, `REPERIO_BIN` (use a
static build you already made), `COMPRESS=1`.

Every pushed commit also builds this image in CI (the "Rescue USB image" job)
and starts it in QEMU, once the old BIOS way and once UEFI; the finished ISO
is attached to the run as an artifact.

## Writing it to a stick

Use a USB stick of 1 GB or more. **Everything on it is erased.**

- **balenaEtcher** (Windows, Mac, Linux): *Flash from file*, choose
  `reperio-rescue.iso`, choose the stick, *Flash*.
- **Rufus** on Windows: choose the ISO, and when asked pick *DD Image mode*.
- **dd** on Linux or a Mac (be sure of the device name; `lsblk` or
  `diskutil list` shows it):

  ```bash
  sudo dd if=reperio-rescue.iso of=/dev/sdX bs=4M status=progress conv=fsync
  # Mac: diskutil unmountDisk /dev/diskN, then of=/dev/rdiskN bs=4m
  ```

The stick then holds the whole system, so the same stick can be used again for
other computers.

## Starting the broken computer from it

Plug the stick in, switch the computer on, and open its one-time boot menu:

- **PCs:** press F12, F9, F11, Esc or F8 straight after switching on (the
  start screen usually says which). Choose the entry that names your USB stick.
  If it has two entries, one with "UEFI" in the name, use that one.
  If the stick is not listed, turn off Secure Boot in the firmware setup
  (usually F2, Del or F10) and check that USB boot is enabled.
- **Intel Macs:** hold **Option (Alt)** while it starts, until the picker
  shows. Choose the orange **EFI Boot** icon. On Macs with the T2 chip,
  first open *Startup Security Utility* from a working recovery system, or
  see Apple's article on external boot, and allow booting from external media.

GRUB shows a three-entry menu for four seconds. The default is right. If the
screen stays black or garbled, restart and choose the second entry, *safe
graphics*. The third entry shows all boot messages, useful when reporting a
problem.

After half a minute the first screen shows the Reperio menu. (Alt+F2 is a
command line for experts.)

## Using the menu

Everything is numbered choices: type a number and press Enter. `b` goes back
one step and `q` quits, at any question.

1. **Which drive to recover from.** Lists the drives it found, with model
   and size, marking internal and USB ones. The stick you started from is not
   offered. Choose the broken computer's drive.
2. **What to do.** *Find and recover files*, *Make a complete copy of this
   drive*, or *Check the drive's health* (SMART, if the drive reports it).
3. **How thorough.** *Quick* reads the drive's lists of files (minutes) and
   finds files that are still there and recently deleted ones, with their
   names and folders. *Thorough* also searches every part of the drive for
   lost files and can take hours; use it after a format or a damaged file
   system. You are asked whether to include files that are not deleted (yes,
   when the system will not boot) and whether to include damaged files.
   Ctrl-C stops a search and shows what it has found so far.
4. **What to bring back.** Everything, only some kinds (Photos, Documents,
   ...), only some folders (type numbers such as `1,3` or `2-4`), or only
   deleted files.
5. **Where to save.** Lists the partitions on *other* drives, with free
   space. The drive you are recovering from is never offered, and the menu
   refuses a folder on it. The menu mounts the one you choose for writing
   (Windows NTFS and FAT, exFAT, Linux ext4) and checks that it can really be
   written to. If you plug in a USB drive now, choose *Look for drives again*.
   It never formats a drive: if yours has no usable file system, prepare it on
   another computer first. A Windows drive that was not shut down cleanly (or
   was hibernated, as with Fast Startup) opens read-only; the menu says so.
6. **Arrangement.** Original folders and names, sorted by type, or by type
   and year.
7. **Recovering.** Progress is shown. At the end the files are in
   `Reperio recovered/` on the destination, with `report.csv` listing every
   file and where it came from, and the menu unmounts the drive so it can be
   unplugged.

### A complete copy (forensic image)

*Make a complete copy of this drive* is for a drive that is failing, or for
evidence. It asks for an optional case number, your name and notes, and
whether to read the drive a second time to check the copy. It then copies the
whole drive, healthy areas first, to `Reperio images/` on the destination you
choose, computes MD5, SHA-1 and SHA-256 and writes `.report.txt` and
`.report.json` next to the image (see the main README, *Forensic imaging*).
On a FAT32 destination the image is split into 2 GB pieces. The destination
needs room for the whole drive. If the copy is stopped, running it again with
the same destination carries on.

### Other keyboard layouts

At the GRUB menu press `e` on the entry and add `reperio.keymap=de` (or `fr`,
`gb`, `es`, ...) to the line that starts with `linux`, then Ctrl-X. The names
are those of Alpine's `kbd-bkeymaps` package.

## When something goes wrong

- *No drives were found*: wait a few seconds and choose *Look for drives
  again*. Some RAID/"Intel RST" or VMD settings hide NVMe drives from Linux;
  switch the firmware's storage mode to AHCI if you can (note Windows may then
  need a repair; the recovery does not).
- *The destination could not be opened for writing*: the message quotes the
  system's reason. NTFS drives left hibernated or unclean by Windows are the
  usual cause.
- *Unreadable sectors* in the warning at the end: the drive is failing. Make
  a complete copy of it first and recover from the copy.

## What has and has not been tested

The menu (drive listing, scanning, selection, mounting ext4 and NTFS
destinations, recovering, imaging with hashes, unmounting, never offering the
source) is tested against loop devices by `scripts/menu_check.py`, and the
static musl binary has been run on a pseudo-terminal. The boot chain (GRUB for
BIOS and UEFI, kernel, ramdisk, from a USB-attached disk) is tested by
`scripts/rescue_boot_check.sh` in QEMU.

Not tested on real hardware: booting real PCs and Intel Macs (firmware
differs a great deal), a T2 Mac, Secure Boot, the on-screen look on a real
display. The Alpine part of the build (package names, `mkinitfs`, the boot
scripts running on Alpine's busybox) can only be exercised with network access
to Alpine's mirrors; CI does that on every push, but treat the first image you
build as unproven and try booting it before you need it.
