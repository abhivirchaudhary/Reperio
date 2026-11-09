# Forensic plug-ins and tools

Your own code, loaded without editing or rebuilding Reperio. You write a
script or program (any language), put it in a plug-ins folder, and run it
from the app or the command line.

**Reperio runs any plug-in you make. It does not limit them.** A plug-in runs
with your user's permissions on this computer, and on a connected phone it
can run any command (`shell`, with or without `su`) and any `adb` command.
You know what your plug-in does; Reperio does not second-guess it. What
Reperio adds is a *record*, not a restriction (below). Only run code you
trust, and be careful with anything that writes to a phone.

Two kinds, both set by `"kind"` in the plug-in's `info` answer:

- **`analyzer`**: works on a phone or an image, reports findings and files.
- **`tool`**: any program at all, with or without a phone or image. It needs no
  protocol: it is started, given its arguments, and when it exits with status
  0 it has finished. Files it leaves in its folder are listed (and hashed)
  as its output.

```
reperio plugins                                    where to put it, and what is installed
reperio android analyze ./out                      run every analyzer on the connected phone
reperio android analyze ./out --plugin my-analyzer
reperio analyze phone.img ./out                    run analyzers that work on images
reperio run my-tool -o ./out one "two words" --flag    any plug-in; options first, then its arguments
reperio run my-analyzer -o ./out --phone           on the phone;  --image FILE  on an image
```

In the app: **Run a plug-in…** on the home screen (tools, with an arguments
box), the plug-in card on the **Android phone…** screen, and the card shown
before each scan.

This is the second kind of plug-in. The first (file-system readers) is in
`docs/plugins.md`; the folders and the way a plug-in is started are the same.

## Start from the example

`plugins/examples/analyzer_device_report.py` identifies the phone, lists
installed apps, reads Android's trash, copies a few trashed files, and writes
a CSV and a JSON report. It uses every part of the protocol below, and it is
about 100 lines. Copy it, change `INFO` and `analyze_phone`, and drop it in a
plug-ins folder.

## What Reperio records

- **An audit log** (`audit.log`): every request the plug-in made of the
  phone, when, what came back, and the SHA-256 of each file it copied.
- **Everything it printed** (`console.log`): plain text on its output that is
  not part of the protocol, and its error output. Shown with the report in
  the app and in the terminal, so an ordinary `print()` is never lost.
- **Fingerprints.** Each file you report is hashed. `report.json` records
  the plug-in's own SHA-256 (which code ran), the findings, the files with
  their hashes, the exit status and the counts. `MANIFEST.sha256.txt` lists
  the hashes of everything in the folder.
- **A folder of its own**, `<dest>/<plug-in name>/`, which your plug-in
  starts in.

## The protocol

Reperio starts your program and talks in lines of JSON: it writes to your
input, you answer on your output. Debug with stderr.

**Info.** `{"cmd":"info"}` → answer once:

    {"info":{"name":"my-analyzer","kind":"analyzer",
             "targets":["android","image"],"description":"What it does"}}

**Run.** `{"cmd":"analyze","target":"android","device":{...},"workdir":"/abs/folder","args":[...]}`.
`target` is `android` (a connected phone), `image` (then also `image_len`) or `none`. `args` are the
arguments you gave it.
`device` says what the phone is: `name`, `android`, `patch`, `encrypted`,
`root`, `serial`, and for rooted phones `storage` and `partitions`.

Then send any of these, one per line:

| You send | Reperio answers / does |
|---|---|
| `{"device":{"op":"shell","cmd":"getprop ro.product.model","root":false}}` | Runs any command on the phone: `{"ok":true,"out":"text"}`, or `{"ok":false,"error":"why"}`. `root:true` runs it with `su`. |
| `{"device":{"op":"adb","args":["get-state"]}}` | Runs `adb -s <phone> <args>` as given (the start message also tells you where `adb` is, as `adb`). |
| `{"device":{"op":"pull","remote":"/sdcard/x.db","to":"x.db"}}` | Copies the file to `<workdir>/pulled/x.db`: `{"ok":true,"path":"/abs/..."}`. Parse it yourself. |
| `{"read":{"offset":N,"len":N}}` (image target) | `{"ok":true,"data":"<base64>"}`, up to 16 MB at a time. |
| `{"finding":{"title":"...","detail":"...","severity":"info\|warn\|important","count":N}}` | Adds a finding to the report and the screen. |
| `{"output":{"path":"messages.csv","title":"...","description":"..."}}` | Records a file you wrote inside your folder (hashed). |
| `{"progress":{"done":N,"total":N,"text":"..."}}` | Updates the progress bar. |
| `{"log":"text"}` | A line in the audit log. |
| `{"done":{}}` / `{"error":"..."}` | Finish (or fail). |

## iPhone backups

An analyzer whose `targets` include `ios-backup` can work on an iPhone backup
(`reperio ios analyze BACKUP DEST`, or **Plug-ins** in the app's backup view).
The start message has `"target":"ios-backup"` and `"backup":{name, model, ios,
date, udid}`. Requests, each `{"backup":{"op":...}}`:

| op | gives |
|---|---|
| `files` (`search`, `domain`, `limit`) | `{"ok":true,"files":[{id,domain,path,size}]}` |
| `read` (`id`, `offset`, `len`, `binary`) | the file's bytes, base64 or raw (see binary mode) |
| `export` (`id`, `to`) | copies the file to `<workdir>/pulled/<to>`, gives its path |
| `photos` | the photo library, including Recently Deleted |
| `messages` (`search`, `limit`) | messages, including recently deleted |
| `deleted_rows` (`domain`, `path`) | rows deleted from a database in the backup, whole, from its free pages and write-ahead log |

SDK: `run.backup_files()`, `run.backup_read()`, `run.backup_export()`,
`run.backup_photos()`, `run.backup_messages()`, `run.backup_deleted_rows()`,
`run.backup`. That is Reperio's SQLite deleted-row recovery for any app's
database, available to your code in one call.

## Reperio's repair, for plug-ins

Any analyzer or tool can hand Reperio a damaged photo, video or document it
pulled or wrote (a local file, relative to its folder or absolute):

    {"repair":{"path":"pulled/IMG_1.jpg","reference":"pulled/good.jpg"}}
    -> {"ok":true,"path":"<workdir>/repaired/IMG_1 (repaired).jpg","how":"..."}

`reference` is a good file from the same camera, for videos and photos whose
header is lost. With the SDK: `run.repair(path)`.

## Offering files to be recovered

An analyzer working on an **image or disk** can hand Reperio files it found,
and they join the results, with previews, repair and recovery, as if Reperio
had found them (the pill says *Found by <plug-in>*):

    {"file":{"path":"Cache/pics/lost.jpg","size":18432,"deleted":true,
             "modified":"2024-05-01T10:20:30","extents":[[5000,18432]]}}

`extents` are `[offset, length]` places in the image, in order; one that lies
outside the image is refused (and logged). With the SDK: `run.file(path, size,
extents)`. A phone has no byte offsets, so on a phone a plug-in `pull`s files
and reports them as outputs instead.

## What it cannot do

- **Decrypt encrypted storage for you.** Reperio has no keys. A plug-in that
  has them can do the work itself.
- **A live iPhone.** There is no iOS connection, only images you already have.
- **Change Reperio's screens.** Results appear as findings, printed output
  and files, not as custom views.
- **Add files to the main list from a phone.** On an image or disk a plug-in
  can offer recoverable files (above). A phone has no byte offsets, so its
  files are pulled, hashed and listed in the plug-in's own report.
- A plug-in that needs a library you have not installed fails, and the
  error is shown in what it printed.

## Ideas

WhatsApp/Telegram media inventory, SMS and call-log export (live data, where
the phone allows it), app-cache image finder, timeline from `dumpsys
usagestats`, SQLite deleted-row recovery (`pull` the database, then parse its
free pages), a report builder that turns several analyzers' findings into
one document.

## Tests

`scripts/android_check.sh` runs the example analyzer, a plug-in that does
anything (`scripts/free_analyzer.py`: shell, root and `adb` commands, plain
text and error output) and a tool (`scripts/free_tool.py`, with arguments)
against a fake phone. `app/tests/gui_android.py`, `gui_plugin_card.py` and
`gui_tools.py` run the app's screens. None has been tried on a real phone yet.
