# iPhone and iPad

An iPhone's storage is not a disk Reperio can read: iOS exposes no block
device, and its flash is hardware-encrypted, so nothing can be scanned or
carved from the phone itself. What a computer the phone trusts *can* get is a
**backup**. Reperio makes one (or opens one Finder made) and recovers from it.

## Setup

    brew install libimobiledevice

(Reperio looks for `idevice_id`, `ideviceinfo`, `idevicepair` and
`idevicebackup2` on the PATH, in Homebrew's folders, and in `$REPERIO_IOS_TOOLS`.)
Connect the phone, unlock it and tap **Trust**.

## In the app

**iPhone / iPad…** on the home screen (a connected phone also appears there by
itself). It shows the phone's model, iOS version, storage and battery, and
**Back up and recover** makes a backup and opens it. Backups Finder made
(`~/Library/Application Support/MobileSync/Backup`, which needs Full Disk
Access) are listed too.

Inside a backup:

- **Photos**: everything the Photos app knows of, with the ones in **Recently
  Deleted** marked. Photos listed there whose file the backup does not hold
  are shown as "not in backup": they cannot come back from this backup.
- **Messages**: every message, including ones in Recently Deleted (iOS 16+),
  with text that iOS keeps only in `attributedBody`. **Recover deleted
  messages** goes further: when a message is deleted SQLite does not erase it,
  so Reperio rebuilds whole rows (text, sender, date, direction) from the
  database's free pages, the unused space inside live pages, and old copies of
  pages in its write-ahead log. They are marked *Recovered, may be incomplete*.
  **Raw text only** lists readable text left in free pages, for what could not
  be rebuilt.
- **WhatsApp**: every chat and message WhatsApp still has, groups included, with
  photos shown inline and other media listed (**Save** copies a chat's media
  out). **Recovered messages** rebuilds messages deleted for good from the
  leftovers in `ChatStorage.sqlite`: text, sender, chat and date where they
  survive.
- **Notes**: every note with its text, including the Notes app's Recently
  Deleted, and **Recover deleted notes** for notes deleted for good (their text,
  and the title where it survives). Notes can be copied or saved as `.txt`
  files.
- **Files**: every file in the backup by its path on the phone (app
  documents, notes, voice memos, …).

Select and **Export**: photos keep their names (deleted ones go in a
`Recently Deleted` folder), files keep their phone folders.

## On the command line

    reperio ios devices
    reperio ios pair UDID
    reperio ios backup UDID ./backups
    reperio ios backups
    reperio ios photos BACKUP [--deleted]
    reperio ios messages BACKUP [--deleted] [--search TEXT] [--fragments]
    reperio ios deleted BACKUP --db sms.db        deleted rows of any database
    reperio ios whatsapp BACKUP [--chat NAME] [--deleted]
    reperio ios notes BACKUP [--deleted] [--text]
    reperio ios analyze BACKUP DEST [--plugin NAME]   plug-ins on the backup
    reperio ios export BACKUP ./out --photos [--deleted]
    reperio ios export BACKUP ./out --files report

## Deleted rows from any database

Messages are one case of a general tool: `reperio ios deleted BACKUP --db NAME`
recovers deleted rows from any SQLite database in the backup (`sms.db`,
`ChatStorage.sqlite` for WhatsApp (also its own tab), `NoteStore.sqlite` (also its own tab), `AddressBook.sqlitedb`,
`History.db` for Safari, ...). It needs no knowledge of the app: a candidate
record is accepted only if its header is well formed, it fits the page, and its
columns agree with the types of a table in that database's own schema. It
reads the free pages, the free space inside live pages, and the write-ahead
log, and leaves out rows that still exist. (`engine/src/sqlite_carve.rs`.)

Limits: a row overwritten since is gone, a row can be partly overwritten, a
record can match the wrong table of the same shape, and rows in tables with no
integer primary key have the start of their record destroyed on deletion and
are not rebuilt.

## What it cannot do

- **Encrypted backups.** Reperio has no way to read them. Turn off "Encrypt
  local backup" in Finder for that phone and back up again.
- **Anything the phone no longer holds.** A backup contains what iOS keeps:
  what was deleted long ago, or from a wiped phone, is not in it. Check iCloud
  Photos' Recently Deleted and older backups.
- **Carving a live iPhone.** There is no raw access to iOS storage without a
  full-file-system extraction (jailbreak or forensic tools).
- **iCloud.** Reperio only works with what is on this computer.

## Tests

`cargo test -p reperio-engine ios` runs the whole path (find the phone, trust
it, back it up, read the backup, export) against mock `libimobiledevice`
tools in `scripts/mock_ios/`, which write a backup in the real format. **It
has not been run against a real iPhone yet.**
