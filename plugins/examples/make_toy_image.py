#!/usr/bin/env python3
"""Makes a small test image holding a TOYFS volume, to try a plug-in on.

    python3 make_toy_image.py toy.img

The image has two files: beach.jpg (deleted, 6000 bytes at offset 30000) and
alive.txt (not deleted). The rest is random bytes.
"""
import os
import struct
import sys

out = sys.argv[1] if len(sys.argv) > 1 else "toy.img"
files = [("beach.jpg", 30000, 6000, 1), ("alive.txt", 40000, 500, 0)]

image = bytearray(os.urandom(200_000))
table = b"TOYFS001" + struct.pack("<I", len(files))
for name, start, size, flags in files:
    table += struct.pack("<40sQQI", name.encode(), start, size, flags) + bytes(12)
image[: len(table)] = table
image[30000:36000] = b"\xff\xd8\xff\xe0" + bytes([9]) * 5996  # a stand-in "JPEG"
open(out, "wb").write(image)
print(f"wrote {out}")
