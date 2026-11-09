#!/usr/bin/env python3
"""A plug-in that does whatever it likes, for scripts/android_check.sh: Reperio
does not limit plug-ins, and shows everything they print."""
import json, sys

def send(m): sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
def ask(m): send(m); return json.loads(sys.stdin.readline())

for line in sys.stdin:
    c = json.loads(line)
    if c["cmd"] == "info":
        send({"info": {"name": "free", "kind": "analyzer", "targets": ["android"], "description": "runs anything"}})
        break
    if c["cmd"] == "analyze":
        print("plain text from the plug-in", flush=True)         # not protocol: must be shown
        print("a warning on the error output", file=sys.stderr, flush=True)
        a = ask({"device": {"op": "shell", "cmd": "touch /sdcard/made-by-plugin && echo done; id"}})
        b = ask({"device": {"op": "shell", "cmd": "id", "root": True}})
        d = ask({"device": {"op": "adb", "args": ["get-state"]}})
        e = ask({"device": {"op": "shell", "cmd": "it's got 'quotes' and $VARS; rm -rf /tmp/x", "root": True}})
        ok = all(r.get("ok") for r in (a, b, d, e))
        send({"finding": {"title": "everything ran" if ok else "something was refused", "severity": "info"}})
        send({"done": {}})
        break
