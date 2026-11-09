# Writing a plug-in, the quick way

    reperio plugin new my-plugin --kind tool --lang py
    reperio plugin test ~/.local/share/reperio/plugins/my-plugin.py   # or the path it printed
    reperio plugins

`plugin new` writes a working starter and the SDK beside it (`_reperio.py` or
`_reperio.js`; the leading `_` keeps Reperio from treating it as a plug-in).
`--kind` is one of:

| kind | use it to | you write |
|---|---|---|
| `tool` | run any program, with or without a device | `@plugin.analyze def f(run)` |
| `analyzer` | look at an Android phone or a disk image | `@plugin.analyze def f(run)` |
| `reader` | read a file system Reperio doesn't know | `@plugin.scan def f(volume)` |
| `source` | reach a device Reperio can't (iPhone, MTP, network...) | `@plugin.devices`, `@plugin.open` |

Every starter is commented, runs, and passes `reperio plugin test`.

## The SDK in one page (Python; JS mirrors it, see `plugins/sdk/js/_reperio.js`)

    import _reperio as reperio
    plugin = reperio.Plugin(name="x", kind="analyzer", targets=["android", "image"],
                            description="...", version="1.0.0",
                            permissions=["writes to the phone"],
                            options=[reperio.Option("deep", "Deep search", type="toggle", default=False)])

**run** (analyzer, tool): `run.target` (`android`/`image`/`none`), `run.device`,
`run.options`, `run.args`, `run.workdir`, `run.path(name)`;
`run.shell(cmd, root=False)`, `run.adb(*args)`, `run.pull(remote, to)`,
`run.read(offset, length)`; `run.finding(title, detail, severity, count)`,
`run.output(path, title, description)`, `run.progress(done, total, text)`,
`run.log(text)`.

**volume** (reader): `volume.read(offset, length)`, `volume.file(path, size,
extents, deleted=True, modified=...)`, `volume.free(extents)`, `volume.log(text)`,
`volume.offset/length/fs/options`.

**source**: `@plugin.devices` returns `[reperio.Device(id, name, size, kind,
detail)]`; `@plugin.open` returns `reperio.Opened(size, read, label)` where
`read(offset, length)` gives bytes.

Reperio puts no limit on what a plug-in does; see `docs/plugins.md` for the
protocol underneath and `docs/forensic-plugins.md` for what Reperio records.

## What `plugin test` checks

It starts the plug-in and checks it answers `info`, has a sensible manifest,
and then, for its kind: a **reader** scans an empty and a noisy volume; an
**analyzer/tool** runs on nothing and on an empty image (a phone is not
needed or used); a **source** must list devices, open one, read the start
twice and get the same bytes, read the end, and get an empty answer past the
end. A plug-in that hangs is stopped after 30 seconds. It shows Reperio can
talk to the plug-in, not that it finds the right things. In the app: Settings →
Plug-ins → **Test**.
