#!/usr/bin/env node
// {{name}}: a Reperio tool plug-in (see the Python template for the full story).
// Run it from "Run a plug-in…" or `reperio run {{name}}`. Test: `reperio plugin test {{name}}.js`.
const reperio = require("./_reperio");

const plugin = new reperio.Plugin({
  name: "{{name}}",
  kind: "tool",
  targets: ["none"], // "none", "android", "image"
  description: "Describe what this plug-in does in a sentence.",
  version: "0.1.0",
  options: [reperio.Option("greeting", "Greeting", "text", "Hello")],
});

plugin.analyze((run) => {
  run.log(`started with options ${JSON.stringify(run.options)}`);
  require("fs").writeFileSync(run.path("hello.txt"), `${run.options.greeting || "Hello"} from {{name}}\n`);
  run.output("hello.txt", "A greeting", "A file this tool wrote");
  run.finding("It ran", "This tool did what it was asked.");
});

plugin.run();
