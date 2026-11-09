"""Adds a Volume Shadow Copy to an NTFS image, as Windows would have kept
it, given the volume as it was when the shadow copy was taken.

    vss_store.py BEFORE.img LIVE.img

Windows copies each 16 KB block of the volume to a store file in
"System Volume Information" just before it is first overwritten after a
shadow copy was taken. Linux cannot do that, so this works backwards:
every block that differs between BEFORE and LIVE goes into the store,
written into the space of a placeholder file on LIVE whose 16 KB pieces
are filled with a marker (see vss_check.sh). The catalog and the block
list go there too, and the volume header at 0x1E00 points at them.
Layout as documented by libvshadow.
"""
import struct
import sys
import time

BLOCK = 0x4000
VSS_ID = bytes.fromhex("6b87083876c1484eb7ae04046e6cc752")
MARK = b"REPERIO-VSS-PIECE"
WINDOWS_EPOCH = 11_644_473_600


def block_header(kind, at, next_at=0):
    h = bytearray(128)
    h[:16] = VSS_ID
    struct.pack_into("<IIQQQ", h, 16, 1, kind, 0, at, next_at)
    return h


def main(before_path, live_path):
    before = open(before_path, "rb").read()
    live = bytearray(open(live_path, "rb").read())
    assert len(before) == len(live)

    # The placeholder's pieces: each starts with the marker and its index
    # and must be whole (16 KB in one run) to be used.
    pieces = []
    at = live.find(MARK)
    while at >= 0:
        end = at + BLOCK
        if at % 512 == 0 and live[end - len(MARK):end] == MARK:
            pieces.append(at)
        at = live.find(MARK, at + 1)
    # Windows keeps only blocks in use when the copy was taken; on a fresh
    # volume, free space is zeros, which stands in for its bitmap here.
    zero = bytes(BLOCK)
    changed = [o for o in range(0, len(live), BLOCK)
               if before[o:o + BLOCK] != live[o:o + BLOCK] and before[o:o + BLOCK] != zero
               and not any(p < o + BLOCK and o < p + BLOCK for p in pieces)]
    per_list = (BLOCK - 128) // 32
    lists = -(-len(changed) // per_list)
    assert len(pieces) >= 1 + lists + len(changed), (len(pieces), lists, len(changed))
    catalog, list_blocks, data = pieces[0], pieces[1:1 + lists], pieces[1 + lists:]

    # The volume header must go somewhere NTFS does not use.
    hdr = 0x1E00
    assert not any(live[hdr:hdr + 512]), "0x1E00 is in use"
    h = bytearray(512)
    h[:16] = VSS_ID
    struct.pack_into("<IIQQQQQ", h, 16, 1, 1, hdr, 0, 0, catalog, len(pieces) * BLOCK)
    live[hdr:hdr + 512] = h

    store_id = b"reperio-store-01"
    created = (int(time.time()) + WINDOWS_EPOCH) * 10_000_000
    c = block_header(2, catalog) + bytearray(BLOCK - 128)
    struct.pack_into("<QQ16sQQQ", c, 128, 2, len(live), store_id, 1, 0, created)
    struct.pack_into("<QQ16sQ", c, 256, 3, list_blocks[0] if list_blocks else 0, store_id, 0)
    live[catalog:catalog + BLOCK] = c

    for i, lb in enumerate(list_blocks):
        nxt = list_blocks[i + 1] if i + 1 < len(list_blocks) else 0
        b = block_header(3, lb, nxt) + bytearray(BLOCK - 128)
        for j, orig in enumerate(changed[i * per_list:(i + 1) * per_list]):
            n = i * per_list + j
            struct.pack_into("<QQQII", b, 128 + j * 32, orig, 0, data[n], 0, 0)
            live[data[n]:data[n] + BLOCK] = before[orig:orig + BLOCK]
        live[lb:lb + BLOCK] = b

    open(live_path, "wb").write(live)
    print(f"shadow copy: {len(changed)} changed blocks kept in the store")


if __name__ == "__main__":
    main(*sys.argv[1:])
