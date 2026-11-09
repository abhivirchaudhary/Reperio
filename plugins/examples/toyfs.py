#!/usr/bin/env python3
"""Example Reperio plug-in: reads a made-up file system called TOYFS.

Copy this file, change `INFO` and `scan()`, and drop it in the plug-ins
folder (`reperio plugins` shows where). Nothing to build or install.

TOYFS layout, so the example is runnable:
  bytes 0..8    b"TOYFS001"
  bytes 8..12   number of entries (u32 little endian)
  then 64-byte entries: name (40 bytes, zero padded), start (u64),
  size (u64), flags (u32: 1 = deleted), padding.
"""
import base64
import json
import struct
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def read(offset, length):
    """Bytes of the volume, as Reperio reads them (offset 0 = volume start)."""
    send({"read": {"offset": offset, "len": length}})
    reply = json.loads(sys.stdin.readline())
    return base64.b64decode(reply["data"]) if reply.get("ok") else b""


INFO = {"name": "toyfs-python", "fs": ["unknown"]}  # or ntfs, exfat, apfs, ext, any


def scan(volume_len):
    head = read(0, 12)
    if head[:8] != b"TOYFS001":
        return  # not ours: send nothing, Reperio moves on
    (count,) = struct.unpack("<I", head[8:12])
    table = read(12, 64 * count)
    for i in range(count):
        name, start, size, flags = struct.unpack_from("<40sQQI", table, 64 * i)
        send({"file": {
            "path": "Photos/" + name.rstrip(b"\0").decode(),
            "size": size,
            "deleted": bool(flags & 1),
            "extents": [[start, size]],
        }})
    send({"done": {"label": "TOYFS volume"}})


for line in sys.stdin:
    command = json.loads(line)
    if command["cmd"] == "info":
        send({"info": INFO})
        break
    if command["cmd"] == "scan":
        scan(command["len"])
        break
