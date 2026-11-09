#!/usr/bin/env python3
"""{{name}}: a Reperio file-system reader plug-in.

Reperio asks it to read a volume it does not understand (or any volume, if
`fs` includes "any"). It reads bytes with volume.read(offset, length) and
reports files it finds, with where their data is, so Reperio can preview,
repair and recover them. Reads go through Reperio, so it works on a disk, an
image or a RAID alike.

Test it: `reperio plugin test {{name}}.py`.
"""
import _reperio as reperio

plugin = reperio.Plugin(
    name="{{name}}",
    kind="volume",
    fs=["unknown"],              # "ntfs", "fat", "exfat", "hfs+", "apfs", "ext", "unknown", "any"
    description="Describe the file system this reads in a sentence.",
    version="0.1.0",
    # binary=True,              # raw bytes instead of base64: much faster on big disks
)


@plugin.scan
def scan(volume):
    header = volume.read(0, 512)
    if header[:8] != b"MYFS0001":
        return                   # not our file system: say nothing

    # Report each file: its path, its size, and where its bytes are
    # (volume offsets, in order). deleted=True lists it as a deleted file.
    volume.file("DCIM/gone.jpg", size=100, extents=[(4096, 100)], deleted=True,
                modified="2024-05-01T10:20:30")
    # Unused space, which the deep scan then searches:
    volume.free([(8192, volume.length - 8192)])


plugin.run()
