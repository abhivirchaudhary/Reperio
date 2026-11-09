# Plug-ins

Reperio's plug-ins are not published in this repository. If you want plug-ins,
such as the Red Team forensic suite, open an issue or contact Abhivir Chaudhary
through [GitHub](https://github.com/abhivirchaudhary).

This folder keeps what you need to write your own:

- `sdk/`: the Python and JavaScript helpers a plug-in imports.
- `templates/`: the starting points `reperio plugin new NAME --kind ...` copies out.
- `examples/`: small working examples in Python, Rust, C and C++. They are not
  active until you copy one out of that folder. Start with `examples/README.md`.

Drop a script or program here (or in the folder `reperio plugins` prints) and
Reperio uses it on the next scan. No editing, no rebuilding. Python (`.py`),
Node (`.js`), shell (`.sh`), Ruby, Perl, **source code** (`.rs`, `.c`, `.cpp`,
`.go`, `.swift`, `.zig`: Reperio compiles it for you the first time), or a finished
program you made executable.

The protocol is in `docs/plugins.md`.
