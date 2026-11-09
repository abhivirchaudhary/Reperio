#!/usr/bin/env node
// {{name}}: a Reperio source plug-in, for a device Reperio cannot reach.
// Try: `reperio devices`, then `reperio scan plugin:{{name}}:DEVICE -o ./out`.
const reperio = require("./_reperio");

const SIZE = 1 << 20;
const plugin = new reperio.Plugin({
  name: "{{name}}",
  kind: "source",
  description: "Describe the devices this reaches in a sentence.",
  version: "0.1.0",
});

plugin.devices(() => [reperio.Device("example", "Example device", SIZE, "drive", "Replace me with a real device")]);

plugin.open((id) => {
  if (id !== "example") throw new Error(`no device called ${id}`);
  const read = (offset, length) => Buffer.from(Array.from({ length: Math.min(length, SIZE - offset) }, (_, i) => (offset + i) % 251));
  return reperio.Opened(SIZE, read, "Example device");
});

plugin.run();
