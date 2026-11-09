#!/usr/bin/env python3
"""{{name}}: a Reperio analyzer plug-in for Android phones and disk images.

On a phone it can run any shell, root or adb command, and pull files. On an
image it can read bytes. It reports findings and writes files. Reperio puts
no limit on it and keeps an audit log of what it asked for.

Try it: `reperio android analyze ./out --plugin {{name}}` (phone) or
`reperio analyze image.img ./out` (image). Test: `reperio plugin test {{name}}.py`.
"""
import _reperio as reperio

plugin = reperio.Plugin(
    name="{{name}}",
    kind="analyzer",
    targets=["android", "image"],
    description="Describe what this analyzer looks for in a sentence.",
    version="0.1.0",
    permissions=[],              # e.g. "writes to the phone", "uses the network"
)


@plugin.analyze
def analyze(run):
    if run.target == "android":
        model = run.shell("getprop ro.product.model").strip()
        run.finding("Phone", f"{model}, Android {run.device.get('android', '?')}")
        # run.pull("/sdcard/DCIM/.thumbnails", "thumbs")   # copy files off
    elif run.target == "image":
        head = run.read(0, 512)
        run.finding("Image", f"{run.image_len} bytes; first sector starts {head[:8].hex()}")


plugin.run()
