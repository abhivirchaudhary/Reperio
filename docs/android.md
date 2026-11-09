# Android phones: `reperio android`

Needs `adb` (Android platform-tools) and USB debugging on:
Settings > About phone > tap *Build number* 7 times, then Settings >
System > Developer options > *USB debugging*. Connect the cable, unlock the
phone and tap *Allow* on the prompt. Nothing is ever written to the phone.

    reperio android info                    what this phone is, and what can be done
    reperio android pull  ./phone-out       no root: trash and thumbnails
    reperio android image ./phone.img --whole --scan-to ./found   rooted: the whole storage chip
    reperio android image ./phone-parts --all                     rooted: every partition, one file each
    reperio android image ./userdata.img                          rooted: user data only

## In the app

On the home screen choose **Android phone…**. It checks for `adb` (and shows
how to install it if it is missing: on a Mac, `brew install
android-platform-tools`), waits for the phone, shows what it is and whether
it is encrypted or rooted, and offers the same two things as below:
**Bring back what the phone kept** (no root) and **Make a full image to
scan** (rooted phones). After an image, **Scan the image** opens it in the
normal scan and results screens. The app does not ship `adb` itself.

## What each one does

**`info`** shows model, Android version, whether storage is encrypted and
whether root is available, and says plainly what that means.

**`pull`** (no root) copies what Android keeps after a delete:

- `.trashed-<time>-<name>` files: Android 11 and later keep deleted photos,
  videos and files in the same folder for 30 days. They are put back under
  their own names in `Recovered from trash/`, with the date Android will
  remove the oldest one for good.
- Gallery apps' trash folders (`.Trash`, `.trash`, `.RecycleBin`).
- Thumbnail caches (`DCIM/.thumbnails`): small previews, some of deleted
  photos.
- `--everything` also copies all of the shared storage.

**`image`** (rooted phones) is the full device image. Three sizes of it:

- `--whole`: the whole storage chip (`sda`, `mmcblk0` or `nvme0n1`: the
  partition table and every partition in one file). This is the fullest
  single image, and Reperio reads its partition table when scanning.
- `--all`: every partition in its own file in the folder `dest`, with
  `MANIFEST.txt` listing each one's size, MD5 and SHA-256.
- `--partition NAME` (default `userdata`): one partition. `--list` shows
  the chip and all partitions with their sizes.

It is hashed (MD5, SHA-256) as it is written, with `<image>.hashes.txt`
beside it, and `--scan-to DIR` runs `reperio scan` on it afterwards. A phone
with several storage chips (UFS phones have `sda` to `sdg`) images the
biggest one with `--whole`; use `--all` to get the others' partitions too.

**`pull --everything`** is the nearest thing without root: a logical copy of
all of the shared storage, with `MANIFEST.sha256.txt` listing the SHA-256 of
every file. It is not a byte-for-byte image: app data and deleted files
are not reachable without root.

## What it cannot do

- **Encrypted phones.** Nearly every phone since about 2016 encrypts
  storage with keys kept in the phone's secure hardware. A raw image of such
  a phone is encrypted, and no software can find deleted files in it. `image`
  refuses to start on an encrypted phone unless `--even-if-encrypted`.
- **TRIM.** Phones tell their storage which space is free, and it is wiped
  soon after. Even without encryption, deleted data is often gone.
- **Unrooted phones** cannot be imaged. A full image of the storage chip
  needs root (or an engineering build where `adb root` works). Over plain
  USB, a phone only gives up its shared storage. Rooting a phone can erase it and may
  void its warranty, so do not do it to a phone you want to recover from.
- **f2fs.** Many phones use the f2fs file system, which Reperio cannot read
  yet. An unencrypted f2fs image is deep-scanned by file signature only
  (photos, videos, documents) without names or folders. A reader can be
  added as a plug-in (`docs/plugins.md`).
- **iPhones** are not supported.

## Tested with a fake adb

`scripts/android_check.sh` (command line) and `app/tests/gui_android.py` (the app's screen) run against a fake `adb` (`scripts/mock_adb.py`): the trash is put
back under the original names, nothing on the "phone" changes, an encrypted
or unrooted phone is refused, and an image of a rooted unencrypted phone is
byte for byte identical with the right hashes. It has not been tried on a
real phone yet. Please report how it behaves on yours.
