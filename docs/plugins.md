# Plug-ins: your own recovery code, no rebuilding

You write a script (or program) for any file system, drop it in a folder, and
Reperio uses it on the next scan. Nothing to edit in Reperio, nothing to
rebuild.

## Using one

1. Put the file in a plug-in folder. `reperio plugins` prints them:
   - `$REPERIO_PLUGINS`, if set
   - Mac: `~/Library/Application Support/Reperio/plugins`
   - Linux: `~/.local/share/reperio/plugins`
   - `plugins/` next to the `reperio` program (this repo's `plugins/` too)
2. Scan as usual, in the app or `reperio scan`. Files your plug-in finds
   are listed, previewed and recovered like any other.

Any language that can read and write lines of JSON works. Three ways to
supply one:

- **A script**: `.py`, `.js`, `.sh`, `.rb` or `.pl` is run with python3, node,
  sh, ruby or perl.
- **Source code**: `.rs` (rustc), `.c` (cc), `.cpp`/`.cc` (c++), `.go` (go), `.zig` (zig) and
  `.swift` (swiftc) are **built for you** the first time and again whenever
  the file changes. The program is kept in `plugin-builds` beside the user
  plug-ins folder, so later scans don't rebuild. The compiler must be
  installed (Reperio looks on the PATH and in `~/.cargo/bin`, Homebrew and
  `/usr/bin`, so it also works from an app started in the Finder). On a Mac,
  `xcode-select --install` gives `cc` and `c++`; Rust is at https://rustup.rs.
- **A finished program** (make it executable: `chmod +x`).

Names starting with `_` or `.`, and `.md`/`.txt`/`.json`/`.h` files, are
ignored.

**If a file can't be used, Reperio says why** instead of skipping it: in
Settings → Plug-ins ("Could not be used"), on the card before a scan when no
plug-in works, and in `reperio plugins`. Typical reasons: the compiler isn't
installed, the source has a compile error (the compiler's message is shown),
the file isn't executable, or it doesn't answer `{"cmd":"info"}` like a
plug-in.

## Choosing which plug-ins to use

Before every scan in the app, a card lists the installed plug-ins with a
tick box each, so you can use one, several or none for that scan:

- **Readers** (file-system plug-ins) add files from volumes Reperio can't
  read itself. Reperio's own readers always run.
- **Analyzers** (forensic plug-ins, `docs/forensic-plugins.md`) run on the
  drive or image after it is scanned. Their findings appear under a **Plug-in
  reports** button in the results.

Your last choice is remembered (a plug-in you add later starts ticked). Tick
"Don't ask again" on the card to reuse it silently, and turn the card back on
in Settings → Scanning. If no plug-in is installed, the card is skipped.

On the command line: `reperio scan DISK --plugin NAME` (repeat for several)
uses only those, `--no-plugins` uses none, and with neither every installed
plug-in is used.

## Installing, removing and keeping track

    reperio plugin install ./my-plugin.py          # a file
    reperio plugin install ./my-plugin/            # a folder with a plugin.json
    reperio plugin install ./my-plugin.zip         # a .zip of either
    reperio plugin install https://example.com/p.py --sha256 ba78...  # a URL, checked
    reperio plugin remove my-plugin

In the app: Settings → Plug-ins → **Install a plug-in…** (and **Remove**, **Test**
on each one). Install copies it into your plug-ins folder, checks that it
starts and says what it is (something that does not is not left behind), and
shows what it says it does. If you give the SHA-256 it should have, a file
that differs is not installed.

**A folder plug-in** is a folder with a `plugin.json` naming the file to run,
and anything else it needs beside it (modules, data):

    my-plugin/
      plugin.json     {"name": "my-plugin", "entry": "main.py"}
      main.py
      helper.py

**New and changed are notes, not blocks.** Reperio remembers each plug-in's
SHA-256 (the first time it sees it, or when you install it). A plug-in you
have not looked at is marked **New**; one whose file is not the one you had is
marked **Changed**, so a swapped file does not go unnoticed. Click **Keep**
(or `reperio plugin trust NAME`) to say you know it. Plug-ins already installed
when Reperio first looks count as known. A plug-in runs either way: Reperio
never refuses code that is yours.

## The manifest

Every plug-in says what it is in its `info` answer. Only `name` is needed;
the rest is optional and lets Reperio show, check and configure it:

    {"info":{
      "name":"my-recoverer", "kind":"volume", "fs":["unknown"],
      "version":"1.2.0", "author":"Sam",
      "description":"What it does, in a sentence",
      "protocol":1,                      // the plug-in protocol it speaks
      "requires":">=0.2",                // the Reperio it needs
      "permissions":["writes to the phone","uses the network"],
      "options":[
        {"id":"depth","label":"How deep","type":"choice",
         "choices":["fast",{"value":"deep","label":"Deep (slow)"}],"default":"fast"},
        {"id":"keep_empty","label":"Keep empty files","type":"toggle","default":false},
        {"id":"min_size","label":"Smallest file (KB)","type":"number","default":4},
        {"id":"prefix","label":"Name prefix","type":"text","default":""}
      ]}}

- **`protocol` and `requires`**: a plug-in that needs a newer Reperio is
  listed under "could not be used" with the reason, not run.
- **`permissions`** are what the plug-in says about itself. Reperio shows
  them next to the plug-in; it does not enforce or judge them.
- **`options`** become a form on the plug-in card (types `toggle`, `text`,
  `number`, `choice`). The person's choices are remembered and sent in every
  start message (`scan`, `analyze`, `open`) as `"options":{"depth":"deep",...}`,
  defaults filled in. They are kept in `plugin-options.json`.

## Fast reads: binary mode

Base64 inside JSON costs about a third more data and a lot of CPU on a big
disk. A plug-in can ask for raw bytes instead:

- **Reading the volume or image (readers, analyzers):** add `"binary":true` to
  the request, `{"read":{"offset":O,"len":L,"binary":true}}`. Reperio answers
  with one line `{"ok":true,"binary":N}` followed by exactly N raw bytes.
- **Serving a device (sources):** say `"binary":true` in the `opened` answer.
  Each reply is then the line `{"ok":true,"binary":N}` followed by exactly N raw
  bytes, in place of `"data"`.

With the SDK it is one flag: `reperio.Plugin(..., binary=True)`.

## Windows

Plug-ins live in `%APPDATA%\Reperio\plugins`; Python scripts run with `python`,
Node scripts with `node`, and a finished program is a `.exe`. Compilers are
looked for as `name.exe` on the PATH and in `%USERPROFILE%\.cargo\bin`. iTunes
backups are in `%APPDATA%\Apple Computer\MobileSync\Backup`. This has been written
but not run on Windows.

## Which plug-in works on which device

The plug-in card only offers what can work on what you opened, and lists the
rest greyed out under **Not for this device**, with the reason:

| Plug-in | Works on |
|---|---|
| Reader (`volume`) | Anything Reperio reads as bytes: a card, drive, image, RAID, or a device served by a source plug-in. |
| Analyzer | What its `targets` say: `android` (a connected phone), `image` (a drive or image), `none`. |
| Tool (`none`) | Nothing in particular; start it from **Run a plug-in…**. |
| Source | Provides devices; see below. |

## Device plug-ins: any device, any method

Reperio reads disks, cards and images itself. For everything else, write a
**source plug-in**: it says how to reach a device (an iPhone over USB, a
camera over MTP, a network share, a cloud bucket, a vendor tool's dump, a
JTAG or chip-off reader) and hands Reperio the bytes. From there Reperio
does what it does for a card: reads file systems (and your reader plug-ins),
carves free space, checks and repairs files, previews and recovers them. The
plug-in only ever answers "give me N bytes at offset X", so it cannot write
through Reperio.

Its devices appear on the home screen under **Other devices**, and on the
command line as `reperio devices`; scan one with
`reperio scan plugin:NAME:DEVICE -o ./recovered`.

Three calls, in lines of JSON, like any plug-in. `info` says
`"kind":"source"`:

    {"info":{"name":"ios-usb","kind":"source","description":"iPhone over USB"}}

**Which devices?** `{"cmd":"devices"}` → one line, then exit:

    {"devices":[{"id":"00008030-0012","name":"Sam's iPhone","size":128000000000,
                 "kind":"phone","detail":"iOS 17, trusted"}]}

`kind` is `card`, `usb`, `phone`, `camera`, `network`, `drive` or anything
else. Listing should take a second or two; it runs while the home screen is
open (and at most every few seconds).

**Open one.** `{"cmd":"open","device":"<id>"}`. Answer
`{"opened":{"size":N,"label":"Sam's iPhone"}}` (or `{"error":"why"}`; the
person sees the text), then serve reads until your input closes. Opening may
wait on the person, such as a "Trust this computer?" tap, for as long as it takes.

    in:  {"read":{"offset":O,"len":L}}                 (L is at most 4 MB)
    out: {"ok":true,"data":"<base64>"}  or  {"ok":false,"error":"why"}

Answer with fewer bytes than asked only at the end of the device. A read
that fails is reported to Reperio as a bad sector, like a damaged card. A
plug-in is never stopped for being slow unless you set an idle limit.

Start from `plugins/examples/source_file.py`: a toy phone, plus any dump
files in `$REPERIO_EXAMPLE_DUMPS`. If a device can only hand over *files*,
not storage, build a disk image of them (FAT is enough) and serve that.

## Writing one

Start from `plugins/examples/` (Python, Rust, C and C++; `README.md` there). Each is a small runnable example.

For forensic analysis code that runs on a phone or an image, and for tools
(any program), see `docs/forensic-plugins.md`. Plug-ins run with full access;
Reperio does not limit them.

Reperio starts your program and sends one line of JSON on its input. You
answer with lines of JSON on its output.

**Ask: what are you?** `{"cmd":"info"}` → answer once, then exit:

    {"info":{"name":"my-ntfs","fs":["ntfs","unknown"]}}

`fs` is the kinds of volume you want: `ntfs`, `fat`, `exfat`, `hfs+`,
`apfs`, `ext`, `unknown` (anything Reperio cannot read: Android f2fs, iOS
images, odd file systems) or `any`.

**Ask: scan this.** `{"cmd":"scan","offset":0,"len":N,"fs":"ntfs"}`. Send any
of these, one per line:

| You send | Meaning |
|---|---|
| `{"read":{"offset":O,"len":L}}` | Give me bytes. Offsets count from the volume's start. Reply comes on your input: `{"ok":true,"data":"<base64>"}` or `{"ok":false}`. Up to 16 MB at once. |
| `{"file":{"path":"a/b.jpg","size":S,"deleted":true,"modified":"2024-05-01T10:20:30","extents":[[O,L],...]}}` | A file found. `extents` are where its data is (volume offsets, in order). `created`, `overwritten` optional. |
| `{"free":[[O,L],...]}` | Unused space, which the deep scan searches. |
| `{"progress":{"done":N,"total":N,"text":"..."}}` | How far along it is (all optional). The scan screen shows it; without it Reperio shows how far the plug-in has read and how many files it has sent. |
| `{"log":"text"}` | Message (your stderr also shows). |
| `{"done":{"label":"My volume"}}` | Finished. |

Send nothing (just exit) when the volume is not yours.

## What it does with yours

- Files from a plug-in are **added** to what Reperio's own reader finds; one
  with the same folder, name, size and deleted flag counts once.
- On a volume Reperio cannot read, the plug-in's result is used by itself.
- Reads go through Reperio, so a plug-in works on disks, images and RAID
  alike. Never open the device yourself; the read-only guarantee holds only
  for reads through Reperio.
- Reperio puts **no limit** on a plug-in: no time limit, no cap on what it
  reads, sends or finds. A plug-in that crashes or sends nonsense is skipped,
  and extents outside the volume are rejected (they could not be read).
  **Stop** ends a run, including a plug-in being waited on. If you want hung
  plug-ins stopped for you, opt in: Settings → Plug-ins, or
  `REPERIO_PLUGIN_IDLE_SECS` and `REPERIO_PLUGIN_MAX_SECS` on the command line.
- Everything a plug-in logs or prints is shown as it happens (the plug-in
  activity view in the app, stderr in the terminal; silence it with
  `REPERIO_PLUGIN_QUIET=1`), and how its last run went is kept and shown.

## Limits, honestly

- A plug-in runs with your user's permissions. Only use code you trust.
- Reader plug-ins work on something Reperio can read as bytes: a disk, a
  partition, an image, a RAID, or a device served by a source plug-in. A phone
  reachable only over USB/MTP is none of those until a source plug-in (above)
  gives Reperio its bytes, or you make an image first (`reperio image`).
- For compiled-in Rust readers and scanners instead, see
  `docs/custom-scanners.md`.
