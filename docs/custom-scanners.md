# Adding your own scanner

A scanner (a "carver") finds one kind of file in raw data and says how long
it is. It is used by the deep scan in the app and in `reperio scan`.

## In three steps

1. Copy `engine/src/formats/custom/_template.rs` to
   `engine/src/formats/custom/my_format.rs` (lower-case letters, digits and
   `_`; a leading `_` means "ignore this file").
2. Edit it: `name`, `matches`, `carve`, the extension in `Found`, and a
   valid `samples()` file.
3. Run:

   ```sh
   cargo test -p reperio-engine custom     # damage tests on your samples
   cargo build --release                   # CLI and app pick it up
   reperio scan /path/to/image -o out --types my_format
   ```

There is nothing to register. `engine/build.rs` finds every file in the
folder and adds it to `formats::all()` (after the built-in scanners with
signatures, before the guess-by-shape ones for CSV and plain text).

## What you write

| Function | Job |
|---|---|
| `name()` | Name used in `--types`. |
| `max_len()` | Largest file you will carve. |
| `matches(head)` | Cheap yes/no on the first 512 bytes. Runs on every sector, so keep it fast and strict (a 4-byte signature alone matches random data). |
| `carve(r, start, limit)` | `r.seek(start)`, walk the file's own structure, return `Found { len, ext, kind, taken }`, or `None` if it is not a real file. Never read at or past `start + limit`. |
| `format()` | Returns your scanner (called by the build). |
| `samples()` | One or more valid files. The test flips bits in them, cuts them short and fills them with noise; your scanner must never panic or claim more bytes than exist. |

Rules of thumb: walk the structure instead of looking for a footer, check
every length against `limit`, and prefer returning `None` over guessing.

Helpers: `Reader` (`u8`, `u16be`, `u32le`, `bytes`, `seek`, `skip`), and
the parsers in the other files of `formats/` (`bmff`, `tiff`, `zip`, ...)
are good models.

## Your own file-system reader (for example your own NTFS tool)

A reader finds files through a drive's own records: names, folders, dates.

1. Copy `engine/src/fs/custom/_template.rs` to `engine/src/fs/custom/my_ntfs.rs`.
2. In `read(src, offset, len)`, parse the volume your own way and return a
   `Volume` (start from `Volume::empty(...)`) with `FsFile`s in `files`.
   Each file's `extents` are byte offsets in `src`. Set `deleted: true` for
   deleted ones. If your tool has names but not locations, put them in
   `names` instead.
3. Set `fs()` to the file system you add to (`FsType::Ntfs`).
4. `cargo build --release`.

Your reader never replaces the built-in one. Its files are added to the
built-in result (the same folder, name, size and deleted flag counts once),
and if the built-in reader finds nothing, yours is used alone. A panic in
your code is caught and your reader skipped. If your tool is a separate
program, port its parsing to Rust here, or keep it outside and image-test it
against Reperio's result with `reperio scan --list`.

## Not drop-in

- Repair of broken files is in `engine/src/fixfile.rs`; edit it by hand.
