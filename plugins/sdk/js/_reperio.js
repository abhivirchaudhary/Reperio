"use strict";
// Reperio plug-in SDK for Node.js. Same shape as the Python SDK (_reperio.py):
//
//   const reperio = require("./_reperio");
//   const plugin = new reperio.Plugin({ name: "my-plugin", kind: "tool", targets: ["none"], description: "..." });
//   plugin.analyze((run) => { run.log("hello"); run.finding("Hello", "It works"); });
//   plugin.run();
//
// Kinds: "volume" (plugin.scan(volume)), "analyzer" and "tool" (plugin.analyze(run)),
// "source" (plugin.devices(), plugin.open(deviceId)). It talks in lines of JSON
// (docs/plugins.md). Calls are synchronous, so a plug-in reads top to bottom.
// Reperio puts no limit on what a plug-in does.
const fs = require("fs");

const Option = (id, label, type = "text", def, choices, help = "") => ({ id, label: label || id, type, help, ...(def !== undefined && { default: def }), ...(choices && { choices }) });
const Device = (id, name, size = 0, kind = "", detail = "") => ({ id, name: name || id, size, kind, detail });

class Io {
  constructor() { this.buf = Buffer.alloc(0); }
  send(obj) { fs.writeSync(1, JSON.stringify(obj) + "\n"); }
  fill() {
    const chunk = Buffer.alloc(1 << 20);
    let n;
    try { n = fs.readSync(0, chunk, 0, chunk.length, null); } catch (e) { if (e.code === "EAGAIN") return this.fill(); if (e.code === "EOF") n = 0; else throw e; }
    if (n === 0) return false;
    this.buf = Buffer.concat([this.buf, chunk.subarray(0, n)]);
    return true;
  }
  line() {
    for (;;) {
      const i = this.buf.indexOf(10);
      if (i >= 0) { const l = this.buf.subarray(0, i).toString(); this.buf = this.buf.subarray(i + 1); return l.trim() ? JSON.parse(l) : null; }
      if (!this.fill()) { const l = this.buf.toString(); this.buf = Buffer.alloc(0); return l.trim() ? JSON.parse(l) : null; }
    }
  }
  bytes(n) {
    while (this.buf.length < n) if (!this.fill()) throw new Error("Reperio closed the connection");
    const out = this.buf.subarray(0, n);
    this.buf = this.buf.subarray(n);
    return Buffer.from(out);
  }
}

class Reads {
  constructor(io, binary) { this._io = io; this._binary = binary; }
  /** Bytes at `offset`, counted from the start of the volume or image. */
  read(offset, length) {
    this._io.send({ read: { offset, len: length, ...(this._binary && { binary: true }) } });
    const r = this._io.line();
    if (!r || !r.ok) throw new Error(`could not read ${length} bytes at ${offset}`);
    return this._binary && r.binary !== undefined ? this._io.bytes(Number(r.binary)) : Buffer.from(r.data || "", "base64");
  }
}

class Volume extends Reads {
  constructor(io, start, binary) { super(io, binary); Object.assign(this, { offset: start.offset, length: start.len, fs: start.fs || "unknown", options: start.options || {} }); }
  /** A file found: `extents` is [[offset, length], ...] in the volume. */
  file(path, size, extents, { deleted = true, modified, created, overwritten } = {}) {
    this._io.send({ file: { path, size, deleted, extents, ...(modified && { modified }), ...(created && { created }), ...(overwritten && { overwritten }) } });
  }
  free(extents) { this._io.send({ free: extents }); }
  log(text) { this._io.send({ log: String(text) }); }
}

class Run extends Reads {
  constructor(io, start, binary) {
    super(io, binary);
    this._start = start;
    Object.assign(this, { target: start.target || "none", device: start.device || {}, workdir: start.workdir || ".", args: start.args || [], options: start.options || {}, imageLen: start.image_len, adbPath: start.adb || "adb" });
  }
  _device(req) {
    this._io.send({ device: req });
    const r = this._io.line();
    if (!r || !r.ok) throw new Error((r && r.error) || "the phone did not answer");
    return r;
  }
  shell(cmd, root = false) { return this._device({ op: "shell", cmd, root }).out; }
  adb(...args) { return this._device({ op: "adb", args }).out; }
  pull(remote, to) { return this._device({ op: "pull", remote, to }).path; }
  // iPhone backups (target "ios-backup")
  _backup(req) {
    this._io.send({ backup: req });
    const r = this._io.line();
    if (!r || !r.ok) throw new Error((r && r.error) || "the backup did not answer");
    return r;
  }
  get backup() { return this._start.backup || {}; }
  backupFiles(search = "", domain = null, limit = 5000) { return this._backup({ op: "files", search, domain, limit }).files; }
  backupRead(id, offset = 0, length) {
    this._io.send({ backup: { op: "read", id, offset, ...(length !== undefined && { len: length }), ...(this._binary && { binary: true }) } });
    const r = this._io.line();
    if (!r || !r.ok) throw new Error((r && r.error) || "the backup did not answer");
    return this._binary && r.binary !== undefined ? this._io.bytes(Number(r.binary)) : Buffer.from(r.data || "", "base64");
  }
  backupExport(id, to) { return this._backup({ op: "export", id, to }).path; }
  backupPhotos() { return this._backup({ op: "photos" }).photos; }
  backupMessages(search = "", limit = 100000) { return this._backup({ op: "messages", search, limit }).messages; }
  backupDeletedRows(domain, path) { return this._backup({ op: "deleted_rows", domain, path }).rows; }
  /** Asks Reperio to repair a photo, video or document that will not open; returns the repaired copy's path. */
  repair(path, reference) {
    this._io.send({ repair: { path, ...(reference && { reference }) } });
    const r = this._io.line();
    if (!r || !r.ok) throw new Error((r && r.error) || "could not be repaired");
    return r.path;
  }
  /** A recoverable file found in the image: `extents` is [[offset, length], ...]. */
  file(path, size, extents, { deleted = true, modified } = {}) { this._io.send({ file: { path, size, deleted, extents, ...(modified && { modified }) } }); }
  finding(title, detail = "", severity = "info", count) { this._io.send({ finding: { title, detail, severity, ...(count !== undefined && { count }) } }); }
  output(path, title = "", description = "") { this._io.send({ output: { path, title, description } }); }
  progress(done, total, text = "") { this._io.send({ progress: { done, total, text } }); }
  log(text) { this._io.send({ log: String(text) }); }
  path(name) { return require("path").join(this.workdir, name); }
}

class Plugin {
  constructor({ name, kind = "tool", description = "", version, author, fs: fsKinds, targets, options, permissions, requires, binary = false }) {
    this.manifest = { name, kind, description, protocol: 1, ...Object.fromEntries(Object.entries({ version, author, fs: fsKinds, targets, options, permissions, requires }).filter(([, v]) => v && (!Array.isArray(v) || v.length))) };
    this.binary = binary;
  }
  scan(f) { this._scan = f; }
  analyze(f) { this._analyze = f; }
  devices(f) { this._devices = f; }
  open(f) { this._open = f; }
  run() {
    const io = new Io();
    const msg = io.line() || {};
    try {
      if (msg.cmd === "info") io.send({ info: this.manifest });
      else if (msg.cmd === "scan" && this._scan) { this._scan(new Volume(io, msg, this.binary)); io.send({ done: {} }); }
      else if (msg.cmd === "analyze" && this._analyze) { this._analyze(new Run(io, msg, this.binary)); io.send({ done: {} }); }
      else if (msg.cmd === "devices" && this._devices) io.send({ devices: this._devices() });
      else if (msg.cmd === "open" && this._open) this._serve(io, msg);
    } catch (e) {
      try { io.send({ error: String(e.message || e) }); } catch { /* gone */ }
      process.stderr.write(`${e.stack || e}\n`);
      process.exit(1);
    }
  }
  _serve(io, msg) {
    let opened;
    try { opened = this._open(msg.device || ""); } catch (e) { io.send({ error: String(e.message || e) }); return; }
    io.send({ opened: { size: opened.size, label: opened.label || msg.device || "", ...(this.binary && { binary: true }) } });
    for (;;) {
      const req = io.line();
      if (req === null) break;
      const r = req.read;
      if (!r) continue;
      const off = Number(r.offset), len = Number(r.len);
      let data;
      try { data = off < opened.size ? Buffer.from(opened.read(off, len)) : Buffer.alloc(0); } catch (e) { io.send({ ok: false, error: String(e.message || e) }); continue; }
      if (this.binary) { io.send({ ok: true, binary: data.length }); fs.writeSync(1, data); }
      else io.send({ ok: true, data: data.toString("base64") });
    }
    if (opened.close) opened.close();
  }
}

module.exports = { Plugin, Option, Device, Opened: (size, read, label, close) => ({ size, read, label, close }) };
