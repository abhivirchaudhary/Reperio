#!/usr/bin/env python3
"""A source plug-in: gives Reperio a device it can't reach itself.

Reperio calls this program three ways (docs/plugins.md, "Device plug-ins"):

  {"cmd":"info"}                       who are you
  {"cmd":"devices"}                    which devices can you reach right now
  {"cmd":"open","device":"<id>"}       serve bytes of that device until stdin closes

Here the "device" is a made-up phone whose storage is a repeating pattern,
plus every file in the folder named by $REPERIO_EXAMPLE_DUMPS (disk dumps
made by some tool Reperio does not know). A real plug-in would talk to its
hardware or service where this one reads a file: an iPhone over USB, a
camera over MTP, a network share, a cloud bucket.

The plug-in only has to answer "bytes at offset"; Reperio does the scanning,
repair, preview and recovery, and never asks the plug-in to write.
"""
import base64
import json
import os
import sys

NAME = "source-file-example"
TOY_ID = "toy-phone"
TOY_SIZE = 1 << 20


def info():
    return {"info": {"name": NAME, "kind": "source",
                     "description": "Example: a toy phone, and dump files from $REPERIO_EXAMPLE_DUMPS"}}


def dumps():
    folder = os.environ.get("REPERIO_EXAMPLE_DUMPS")
    if not folder or not os.path.isdir(folder):
        return {}
    return {f"file:{n}": os.path.join(folder, n)
            for n in sorted(os.listdir(folder))
            if os.path.isfile(os.path.join(folder, n))}


def devices():
    out = [{"id": TOY_ID, "name": "Toy phone", "size": TOY_SIZE, "kind": "phone",
            "detail": "A made-up device, for trying plug-ins out"}]
    for dev_id, path in dumps().items():
        out.append({"id": dev_id, "name": os.path.basename(path), "size": os.path.getsize(path),
                    "kind": "drive", "detail": "A dump file"})
    return {"devices": out}


def serve(device):
    """Open `device`, then answer reads until Reperio closes our input."""
    path = dumps().get(device)
    if device == TOY_ID:
        size = TOY_SIZE
        read = lambda off, n: bytes((off + i) % 251 for i in range(n))
        label = "Toy phone"
    elif path:
        size = os.path.getsize(path)
        f = open(path, "rb")  # read-only: this plug-in never writes

        def read(off, n):
            f.seek(off)
            return f.read(n)
        label = os.path.basename(path)
    else:
        print(json.dumps({"error": f"No device called {device}"}), flush=True)
        return
    print(json.dumps({"opened": {"size": size, "label": label}}), flush=True)
    for line in sys.stdin:
        msg = json.loads(line)
        r = msg.get("read")
        if not r:
            continue
        off, n = int(r["offset"]), int(r["len"])
        if off >= size:
            data = b""
        else:
            data = read(off, min(n, size - off))
        print(json.dumps({"ok": True, "data": base64.b64encode(data).decode()}), flush=True)


def main():
    msg = json.loads(sys.stdin.readline() or "{}")
    cmd = msg.get("cmd")
    if cmd == "info":
        print(json.dumps(info()), flush=True)
    elif cmd == "devices":
        print(json.dumps(devices()), flush=True)
    elif cmd == "open":
        serve(msg.get("device", ""))


if __name__ == "__main__":
    main()
