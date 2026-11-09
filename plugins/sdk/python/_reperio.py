"""Reperio plug-in SDK for Python.

A plug-in is one script. Say what it is, say what it does, and call run():

    import _reperio as reperio

    plugin = reperio.Plugin(name="my-plugin", kind="tool", targets=["none"],
                            description="What it does")

    @plugin.analyze
    def analyze(run):
        run.log("hello")
        run.finding("Hello", "It works", severity="info")

    plugin.run()

Four kinds (set with `kind=`):

  "volume"    reads a file system Reperio can't:       @plugin.scan   def f(volume)
  "analyzer"  looks at a phone or an image:            @plugin.analyze def f(run)
  "tool"      any program, with or without a device:   @plugin.analyze def f(run)
  "source"    reaches a device Reperio can't:          @plugin.devices def f(), @plugin.open def f(device_id)

The protocol underneath is lines of JSON (docs/plugins.md); this module does
the talking so the plug-in does not have to. Reperio puts no limit on what a
plug-in does.
"""
import base64
import json
import os
import sys

__all__ = ["Plugin", "Option", "Device", "Opened"]


def Option(id, label=None, type="text", default=None, choices=None, help=""):
    """A setting. `type` is "toggle", "text", "number" or "choice"."""
    o = {"id": id, "label": label or id, "type": type, "help": help}
    if default is not None:
        o["default"] = default
    if choices:
        o["choices"] = choices
    return o


def Device(id, name=None, size=0, kind="", detail=""):
    """A device a source plug-in can reach."""
    return {"id": id, "name": name or id, "size": size, "kind": kind, "detail": detail}


class Opened:
    """What a source plug-in returns from @plugin.open: how big the device is
    and how to read it. `read(offset, length)` returns bytes (fewer only at the
    end of the device)."""

    def __init__(self, size, read, label=None, close=None):
        self.size, self.read, self.label, self.close = size, read, label, close


class _Io:
    def __init__(self):
        self.stdin = sys.stdin.buffer
        self.stdout = sys.stdout.buffer

    def send(self, obj):
        self.stdout.write(json.dumps(obj, separators=(",", ":")).encode() + b"\n")
        self.stdout.flush()

    def line(self):
        raw = self.stdin.readline()
        return json.loads(raw) if raw.strip() else None

    def bytes(self, n):
        out = b""
        while len(out) < n:
            chunk = self.stdin.read(n - len(out))
            if not chunk:
                raise EOFError("Reperio closed the connection")
            out += chunk
        return out


class _Reads:
    """Reading bytes of the volume or image through Reperio."""

    def __init__(self, io, binary):
        self._io, self._binary = io, binary

    def read(self, offset, length):
        """Bytes at `offset` (counted from the start of the volume or image).
        Raises IOError if they cannot be read."""
        req = {"offset": offset, "len": length}
        if self._binary:
            req["binary"] = True
        self._io.send({"read": req})
        reply = self._io.line()
        if not reply or not reply.get("ok"):
            raise IOError(f"could not read {length} bytes at {offset}")
        if self._binary and "binary" in reply:
            return self._io.bytes(int(reply["binary"]))
        return base64.b64decode(reply.get("data", ""))


class Volume(_Reads):
    """What @plugin.scan is given: one volume to read."""

    def __init__(self, io, start, options, binary):
        super().__init__(io, binary)
        self.offset = start["offset"]
        self.length = start["len"]
        self.fs = start.get("fs", "unknown")
        self.options = options

    def file(self, path, size, extents, deleted=True, modified=None, created=None, overwritten=False):
        """A file found. `extents` is [(offset, length), ...]: where its data is
        in the volume, in order."""
        f = {"path": path, "size": size, "deleted": deleted, "extents": [list(e) for e in extents]}
        if modified:
            f["modified"] = modified
        if created:
            f["created"] = created
        if overwritten:
            f["overwritten"] = True
        self._io.send({"file": f})

    def free(self, extents):
        """Unused space in the volume, which the deep scan searches."""
        self._io.send({"free": [list(e) for e in extents]})

    def log(self, text):
        self._io.send({"log": str(text)})


class Run(_Reads):
    """What @plugin.analyze is given: one run of an analyzer or tool."""

    def __init__(self, io, start, binary):
        super().__init__(io, binary)
        self._start = start
        self.target = start.get("target", "none")  # "android", "image", "ios-backup" or "none"
        self.device = start.get("device") or {}
        self.workdir = start.get("workdir", ".")
        self.args = start.get("args", [])
        self.options = start.get("options", {})
        self.image_len = start.get("image_len")
        self.adb_path = start.get("adb", "adb")

    def _device(self, **req):
        self._io.send({"device": req})
        reply = self._io.line()
        if not reply or not reply.get("ok"):
            raise RuntimeError((reply or {}).get("error", "the phone did not answer"))
        return reply

    def shell(self, cmd, root=False):
        """Runs a command on the phone and returns its output."""
        return self._device(op="shell", cmd=cmd, root=root)["out"]

    def adb(self, *args):
        """Runs `adb -s <phone> ...` and returns its output."""
        return self._device(op="adb", args=list(args))["out"]

    def pull(self, remote, to):
        """Copies a file off the phone into <workdir>/pulled/<to>; returns its path."""
        return self._device(op="pull", remote=remote, to=to)["path"]

    # -- iPhone backups (target "ios-backup")
    def _backup(self, **req):
        self._io.send({"backup": req})
        reply = self._io.line()
        if not reply or not reply.get("ok"):
            raise RuntimeError((reply or {}).get("error", "the backup did not answer"))
        return reply

    @property
    def backup(self):
        """About the backup being analyzed: name, model, ios, date, udid."""
        return self._start.get("backup") or {}

    def backup_files(self, search="", domain=None, limit=5000):
        """Files in the backup whose path contains `search`: [{id, domain, path, size}]."""
        return self._backup(op="files", search=search, domain=domain, limit=limit)["files"]

    def backup_read(self, file_id, offset=0, length=None):
        """Bytes of a file in the backup."""
        req = {"op": "read", "id": file_id, "offset": offset}
        if length is not None:
            req["len"] = length
        if self._binary:
            req["binary"] = True
        self._io.send({"backup": req})
        reply = self._io.line()
        if not reply or not reply.get("ok"):
            raise RuntimeError((reply or {}).get("error", "the backup did not answer"))
        if self._binary and "binary" in reply:
            return self._io.bytes(int(reply["binary"]))
        return base64.b64decode(reply.get("data", ""))

    def backup_export(self, file_id, to):
        """Copies a file out of the backup into <workdir>/pulled/<to>; returns its path."""
        return self._backup(op="export", id=file_id, to=to)["path"]

    def backup_photos(self):
        """The photo library, including Recently Deleted."""
        return self._backup(op="photos")["photos"]

    def backup_messages(self, search="", limit=100000):
        return self._backup(op="messages", search=search, limit=limit)["messages"]

    def backup_deleted_rows(self, domain, path):
        """Deleted rows recovered from a database in the backup:
        [{table, from, page, cells: [{type, value}, ...]}]."""
        return self._backup(op="deleted_rows", domain=domain, path=path)["rows"]

    def repair(self, path, reference=None):
        """Asks Reperio to repair a photo, video or document that will not open
        (a local file: relative to the working folder, or absolute). Returns the
        path of the repaired copy, in <workdir>/repaired/."""
        req = {"path": path}
        if reference:
            req["reference"] = reference
        self._io.send({"repair": req})
        reply = self._io.line()
        if not reply or not reply.get("ok"):
            raise RuntimeError((reply or {}).get("error", "could not be repaired"))
        return reply["path"]

    def file(self, path, size, extents, deleted=True, modified=None):
        """A recoverable file found in the image: `extents` is [(offset, length), ...],
        where its bytes are in the image. It is checked, repaired if it can be, and
        joins the results beside the files Reperio found. (Image targets only.)"""
        f = {"path": path, "size": size, "deleted": deleted, "extents": [list(e) for e in extents]}
        if modified:
            f["modified"] = modified
        self._io.send({"file": f})

    def finding(self, title, detail="", severity="info", count=None, tags=None, confidence=None):
        f = {"title": title, "detail": detail, "severity": severity}
        if count is not None:
            f["count"] = count
        if tags:
            f["tags"] = tags
        if confidence:
            f["confidence"] = confidence
        self._io.send({"finding": f})

    def output(self, path, title="", description=""):
        """Records a file you wrote inside the working folder (it is hashed)."""
        self._io.send({"output": {"path": path, "title": title, "description": description}})

    def progress(self, done, total, text=""):
        self._io.send({"progress": {"done": done, "total": total, "text": text}})

    def log(self, text):
        self._io.send({"log": str(text)})

    def path(self, name):
        """A path inside the working folder."""
        return os.path.join(self.workdir, name)


class Plugin:
    def __init__(self, name, kind="tool", description="", version="", author="", fs=None, targets=None,
                 options=None, permissions=None, requires=None, binary=False):
        self.manifest = {"name": name, "kind": kind, "description": description, "protocol": 1}
        for key, value in (("version", version), ("author", author), ("fs", fs), ("targets", targets),
                           ("options", options), ("permissions", permissions), ("requires", requires)):
            if value:
                self.manifest[key] = value
        self.binary = binary
        self._scan = self._analyze = self._devices = self._open = None

    # -- decorators
    def scan(self, f):
        self._scan = f
        return f

    def analyze(self, f):
        self._analyze = f
        return f

    def devices(self, f):
        self._devices = f
        return f

    def open(self, f):
        self._open = f
        return f

    # -- the loop
    def run(self):
        io = _Io()
        msg = io.line() or {}
        cmd = msg.get("cmd")
        try:
            if cmd == "info":
                io.send({"info": self.manifest})
            elif cmd == "scan" and self._scan:
                v = Volume(io, msg, msg.get("options", {}), self.binary)
                self._scan(v)
                io.send({"done": {}})
            elif cmd == "analyze" and self._analyze:
                self._analyze(Run(io, msg, self.binary))
                io.send({"done": {}})
            elif cmd == "devices" and self._devices:
                io.send({"devices": [d if isinstance(d, dict) else Device(*d) for d in self._devices()]})
            elif cmd == "open" and self._open:
                self._serve(io, msg)
        except EOFError:
            pass
        except Exception as e:  # tell Reperio, don't just die
            try:
                io.send({"error": f"{type(e).__name__}: {e}"})
            except Exception:
                pass
            print(f"{type(e).__name__}: {e}", file=sys.stderr)
            sys.exit(1)

    def _serve(self, io, msg):
        try:
            opened = self._open(msg.get("device", ""))
        except Exception as e:
            io.send({"error": str(e)})
            return
        header = {"size": opened.size, "label": opened.label or msg.get("device", "")}
        if self.binary:
            header["binary"] = True
        io.send({"opened": header})
        while True:
            req = io.line()
            if req is None:
                break
            r = req.get("read")
            if not r:
                continue
            offset, length = int(r["offset"]), int(r["len"])
            try:
                data = opened.read(offset, length) if offset < opened.size else b""
            except Exception as e:
                io.send({"ok": False, "error": str(e)})
                continue
            if self.binary:
                io.send({"ok": True, "binary": len(data)})
                io.stdout.write(data)
                io.stdout.flush()
            else:
                io.send({"ok": True, "data": base64.b64encode(data).decode()})
        if opened.close:
            opened.close()
