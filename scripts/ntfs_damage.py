"""Makes a damaged copy of an NTFS image, breaking what the reader leans on.

    scripts/ntfs_damage.py SRC.img OUT.img KIND...

Kinds:
  split-mft   the MFT's second half is described only by an extension record
              of record 0, as on large, fragmented volumes
  no-record0  record 0 (the MFT's own) is unreadable; $MFTMirr has a copy
  no-bitmap   $Bitmap's record is gone
  no-boot     the first boot sector is gone; NTFS keeps a copy at the end
"""
import struct, sys, shutil, subprocess
src, out, kinds = sys.argv[1], sys.argv[2], sys.argv[3:]
subprocess.run(["cp", "--sparse=always", src, out], check=True)
f = open(out, "r+b")
def rd(at, n): f.seek(at); return bytearray(f.read(n))
def wr(at, b): f.seek(at); f.write(b)
boot = rd(0, 512)
sector = struct.unpack_from("<H", boot, 11)[0]; spc = boot[13]
cluster = sector * spc
mft_lcn = struct.unpack_from("<Q", boot, 48)[0]
cpr = struct.unpack_from("<b", boot, 64)[0]
rsize = 1 << -cpr if cpr < 0 else cpr * cluster
mft = mft_lcn * cluster
def unfix(r):
    usa, cnt = struct.unpack_from("<HH", r, 4)
    for i in range(1, cnt):
        e = i * 512; r[e-2:e] = r[usa+2*i:usa+2*i+2]
    return r
def fix(r):
    usa, cnt = struct.unpack_from("<HH", r, 4)
    usn = r[usa:usa+2]
    for i in range(1, cnt):
        e = i * 512; r[usa+2*i:usa+2*i+2] = r[e-2:e]; r[e-2:e] = usn
    return r
def attrs(r):
    pos = struct.unpack_from("<H", r, 20)[0]
    while True:
        t, l = struct.unpack_from("<II", r, pos)
        if t == 0xFFFFFFFF: return
        yield pos, t, l
        pos += l
for kind in kinds:
    if kind == "split-mft":
        r = unfix(rd(mft, rsize))
        for pos, t, l in attrs(r):
            if t == 0x80 and r[pos+8]:
                ro = pos + struct.unpack_from("<H", r, pos+32)[0]
                h = r[ro]; ls, os_ = h & 15, h >> 4
                length = int.from_bytes(r[ro+1:ro+1+ls], "little")
                lcn = int.from_bytes(r[ro+1+ls:ro+1+ls+os_], "little", signed=True)
                assert r[ro+1+ls+os_] == 0, "expected the MFT in one run"
                half = length // 2
                r[ro+1:ro+1+ls] = half.to_bytes(ls, "little")
                struct.pack_into("<Q", r, pos+24, half - 1)  # last VCN
                break
        wr(mft, fix(r))
        # Record 17 becomes record 0's extension, holding the second half.
        e = bytearray(rsize)
        e[:4] = b"FILE"; struct.pack_into("<HH", e, 4, 48, rsize // 512 + 1)
        struct.pack_into("<H", e, 16, 1)          # sequence
        struct.pack_into("<H", e, 20, 56)         # first attribute
        struct.pack_into("<H", e, 22, 1)          # in use
        struct.pack_into("<Q", e, 32, (1 << 48) | 0)  # base: record 0, seq 1
        e[48:50] = b"\x01\x00"                    # update sequence number
        a = bytearray(72)
        rest = length - half
        runs = bytes([0x44]) + rest.to_bytes(4, "little") + (lcn + half).to_bytes(4, "little", signed=True) + b"\0"
        struct.pack_into("<IIBB", a, 0, 0x80, 0, 1, 0)
        struct.pack_into("<HH", a, 10, 0x40, 0)   # name offset, flags
        struct.pack_into("<QQH", a, 16, half, length - 1, 0x40)
        a = a[:0x40] + runs
        a += b"\0" * (-len(a) % 8)
        struct.pack_into("<I", a, 4, len(a))
        e[56:56+len(a)] = a
        end = 56 + len(a)
        struct.pack_into("<I", e, end, 0xFFFFFFFF)
        struct.pack_into("<I", e, 24, end + 8)    # bytes used
        struct.pack_into("<I", e, 28, rsize)
        wr(mft + 17 * rsize, fix(e))
    elif kind == "no-record0":
        wr(mft, bytes(rsize))
    elif kind == "no-bitmap":
        wr(mft + 6 * rsize, bytes(rsize))
    elif kind == "no-boot":
        wr(0, bytes(512))
    else:
        sys.exit("unknown " + kind)
f.close()
