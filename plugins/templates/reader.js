#!/usr/bin/env node
// {{name}}: a Reperio file-system reader. Test: `reperio plugin test {{name}}.js`.
const reperio = require("./_reperio");

const plugin = new reperio.Plugin({
  name: "{{name}}",
  kind: "volume",
  fs: ["unknown"], // "ntfs", "fat", "exfat", "hfs+", "apfs", "ext", "unknown", "any"
  description: "Describe the file system this reads in a sentence.",
  version: "0.1.0",
});

plugin.scan((volume) => {
  if (volume.read(0, 8).toString() !== "MYFS0001") return; // not ours: say nothing
  volume.file("DCIM/gone.jpg", 100, [[4096, 100]], { deleted: true, modified: "2024-05-01T10:20:30" });
  volume.free([[8192, volume.length - 8192]]);
});

plugin.run();
