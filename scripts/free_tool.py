#!/usr/bin/env python3
"""A tool plug-in for scripts/android_check.sh: no protocol after info. It gets its
arguments in the start message, prints, writes a file, and exits."""
import json, sys

for line in sys.stdin:
    c = json.loads(line)
    if c["cmd"] == "info":
        print(json.dumps({"info": {"name": "free-tool", "kind": "tool", "targets": ["none", "android"], "description": "does anything"}}), flush=True)
        break
    if c["cmd"] == "analyze":
        print("tool started with args:", c["args"], "target:", c["target"])
        open("result.txt", "w").write("args=" + ",".join(c["args"]) + "\n")
        print("tool wrote result.txt")
        break
