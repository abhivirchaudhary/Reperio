#!/usr/bin/env python3
"""Writes a small, format-faithful iPhone backup, for testing without a phone.

    make_fake_backup.py DIR [--encrypted]

Manifest.db (Files table), the fileID/xx/fileID layout, Info.plist and
Manifest.plist, a photo library (Photos.sqlite, with one photo in Recently
Deleted) and a message database (sms.db) with a recently deleted message, a
message whose text lives only in attributedBody, and text left in free pages.
"""
import hashlib
import os
import plistlib
import sqlite3
import sys
import datetime

APPLE = 978307200


def file_id(domain, path):
    return hashlib.sha1(f"{domain}-{path}".encode()).hexdigest()


def main(out, encrypted=False):
    os.makedirs(out, exist_ok=True)
    files = {}  # (domain, path) -> bytes

    # ---- photos
    work = os.path.join(out, "_work")
    os.makedirs(work, exist_ok=True)
    pdb = os.path.join(work, "Photos.sqlite")
    if os.path.exists(pdb):
        os.remove(pdb)
    c = sqlite3.connect(pdb)
    c.execute("""CREATE TABLE ZASSET (Z_PK INTEGER PRIMARY KEY, ZDIRECTORY TEXT, ZFILENAME TEXT, ZKIND INTEGER,
                 ZDATECREATED REAL, ZTRASHEDSTATE INTEGER, ZTRASHEDDATE REAL, ZWIDTH INTEGER, ZHEIGHT INTEGER)""")
    t = lambda y, m, d: (datetime.datetime(y, m, d, 12, 0, tzinfo=datetime.timezone.utc).timestamp() - APPLE)
    rows = [
        ("DCIM/100APPLE", "IMG_0001.JPG", 0, t(2024, 3, 1), 0, None, 4032, 3024),
        ("DCIM/100APPLE", "IMG_0002.JPG", 0, t(2024, 3, 2), 1, t(2024, 4, 20), 4032, 3024),
        ("DCIM/100APPLE", "IMG_0003.MOV", 1, t(2024, 3, 3), 0, None, 1920, 1080),
        ("DCIM/100APPLE", "IMG_0004.JPG", 0, t(2024, 3, 4), 1, t(2024, 4, 21), 4032, 3024),  # trashed, not in backup
    ]
    for i, r in enumerate(rows, 1):
        c.execute("INSERT INTO ZASSET VALUES (?,?,?,?,?,?,?,?,?)", (i, *r))
    c.commit()
    c.close()
    files[("CameraRollDomain", "Media/PhotoData/Photos.sqlite")] = open(pdb, "rb").read()
    for name, body in (("IMG_0001.JPG", b"\xff\xd8\xff\xe0 first photo \xff\xd9"),
                       ("IMG_0002.JPG", b"\xff\xd8\xff\xe0 deleted photo \xff\xd9"),
                       ("IMG_0003.MOV", b"\x00\x00\x00\x14ftypqt  video")):
        files[("CameraRollDomain", f"Media/DCIM/100APPLE/{name}")] = body

    # ---- messages
    sdb = os.path.join(work, "sms.db")
    if os.path.exists(sdb):
        os.remove(sdb)
    c = sqlite3.connect(sdb)
    c.execute("PRAGMA secure_delete=OFF")
    c.execute("CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT)")
    c.execute("CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT, attributedBody BLOB, handle_id INTEGER, date INTEGER, is_from_me INTEGER)")
    c.execute("CREATE TABLE chat_recoverable_message_join (chat_id INTEGER, message_id INTEGER, delete_date INTEGER, ck_sync_state INTEGER)")
    c.execute("INSERT INTO handle VALUES (1, '+15551230001')")
    ns = lambda y, m, d: int((datetime.datetime(y, m, d, 9, 30, tzinfo=datetime.timezone.utc).timestamp() - APPLE) * 1e9)
    body = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+" + bytes([len(b"text only in the blob")]) + b"text only in the blob" + b"\x86\x84\x02iI"
    c.execute("INSERT INTO message VALUES (1, 'see you at nine', NULL, 1, ?, 0)", (ns(2024, 4, 1),))
    c.execute("INSERT INTO message VALUES (2, 'the code is 4821', NULL, 1, ?, 1)", (ns(2024, 4, 2),))
    c.execute("INSERT INTO message VALUES (3, NULL, ?, 1, ?, 0)", (body, ns(2024, 4, 3)))
    c.execute("INSERT INTO chat_recoverable_message_join VALUES (1, 2, 0, 0)")

    # Text that was deleted and left in free pages: make a table, fill it, drop it.
    c.execute("CREATE TABLE scratch (a TEXT)")
    for n in range(60):
        c.execute("INSERT INTO scratch VALUES (?)", (f"Remember to bring the signed contract number {n} to the meeting on Friday " * 3,))
    c.commit()
    c.execute("DROP TABLE scratch")
    c.commit()
    # Messages deleted for good: still in the database's pages until overwritten.
    for n in range(40):
        c.execute("INSERT INTO message VALUES (?, ?, NULL, 1, ?, ?)",
                  (100 + n, f"Gone for good: the meeting moved to room {n} at ten", ns(2024, 3, 1) + n * 60_000_000_000, n % 2))
    c.commit()
    c.execute("DELETE FROM message WHERE ROWID >= 100")
    c.commit()
    c.close()
    files[("HomeDomain", "Library/SMS/sms.db")] = open(sdb, "rb").read()

    # ---- WhatsApp
    import gzip
    wdb = os.path.join(work, "ChatStorage.sqlite")
    if os.path.exists(wdb):
        os.remove(wdb)
    c = sqlite3.connect(wdb)
    c.execute("PRAGMA secure_delete=OFF")
    c.execute("CREATE TABLE ZWACHATSESSION (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZCONTACTJID VARCHAR, ZPARTNERNAME VARCHAR, ZLASTMESSAGEDATE TIMESTAMP, ZSESSIONTYPE INTEGER)")
    c.execute("CREATE TABLE ZWAMEDIAITEM (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, ZMESSAGE INTEGER, ZMEDIALOCALPATH VARCHAR, ZTITLE VARCHAR, ZFILESIZE INTEGER)")
    c.execute("CREATE TABLE ZWAMESSAGE (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZCHATSESSION INTEGER, ZISFROMME INTEGER, ZMEDIAITEM INTEGER, ZMESSAGETYPE INTEGER, ZMESSAGEDATE TIMESTAMP, ZFROMJID VARCHAR, ZTOJID VARCHAR, ZTEXT VARCHAR)")
    c.execute("INSERT INTO ZWACHATSESSION VALUES (1, 6, 1, '4915551234@s.whatsapp.net', 'Alex', ?, 0)", (t(2024, 4, 5),))
    c.execute("INSERT INTO ZWACHATSESSION VALUES (2, 6, 1, '120363001@g.us', 'Family', ?, 1)", (t(2024, 4, 6),))
    c.execute("INSERT INTO ZWAMEDIAITEM VALUES (1, 8, 3, 'Media/4915551234@s.whatsapp.net/a/b/PHOTO1.jpg', 'Beach', 23456)")
    live = [(1, 1, 0, None, 0, t(2024, 4, 1), "4915551234@s.whatsapp.net", None, "Are we still on for Saturday?"),
            (2, 1, 1, None, 0, t(2024, 4, 1) + 60, None, "4915551234@s.whatsapp.net", "Yes, see you at the beach"),
            (3, 1, 0, 1, 1, t(2024, 4, 2), "4915551234@s.whatsapp.net", None, ""),
            (4, 2, 0, None, 0, t(2024, 4, 6), "4915559999@s.whatsapp.net", None, "Dinner at mum's on Sunday")]
    for r in live:
        c.execute("INSERT INTO ZWAMESSAGE VALUES (?,6,1,?,?,?,?,?,?,?,?)", r)
    for n in range(40):  # messages deleted for good
        c.execute("INSERT INTO ZWAMESSAGE VALUES (?,6,1,1,?,NULL,0,?,?,?,?)",
                  (100 + n, n % 2, t(2024, 3, 1) + n * 60, None if n % 2 else "4915551234@s.whatsapp.net", "4915551234@s.whatsapp.net" if n % 2 else None,
                   f"Deleted chat: the keys are under the blue pot number {n}"))
    c.commit()
    c.execute("DELETE FROM ZWAMESSAGE WHERE Z_PK >= 100")
    c.commit()
    c.close()
    WA = "AppDomainGroup-group.net.whatsapp.WhatsApp.shared"
    files[(WA, "ChatStorage.sqlite")] = open(wdb, "rb").read()
    files[(WA, "Message/Media/4915551234@s.whatsapp.net/a/b/PHOTO1.jpg")] = b"\xff\xd8\xff\xe0 beach photo \xff\xd9"

    # ---- Notes
    def varint(n):
        out = b""
        while True:
            b = n & 0x7f
            n >>= 7
            out += bytes([b | (0x80 if n else 0)])
            if not n:
                return out

    def ld(field, data):
        return varint(field << 3 | 2) + varint(len(data)) + data

    def note_body(text):
        return gzip.compress(ld(2, ld(3, ld(1, b"\x00") + ld(2, text.encode()))))

    ndb = os.path.join(work, "NoteStore.sqlite")
    if os.path.exists(ndb):
        os.remove(ndb)
    c = sqlite3.connect(ndb)
    c.execute("PRAGMA secure_delete=OFF")
    c.execute("CREATE TABLE ZICCLOUDSYNCINGOBJECT (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, ZTITLE1 VARCHAR, ZTITLE2 VARCHAR, ZSNIPPET VARCHAR, ZMODIFICATIONDATE1 TIMESTAMP, ZCREATIONDATE1 TIMESTAMP, ZFOLDER INTEGER, ZMARKEDFORDELETION INTEGER, ZNOTEDATA INTEGER, ZFOLDERTYPE INTEGER)")
    c.execute("CREATE TABLE ZICNOTEDATA (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER, ZNOTE INTEGER, ZDATA BLOB)")
    c.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT VALUES (1, 15, NULL, 'Notes', NULL, NULL, NULL, NULL, 0, NULL, 0)")
    c.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT VALUES (2, 15, NULL, 'Recently Deleted', NULL, NULL, NULL, NULL, 0, NULL, 1)")
    notes = [("Shopping list", "Milk, eggs, bread and the good coffee", 1, 0, t(2024, 4, 10)),
             ("Passwords hint", "The hint for the safe is the street I grew up on", 1, 0, t(2024, 4, 11)),
             ("Trip ideas", "Lisbon in May, Porto for a weekend, then the train to Madrid", 1, 0, t(2024, 4, 12)),
             ("Old draft", "A draft I deleted last week and can still restore", 2, 1, t(2024, 4, 13))]
    for i, (title, body, folder, marked, when) in enumerate(notes, 10):
        c.execute("INSERT INTO ZICNOTEDATA VALUES (?, 19, 1, ?, ?)", (i, i, note_body(title + "\n" + body)))
        c.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT VALUES (?, 12, ?, NULL, ?, ?, ?, ?, ?, ?, 0)", (i, title, body[:40], when, when - 86400, folder, marked, i))
    for n in range(25):  # notes deleted for good
        i = 100 + n
        title = f"Gone note {n}"
        c.execute("INSERT INTO ZICNOTEDATA VALUES (?, 19, 1, ?, ?)", (i, i, note_body(title + "\n" + f"Remember the appointment number {n} with the dentist on Thursday morning")))
        c.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT VALUES (?, 12, ?, NULL, ?, ?, ?, 1, 0, ?, 0)", (i, title, f"Remember the appointment number {n} with the dent", t(2024, 2, 1) + n * 3600, t(2024, 2, 1), i))
    c.commit()
    c.execute("DELETE FROM ZICNOTEDATA WHERE Z_PK >= 100")
    c.execute("DELETE FROM ZICCLOUDSYNCINGOBJECT WHERE Z_PK >= 100")
    c.commit()
    c.close()
    files[("AppDomainGroup-group.com.apple.notes", "NoteStore.sqlite")] = open(ndb, "rb").read()

    # ---- other files
    files[("HomeDomain", "Library/Notes/notes.txt")] = b"a plain file in the backup"
    files[("AppDomain-com.example.app", "Documents/report.pdf")] = b"%PDF-1.4 fake"

    # ---- the manifest and the folder layout
    mdb = os.path.join(out, "Manifest.db")
    if os.path.exists(mdb):
        os.remove(mdb)
    m = sqlite3.connect(mdb)
    m.execute("CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT, flags INTEGER, file BLOB)")
    for (domain, path), data in files.items():
        fid = file_id(domain, path)
        os.makedirs(os.path.join(out, fid[:2]), exist_ok=True)
        with open(os.path.join(out, fid[:2], fid), "wb") as f:
            f.write(data)
        m.execute("INSERT INTO Files VALUES (?,?,?,1,NULL)", (fid, domain, path))
    # IMG_0004 is listed (and trashed) but its file was never backed up; a folder entry too.
    m.execute("INSERT INTO Files VALUES (?,?,?,2,NULL)", (file_id("CameraRollDomain", "Media/DCIM"), "CameraRollDomain", "Media/DCIM"))
    m.commit()
    m.close()
    for f in os.listdir(work):
        os.remove(os.path.join(work, f))
    os.rmdir(work)

    udid = os.path.basename(os.path.abspath(out))
    with open(os.path.join(out, "Info.plist"), "wb") as f:
        plistlib.dump({"Device Name": "Sam's iPhone", "Product Type": "iPhone15,2", "Product Version": "17.4",
                       "Unique Identifier": udid, "Last Backup Date": datetime.datetime(2024, 5, 1, 10, 20, 30)}, f)
    with open(os.path.join(out, "Manifest.plist"), "wb") as f:
        plistlib.dump({"IsEncrypted": encrypted, "Version": "10.0"}, f)
    with open(os.path.join(out, "Status.plist"), "wb") as f:
        plistlib.dump({"IsFullBackup": True, "SnapshotState": "finished"}, f)


if __name__ == "__main__":
    main(sys.argv[1], "--encrypted" in sys.argv)
