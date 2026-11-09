# Testing on real cards

The generated test cards (`scripts/make_test_cards.py`) cover the
filesystems, but real cameras, phones and drones lay files out in ways no
script imitates. This is a protocol for testing Reperio on real cards and
comparing it with PhotoRec, the reference open-source recovery tool.

The rule throughout: **write down what is on the card before deleting
anything.** A recovery is only measurable against a list of the original
files and their checksums.

## What you need

- Spare cards you can erase: one 16 or 32 GB card (FAT32) and one 64 GB or
  larger (exFAT). A USB stick formatted by a Mac covers APFS and Mac OS
  Extended.
- Reperio built for the Mac (`cargo build --release -p reperio`) and
  PhotoRec (`brew install testdisk`).
- Enough free space for two copies of each card image.

## Devices and formats

Test with the devices people actually recover from. Each formats the card
its own way and names and stores files differently:

| Device | Filesystem | What to shoot |
| --- | --- | --- |
| Canon, Nikon, Sony or Fujifilm camera | FAT32 (32 GB and under) or exFAT | JPEG, RAW (CR3/NEF/ARW/RAF), a few videos |
| Phone with a card slot, or GoPro | exFAT | photos, 4K video, a long video (several GB, split into chapters on GoPro) |
| DJI drone | FAT32 or exFAT | photos and videos (`DJI_0001.MP4`, `.SRT` files) |
| Mac Disk Utility | APFS, Mac OS Extended (Journaled), ExFAT, MS-DOS (FAT) | copy a folder of photos and videos from the Mac |
| Windows | NTFS, exFAT | copy a folder of photos and documents |

Always format the card **in the device** before a run (or with Disk Utility
for the Mac rows), so the layout is the device's own.

## Scenarios

Run each scenario on a freshly formatted card that has been filled by the
device. Keep the image of every step; each is a test case.

1. **Delete one file** in the device (or in Finder).
2. **Delete a folder** (in Finder, then empty the Trash; or "delete all" in
   the camera).
3. **Empty the Trash** after moving files to it on a Mac: macOS keeps a
   `.Trashes` folder on the card, so this case differs from the next.
4. **Quick format** in the device (or Disk Utility "Erase" without security
   options). Nothing named survives; only the deep scan finds files.
5. **Overwrite some**: after deleting, shoot a few more photos or a short
   video, so newer files take over part of the deleted ones' space. Some
   files should then come back partly overwritten, and the tools should say
   so rather than return broken files as good.

## Recording the expected manifest

Before deleting anything, with the card mounted on the Mac:

```sh
scripts/compare_tools.py --make-manifest /Volumes/CARD > card1-before.json
```

This lists every file with its folder, size, SHA-256 and first bytes (used
to recognise partial copies). macOS's own clutter (`._*`, `.DS_Store`,
`.Spotlight-V100`, `.fseventsd`, `.Trashes`) is left out.

Then do the scenario. Edit the manifest so each entry says what happened to
the file: `"deleted": true` for the files deleted (or for every file, after a
format), `"deleted": false` for those still on the card. Files deliberately
overwritten in scenario 5 can stay in the list: a tool that returns them
damaged scores them as partial, which is the honest answer.

## Imaging the card

Never test on the card itself: image it once and test on the image, so every
tool sees exactly the same bytes and runs can be repeated.

```sh
scripts/make_card_image.sh card1-delete-folder.img
```

The script lists external disks, refuses internal and virtual ones, asks
for confirmation, unmounts the card (it never ejects, erases or writes to
it), copies it with `reperio image` (or `dd` if Reperio is not on the
`PATH`), records its SHA-256 next to the image, and tells you how to mount
the card again. Check the image's SHA-256 before each run if images are
shared.

## Comparing

```sh
scripts/compare_tools.py card1-delete-folder.img --manifest card1-before.json
```

Both tools recover everything they can from the image into a temporary
folder (Reperio with `--existing`, since PhotoRec also returns files that
were not deleted), and the script prints for each:

- **files**: files written; PhotoRec's extracted EXIF thumbnails are
  counted separately as *thumbs*;
- **exact**: expected files recovered byte for byte, and **deleted**: the
  same counting only deleted files;
- **named** and **pathed**: exact matches that kept their original name,
  and their original folder too;
- **partial**: files that start like an expected file but differ from it
  (cut short, joined to other data, or partly overwritten);
- **other**: everything else (files not in the manifest, embedded previews,
  carving noise);
- **time**, wall clock.

Add `--keep results/` to keep both tools' output for inspection, and
`--json results.json` to keep the numbers. PhotoRec runs on the whole image
with its default settings; `--photorec-options` passes other `/cmd` options,
for example `1,options,mode_ext2,wholespace,search` to select the first
partition and turn on its ext2/ext3 mode. For the test cards
(`make_test_cards.py`), the manifest is found next to the images, and
`--formatted` also compares both tools on a quick-formatted copy of each.

## Reporting

For each card, note the device, card size and brand, filesystem, scenario,
and the table. When Reperio misses a file PhotoRec finds, or returns one as
good that is not, keep the image: `reperio inspect IMAGE NAME` shows how
Reperio sees that file, and a small image that shows the problem (or the
first few hundred MB of it) is the most useful bug report.
