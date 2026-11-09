#!/usr/bin/env python3
"""{{name}}: a Reperio source plug-in, for a device Reperio cannot reach.

It lists the devices it can reach and serves their bytes. From there Reperio
does everything it does for a card: reads file systems, carves free space,
repairs, previews and recovers. It never asks the plug-in to write.

Try it: `reperio devices`, then `reperio scan plugin:{{name}}:DEVICE -o ./out`.
Test: `reperio plugin test {{name}}.py`.
"""
import _reperio as reperio

plugin = reperio.Plugin(
    name="{{name}}",
    kind="source",
    description="Describe the devices this reaches in a sentence.",
    version="0.1.0",
    # binary=True,              # raw bytes instead of base64: much faster on big disks
)

SIZE = 1 << 20


@plugin.devices
def devices():
    # One entry per device you can reach right now (keep this quick).
    return [reperio.Device("example", name="Example device", size=SIZE, kind="drive",
                           detail="Replace me with a real device")]


@plugin.open
def open_device(device_id):
    if device_id != "example":
        raise ValueError(f"no device called {device_id}")

    def read(offset, length):
        # Return `length` bytes at `offset` (fewer only at the end).
        # Replace with a real read: USB, MTP, network, a vendor tool...
        return bytes((offset + i) % 251 for i in range(min(length, SIZE - offset)))

    return reperio.Opened(size=SIZE, read=read, label="Example device")


plugin.run()
