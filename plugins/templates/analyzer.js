#!/usr/bin/env node
// {{name}}: a Reperio analyzer for Android phones and images.
// Try: `reperio android analyze ./out --plugin {{name}}`. Test: `reperio plugin test {{name}}.js`.
const reperio = require("./_reperio");

const plugin = new reperio.Plugin({
  name: "{{name}}",
  kind: "analyzer",
  targets: ["android", "image"],
  description: "Describe what this analyzer looks for in a sentence.",
  version: "0.1.0",
});

plugin.analyze((run) => {
  if (run.target === "android") {
    run.finding("Phone", run.shell("getprop ro.product.model").trim());
  } else if (run.target === "image") {
    run.finding("Image", `${run.imageLen} bytes; starts ${run.read(0, 8).toString("hex")}`);
  }
});

plugin.run();
