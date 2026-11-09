#!/usr/bin/env python3
"""Damages a document for scripts/repair_check.sh.

    repair_damage.py SOURCE KIND OUTPUT

KIND:
  nodir   ZIP: cut off at its central directory (the index is lost)
  cut     ZIP: cut in the middle of the main part's data
  xref    PDF: cross-reference table and startxref overwritten
  cut     PDF: cut at 60% (same name as the ZIP one; chosen by file type)
  zero    OLE (.doc): the first 512 bytes zeroed
"""
import sys
import zipfile

src, kind, out = sys.argv[1:4]
d = open(src, "rb").read()

if kind == "nodir":
    open(out, "wb").write(d[: d.find(b"PK\x01\x02")])
elif kind == "cut" and d.startswith(b"%PDF"):
    open(out, "wb").write(d[: len(d) * 6 // 10])
elif kind == "cut":
    z = zipfile.ZipFile(src)
    main = [i for i in z.infolist() if i.filename in ("word/document.xml", "xl/worksheets/sheet1.xml")][0]
    cut = main.header_offset + 30 + len(main.filename) + main.compress_size // 2
    open(out, "wb").write(d[:cut])
elif kind == "xref":
    i = d.rfind(b"startxref")
    x = d.rfind(b"xref", 0, i)
    b = bytearray(d)
    for k in range(x, x + 40):
        b[k] = 0x20
    b[i : i + 9] = b"startxxxx"
    open(out, "wb").write(bytes(b))
elif kind == "zero":
    b = bytearray(d)
    b[:512] = bytes(512)
    open(out, "wb").write(bytes(b))
else:
    sys.exit("unknown kind")
