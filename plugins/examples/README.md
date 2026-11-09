# Example plug-ins

The same plug-in, written four ways, for a made-up file system called
**TOYFS** so each one can run. Copy the one in your language and replace
`scan` with your own parsing.

| File | Language | Build | Plug-in name |
|---|---|---|---|
| `toyfs.py` | Python 3 | none, drop the file in | `toyfs-python` |
| `toyfs.rs` | Rust (std only) | `rustc --edition 2021 -O toyfs.rs -o toyfs-rust` | `toyfs-rust` |
| `toyfs.c` | C | `cc -O2 -o toyfs-c toyfs.c` | `toyfs-c` |
| `toyfs.cpp` | C++17 | `c++ -std=c++17 -O2 -o toyfs-cpp toyfs.cpp` | `toyfs-cpp` |
| `source_file.py` | Python 3 | none, drop the file in | `source-file-example` (a *source*: gives Reperio a device it cannot reach itself; see `docs/plugins.md`) |

Run `./check.sh` to build all four and prove each recovers a deleted file
from a test image, byte for byte. (`make_toy_image.py` makes that image.)

You don't have to build the Rust, C and C++ ones yourself: **drop the source
file (`toyfs.rs`, `toyfs.c`, `toyfs.cpp`) into the plug-ins folder and Reperio
compiles it the first time** (the compiler must be installed). The build
commands above are for doing it by hand.

## Try one by hand

    python3 make_toy_image.py toy.img
    mkdir /tmp/myplugins && cp toyfs.py /tmp/myplugins/
    REPERIO_PLUGINS=/tmp/myplugins reperio plugins
    REPERIO_PLUGINS=/tmp/myplugins reperio scan toy.img -o out --damaged

(`--damaged` because the test "JPEG" is only stand-in bytes. For real files
you do not need it.) To install for good, put the file in the folder
`reperio plugins` lists first under "Put plug-ins in", then scan from the app.

## How to build your own recovery code

1. **Find what your file system keeps.** Where is its table of files, and for
   each file: name, size, flags (deleted?), and where its data is (a list of
   `(start, length)` byte ranges)? `scan()` in each example is the place for it.
2. **Read through Reperio.** Print `{"read":{"offset":O,"len":L}}`, read one
   line back, base64-decode `data`. Offsets count from the start of the
   volume. Do not open the device yourself: that is what keeps it read-only
   and makes the plug-in work on images, RAID and partitions too.
3. **Report each file** as one line, `{"file":{"path":...,"size":...,
   "deleted":true,"extents":[[start,len],...]}}`, then `{"done":{}}`.
4. **Say which volumes you want** in the answer to `info`: `ntfs`, `fat`,
   `exfat`, `hfs+`, `apfs`, `ext`, `unknown` (what Reperio cannot read, such
   as Android f2fs or an iOS image), or `any`.
5. **If the volume is not yours, print nothing and exit.** Never guess.
6. **Be defensive.** Sizes and offsets on a damaged drive are garbage: check
   every number before using it. Reperio rejects extents outside the volume,
   but a crash in your code only costs you your own results.

Debug by printing to **stderr** (stdout is the protocol). Test with a small
image before a real drive. The full protocol is in `docs/plugins.md`.
