#!/usr/bin/env python3
"""Example Reperio forensic plug-in ("analyzer") for Android.

Drop it in a plug-ins folder (`reperio plugins` lists them) and run it with
`reperio android analyze ./out`, or from the Android screen in the app. It
reads the phone only through Reperio, which allows read-only commands and
keeps an audit log of everything this file asks for.

What it does, as a pattern to copy:
  * asks the phone questions with {"device": {"op": "shell", ...}}
  * copies a file off the phone with {"device": {"op": "pull", ...}}
  * reports what it found with {"finding": ...}
  * writes report files into its own folder and announces them with {"output": ...}
  * on a disk image (target "image") reads bytes with {"read": ...}

Protocol: docs/forensic-plugins.md. Debug by printing to stderr.
"""
import base64
import csv
import json
import os
import re
import sys

INFO = {
    "name": "device-report-python",
    "kind": "analyzer",
    "targets": ["android", "image"],
    "description": "Identifies the phone, lists installed apps and what is in Android's trash.",
}


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def ask(message):
    """Sends a request to Reperio and returns its one-line JSON answer."""
    send(message)
    return json.loads(sys.stdin.readline())


def shell(cmd, root=False):
    reply = ask({"device": {"op": "shell", "cmd": cmd, "root": root}})
    return reply.get("out", "") if reply.get("ok") else ""


def finding(title, detail="", severity="info", count=None):
    send({"finding": {"title": title, "detail": detail, "severity": severity, "count": count}})


def analyze_phone(start):
    device = start["device"]
    workdir = start["workdir"]
    send({"progress": {"done": 0, "total": 4, "text": "Identifying the phone"}})
    finding("Phone", f'{device["name"]}, Android {device["android"]}, '
            f'{"encrypted" if device["encrypted"] else "not encrypted"}, '
            f'{"rooted" if device["root"] else "not rooted"}')
    if device["encrypted"]:
        finding("Storage is encrypted",
                "Deleted data cannot be recovered from the raw storage without the keys.", "warn")

    send({"progress": {"done": 1, "total": 4, "text": "Listing installed apps"}})
    packages = [l[len("package:"):] for l in shell("pm list packages -3").splitlines() if l.startswith("package:")]
    finding("Apps installed by the user", ", ".join(packages[:12]) + (" ..." if len(packages) > 12 else ""),
            count=len(packages))
    for app, label in [("com.whatsapp", "WhatsApp"), ("org.telegram.messenger", "Telegram")]:
        if app in packages:
            finding(f"{label} is installed", f"Its media folder may hold deleted photos and videos.", "important")

    send({"progress": {"done": 2, "total": 4, "text": "Reading Android's trash"}})
    rows = shell('content query --uri content://media/external/file --where "is_trashed=1" '
                 "--projection _data:date_expires")
    trashed = []
    for line in rows.splitlines():
        m = re.search(r"_data=(.*?), date_expires=(\d+)", line)
        if m:
            trashed.append((m.group(1), int(m.group(2))))
    if trashed:
        finding("Files in Android's trash", "These can still be copied back.", "important", len(trashed))
        path = os.path.join(workdir, "trashed-media.csv")
        with open(path, "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["path_on_phone", "android_deletes_it_at_unix_time"])
            w.writerows(trashed)
        send({"output": {"path": "trashed-media.csv", "title": "Files in Android's trash",
                         "description": "Where each trashed file is, and when Android erases it."}})
        # Copy the first few off the phone, as an example of the pull operation.
        for i, (remote, _) in enumerate(trashed[:3]):
            reply = ask({"device": {"op": "pull", "remote": remote, "to": f"trashed/{i}-{os.path.basename(remote)}"}})
            if reply.get("ok"):
                send({"log": f"copied {remote}"})
    else:
        finding("Nothing in Android's trash")

    send({"progress": {"done": 3, "total": 4, "text": "Writing the report"}})
    summary = os.path.join(workdir, "device-report.json")
    with open(summary, "w") as f:
        json.dump({"device": device, "user_apps": packages, "trashed_files": len(trashed),
                   "storage": shell("df -h /sdcard").splitlines()}, f, indent=2)
    send({"output": {"path": "device-report.json", "title": "Device summary",
                     "description": "Identity, installed apps and storage."}})


def analyze_image(start):
    """On an image, the plug-in reads bytes through Reperio."""
    reply = ask({"read": {"offset": 0, "len": 4096}})
    head = base64.b64decode(reply["data"]) if reply.get("ok") else b""
    gpt = ask({"read": {"offset": 512, "len": 8}})
    signature = base64.b64decode(gpt["data"]) if gpt.get("ok") else b""
    finding("Image size", f'{start["image_len"]:,} bytes', count=start["image_len"])
    finding("Partition table", "GPT" if signature == b"EFI PART" else "no GPT header at sector 1")
    finding("First bytes", head[:16].hex())


for line in sys.stdin:
    command = json.loads(line)
    if command["cmd"] == "info":
        send({"info": INFO})
        break
    if command["cmd"] == "analyze":
        try:
            (analyze_phone if command["target"] == "android" else analyze_image)(command)
            send({"done": {}})
        except Exception as e:  # report it instead of crashing
            send({"error": str(e)})
        break
