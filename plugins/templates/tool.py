#!/usr/bin/env python3
"""{{name}}: a Reperio tool plug-in.

A tool is any program. It runs on its own (or on a phone or an image if you
add "android" or "image" to `targets`), reports what it finds, and writes
files into its working folder. Start it from Reperio's "Run a plug-in…", or
`reperio run {{name}}`. Test it with `reperio plugin test {{name}}.py`.
"""
import _reperio as reperio

plugin = reperio.Plugin(
    name="{{name}}",
    kind="tool",
    targets=["none"],            # "none", "android", "image" (any mix)
    description="Describe what this plug-in does in a sentence.",
    version="0.1.0",
    options=[
        reperio.Option("greeting", "Greeting", type="text", default="Hello"),
    ],
)


@plugin.analyze
def analyze(run):
    run.log(f"started with options {run.options} and arguments {run.args}")
    run.progress(0, 1, "working")

    # Write a file into the working folder, then tell Reperio about it.
    with open(run.path("hello.txt"), "w") as f:
        f.write(f"{run.options.get('greeting', 'Hello')} from {{name}}\n")
    run.output("hello.txt", title="A greeting", description="A file this tool wrote")

    run.finding("It ran", "This tool did what it was asked.", severity="info")
    run.progress(1, 1, "done")


plugin.run()
