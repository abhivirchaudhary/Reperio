"use strict";

// ---------------------------------------------------------------------------
// Bridge to the Rust side
// ---------------------------------------------------------------------------

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const listen = (event, fn) => T.event.listen(event, (e) => fn(e.payload));
const fileUrl = (path) => T.core.convertFileSrc(path);
const thumbUrl = (id) =>
  window.__REPERIO_THUMB__ ? window.__REPERIO_THUMB__(id)
  : navigator.userAgent.includes("Windows") ? `http://thumb.localhost/${id}` : `thumb://localhost/${id}`;

// ---------------------------------------------------------------------------
// State and settings
// ---------------------------------------------------------------------------

const S = {
  view: "home",
  platform: "",
  disks: [],
  disksKey: "",
  pluginDevices: [],
  source: null,
  items: [],
  scan: { running: false, phase: "file_system", done: 0, total: 0, started: 0, deepStarted: 0, deepStartDone: 0 },
  filter: { kind: "all", origin: "lost", dir: null, year: null, size: null, goodOnly: false, grouped: false, q: "" },
  /// "Group similar photos": each file in a group, by id, to its group
  /// ({ best, members: [{ id, width, height }] }, best first).
  groupOf: new Map(),
  /// While the groups are being worked out: { done, total }.
  grouping: null,
  /// Of the groups in the list shown, the members shown, by the id of the
  /// one card that stands for them.
  stacks: new Map(),
  sort: "date",
  mode: "grid",
  selected: new Set(),
  touchedSelection: false,
  lastClicked: null,
  limit: 400,
  animate: true,
};

const SETTINGS_KEY = "reperio.settings";
const DEFAULTS = {
  theme: "system",
  motion: "system",
  textSize: 100,
  // Scanning
  scanFirst: "quick",
  autoDeep: true,
  offerResume: true,
  findPartitions: false,
  confirmStop: true,
  // Plug-ins: ask which to use before each scan, and the ones left unticked last time.
  askPlugins: true,
  pluginsOff: [],
  // Limits on plug-ins. Reperio puts none unless you do (0 is none).
  pluginIdleMin: 0,
  pluginMaxHours: 0,
  // Results
  view: "grid",
  sort: "date",
  showExisting: false,
  hideDamaged: false,
  autoSelect: true,
  // Recovery
  layout: "original",
  openWhenDone: false,
};
const settings = (() => {
  try { return { ...DEFAULTS, ...JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}") }; }
  catch { return { ...DEFAULTS }; }
})();

function applySettings() {
  const root = document.documentElement;
  if (settings.theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", settings.theme);
  MOTION.pref = settings.motion;
  root.classList.toggle("reduce-motion", MOTION.reduced);
  root.style.zoom = settings.textSize === 100 ? "" : String(settings.textSize / 100);
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch { /* private mode */ }
}

const KINDS = [
  ["photo", "Photos", "photo"],
  ["video", "Videos", "video"],
  ["raw", "RAW photos", "raw"],
  ["audio", "Audio", "audio"],
  ["document", "Documents", "doc"],
  ["other", "Other files", "file"],
];
const KIND_ICON = Object.fromEntries(KINDS.map(([k, , i]) => [k, i]));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function h(tag, props, ...children) {
  const [name, ...classes] = tag.split(".");
  const el = document.createElement(name || "div");
  if (classes.length) el.className = classes.join(" ");
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "on") for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
    else if (k === "class") el.className += " " + v;
    else if (k === "dataset") Object.assign(el.dataset, v);
    else if (k === "style" && typeof v === "object") Object.assign(el.style, v);
    else if (k === "text") el.textContent = v;
    else if (k === "title") { el.title = v; if (name === "button" && !el.hasAttribute("aria-label")) el.setAttribute("aria-label", v); }
    else if (k === "html") el.innerHTML = v;
    else if (k in el && typeof v !== "string") el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function icon(name, cls) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "i" + (cls ? " " + cls : ""));
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", "#i-" + name);
  svg.append(use);
  return svg;
}

function size(bytes) {
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let v = bytes, u = 0;
  while (v >= 1000 && u < units.length - 1) { v /= 1000; u++; }
  return u === 0 ? `${bytes} bytes` : `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[u]}`;
}

function parseDate(s) {
  if (!s) return null;
  const [d, t] = s.split("T");
  const [y, mo, da] = d.split("-").map(Number);
  const [hh, mi, ss] = (t || "0:0:0").split(":").map(Number);
  return new Date(y, mo - 1, da, hh, mi, ss);
}
const itemDate = (i) => i.taken || i.modified;
const fmtDate = new Intl.DateTimeFormat(undefined, { day: "numeric", month: "short", year: "numeric" });
/// What is being scanned, in words: "card" or "drive".
const medium = () => S.medium || "card";
const cap = (s) => s[0].toUpperCase() + s.slice(1);
const fmtDateTime = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const fmtMonth = new Intl.DateTimeFormat(undefined, { month: "long", year: "numeric" });
const plural = (n, one, many) => `${n.toLocaleString()} ${n === 1 ? one : many || one + "s"}`;

function duration(secs) {
  if (!isFinite(secs) || secs < 0) return "";
  if (secs < 60) return "under a minute";
  if (secs < 3600) return plural(Math.round(secs / 60), "minute");
  const hrs = Math.floor(secs / 3600), mins = Math.round((secs % 3600) / 60);
  return `${plural(hrs, "hour")} ${mins} min`;
}

function clock(secs) {
  const s = Math.floor(secs % 60), m = Math.floor(secs / 60) % 60, hr = Math.floor(secs / 3600);
  const pad = (n) => String(n).padStart(2, "0");
  return hr ? `${hr}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

let toastTimer;
function toast(msg, bad) {
  document.querySelector(".toast")?.remove();
  const el = h("div.toast" + (bad ? ".bad" : ""), { text: msg });
  document.body.append(el);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    el.classList.add("leaving");
    setTimeout(() => el.remove(), 300);
  }, bad ? 7000 : 3500);
}

/// A dimmed overlay holding one sheet. Returns { el, close }.
function overlay(sheet, { onClose } = {}) {
  const el = h("div.overlay", {}, sheet);
  document.body.append(el);
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    onClose?.();
    el.classList.add("closing");
    setTimeout(() => el.remove(), MOTION.reduced ? 0 : 220);
  };
  return { el, close };
}

function busy(title, sub, { onCancel, hintAfter, hint } = {}) {
  const hintEl = hint ? h("p", { text: hint, hidden: true }) : null;
  const o = overlay(h("div.sheet.busy-card", {},
    h("div.spinner"),
    h("h3", { text: title }),
    sub && h("p", { text: sub }),
    hintEl,
    onCancel && h("button.btn", { on: { click: onCancel } }, "Cancel")));
  o.el.id = "busy";
  const timer = hintEl ? setTimeout(() => (hintEl.hidden = false), hintAfter || 20000) : null;
  return () => { clearTimeout(timer); o.close(); };
}

const errText = (e) => (typeof e === "string" ? e : e?.message || String(e));
const app = () => document.getElementById("app");

/// Replaces an element's children, skipping the false and null left by
/// conditional parts.
function setKids(el, ...kids) {
  el.replaceChildren(...kids.flat(Infinity).filter((k) => k != null && k !== false));
}

function show(view) {
  app().replaceChildren(view);
}

function settingsButton() {
  return h("button.btn.icon.ghost", { title: "Settings", on: { click: () => openSettings() } }, icon("gear"));
}

// ---------------------------------------------------------------------------
// Home: choose a drive
// ---------------------------------------------------------------------------

async function refreshDisks() {
  let disks;
  try { disks = await invoke("list_disks"); } catch { disks = []; }
  refreshPluginDevices();
  if (typeof refreshIos === "function") refreshIos();
  const key = JSON.stringify(disks) + JSON.stringify(S.pluginDevices);
  if (key === S.disksKey) return;
  S.disks = disks;
  S.disksKey = key;
  if (S.view === "home") updateDisks();
}

/// What the source plug-ins can reach. Asked at most every 8 seconds, and
/// never twice at once: it starts each plug-in program.
let pluginDevicesAt = 0;
let pluginDevicesBusy = false;
async function refreshPluginDevices() {
  if (pluginDevicesBusy || Date.now() - pluginDevicesAt < 8000) return;
  pluginDevicesBusy = true;
  try {
    const list = await invoke("plugin_devices");
    if (JSON.stringify(list) !== JSON.stringify(S.pluginDevices)) {
      S.pluginDevices = list || [];
      S.disksKey = "";
      if (S.view === "home") updateDisks();
    }
  } catch { /* no plug-in devices */ }
  pluginDevicesAt = Date.now();
  pluginDevicesBusy = false;
}

const deviceIcon = (kind) => ({ phone: "phone", camera: "raw", network: "drive", drive: "drive", usb: "usb", card: "card" })[kind] || "disc";
const deviceKindLabel = (kind) => ({ phone: "Phone", camera: "Camera", network: "Network", drive: "Drive", usb: "USB drive", card: "Memory card" })[kind] || "Device";

/// A device reached by a plug-in: scanned and recovered like a card.
function pluginDeviceButton(d, i) {
  const meta = [deviceKindLabel(d.kind), d.size ? size(d.size) : null, d.detail].filter(Boolean);
  return h("div.disk-slot", {},
    h("button.disk", { style: { animationDelay: `${80 + i * 60}ms` }, on: { click: () => openPluginDevice(d) } },
      h("div.dicon", {}, icon(deviceIcon(d.kind))),
      h("div.dtext", {}, h("div.dname", { text: d.name }), h("div.dmeta", { text: meta.join(" · ") }), h("div.dvia", { text: `via ${d.plugin}` })),
      icon("next", "go")));
}

async function openPluginDevice(d) {
  S.medium = d.kind === "card" ? "card" : d.kind === "phone" ? "phone" : "device";
  const done = busy(`Opening ${d.name}`, `The ${d.plugin} plug-in is connecting. You may need to approve this computer on the device.`);
  try {
    S.source = await invoke("open_plugin_device", { plugin: d.plugin, id: d.id });
    done();
    beginScan();
  } catch (e) {
    done();
    const msg = errText(e);
    if (msg !== "Cancelled.") toast(msg, true);
  }
}

const diskIcon = (kind) => ({ card: "card", usb: "usb", image: "disc" })[kind] || "drive";
const diskKindLabel = (kind) => ({ card: "Memory card", usb: "USB drive", external: "External drive", internal: "Internal drive" })[kind] || "Disk";

function diskButton(d, i) {
  const meta = [diskKindLabel(d.kind), size(d.size)];
  const vols = d.volumes.filter((v) => v !== d.name);
  if (vols.length) meta.unshift(vols.join(", "));
  // The card, with a small link to the drive's health beside its arrow.
  return h("div.disk-slot", {},
    h("button.disk" + (d.kind === "internal" ? ".internal" : ""), { style: { animationDelay: `${80 + i * 60}ms` }, on: { click: () => openDisk(d) } },
      h("div.dicon", {}, icon(diskIcon(d.kind))),
      h("div.dtext", {}, h("div.dname", { text: d.name }), h("div.dmeta", { text: meta.join(" · ") })),
      icon("next", "go")),
    h("button.disk-health", { title: `Drive health of ${d.name}`, style: { animationDelay: `${80 + i * 60}ms` }, on: { click: () => openDriveHealth(d) } }, icon("pulse")));
}

function renderHome() {
  S.view = "home";
  show(h("div.view", {},
    h("div.toolbar", {},
      h("div.wordmark", {}, icon("logo"), h("span.name", { text: "Reperio" }), h("span.by", { text: "Made by Abhivir" })),
      h("div.spacer"),
      settingsButton()),
    h("div.home", {},
      h("div.home-inner", {},
        h("header.welcome", {},
          h("div.hero-mark", { "aria-hidden": "true" }, icon("logo")),
          h("h1", { text: "Let's get your files back" }),
          h("p", { text: "Choose where they were. Reperio only reads from it, so nothing on it is ever changed." })),
        h("div", { id: "disk-list" }),
        h("div.home-more", {},
          h("button.chip", { on: { click: openImage } }, icon("disc"), "Open a disk image"),
          h("button.chip", { on: { click: openRaid } }, icon("drive"), "Combine drives (RAID)…"),
          h("button.chip", { on: { click: () => openIphone() } }, icon("phone"), "iPhone / iPad…"),
          h("button.chip", { on: { click: openAndroid } }, icon("phone"), "Android phone…"),
          h("button.chip", { on: { click: openTools } }, icon("open"), "Run a plug-in…"),
          h("button.chip", { on: { click: openResults } }, icon("open"), "Open a saved scan")),
        h("p.home-tip", { text: "Until your files are back, try not to save anything new onto the card or drive." })))));
  updateDisks();
}

function updateDisks() {
  const list = document.getElementById("disk-list");
  if (!list) return;
  const removable = S.disks.filter((d) => d.kind !== "internal");
  const internal = S.disks.filter((d) => d.kind === "internal");
  setKids(list,
    h("div.section-label", { text: "Cards and drives" }),
    removable.length
      ? h("div.disks", {}, removable.map(diskButton))
      : h("div.plug", {},
        h("div.plug-icons", {}, ["card", "usb", "drive"].map((ic) => h("span", {}, icon(ic)))),
        h("b", { text: "Connect the card or drive" }),
        h("p", { text: "Memory cards, USB sticks and external SSDs or hard drives show up here as soon as they are plugged in." })),
    typeof iosHomeSection === "function" && iosHomeSection(),
    S.pluginDevices.length > 0 && [
      h("div.section-label", { text: "Other devices" }),
      h("div.disks", {}, S.pluginDevices.map((d, i) => pluginDeviceButton(d, i + removable.length))),
      h("p.fine", { text: "Reached by your source plug-ins. Reperio reads them the same way, and never writes to them." }),
    ],
    internal.length > 0 && [
      h("div.section-label", { text: "This computer" }),
      h("div.disks", {}, internal.map((d, i) => diskButton(d, i + removable.length))),
      S.platform === "macos" && h("p.fine", { text: "Files deleted from a Mac's internal SSD can rarely be recovered by any app: it is encrypted and clears freed space automatically. Check the Trash and Time Machine first." }),
    ]);
}

async function openDisk(d) {
  S.medium = d.kind === "card" ? "card" : "drive";
  const done = busy(`Opening ${d.name}`, `You may be asked for your password, so Reperio can read the ${medium()} directly.`, {
    onCancel: () => invoke("cancel_open"),
    hint: "No password window? It may be behind this one: press ⌘-Tab or check the Dock.",
  });
  try {
    S.source = await invoke("open_disk", { id: d.id });
    done();
    beginScan();
    invoke("disk_health", { id: d.id }).then((hl) => {
      if (hl.status === "failing") toast(`${d.name} reports that it is failing (${hl.detail}). Back it up first, then scan the copy.`, true);
    }).catch(() => {});
  } catch (e) {
    done();
    const msg = errText(e);
    if (msg.startsWith("Full Disk Access needed.")) showFullDiskAccess(msg);
    else if (msg !== "Cancelled.") toast(msg, true);
  }
}

/// macOS blocks raw access to cards until the user allows it, once.
function showFullDiskAccess(msg) {
  const detail = (msg || "").match(/\(([^)]*)\)\s*$/)?.[1] || "";
  const o = overlay(h("div.sheet", {},
    h("h2", { text: "Allow Reperio to read cards" }),
    h("p.lead", { text: "macOS protects memory cards. Before any recovery app can read one directly, you allow it once." }),
    h("ol.steps", {},
      h("li", {}, "Click ", h("b", { text: "Open Privacy Settings" }), "."),
      h("li", {}, "Turn on ", h("b", { text: "Reperio" }), ". If it isn't listed, click ", h("b", { text: "+" }), " and choose Reperio in Applications."),
      h("li", {}, "Quit Reperio (⌘Q), open it again, and choose the card.")),
    h("p.fine", { text: "After installing a new build of Reperio, remove it from the list and add it again: macOS ties the permission to each build." }),
    detail && h("p.fine", { text: detail }),
    h("div.actions", {},
      h("button.btn", { on: { click: () => o.close() } }, "Close"),
      h("button.btn.primary", { on: { click: () => invoke("open_full_disk_access") } }, "Open Privacy Settings"))));
}

async function openImage() {
  try {
    const src = await invoke("open_image");
    if (src) { S.source = src; S.medium = null; beginScan(); }
  } catch (e) {
    toast(errText(e), true);
  }
}

// ---------------------------------------------------------------------------
// Android phones
// ---------------------------------------------------------------------------

/// What a phone still holds after files were deleted from it, over USB. Two
/// ways in: copy what Android keeps (trash, thumbnails), which needs nothing
/// but USB debugging, and an image of a rooted phone, which can then be
/// scanned like any disk. Most phones are encrypted, and the screen says so.
function openAndroid() {
  const a = { adbError: null, devices: [], serial: null, facts: null, factsFor: null, factsError: null,
    step: "pick", job: null, error: null, everything: false, anyway: false, what: "whole", analyzers: null, picked: null, reports: null, prog: null, result: null, last: "" };
  const sheet = h("div.sheet.wide.android");
  let timer = null, unlisten = null, closed = false;
  const stop = () => { clearInterval(timer); unlisten?.(); unlisten = null; };
  const o = overlay(sheet, { onClose: () => { closed = true; stop(); document.removeEventListener("keydown", onKey, true); } });
  const onKey = (e) => { if (e.key === "Escape" && a.step !== "working") o.close(); };
  document.addEventListener("keydown", onKey, true);

  async function poll() {
    if (a.step !== "pick") return;
    let d;
    try { d = await invoke("android_devices"); } catch { return; }
    if (closed || a.step !== "pick") return;
    a.adbError = d.adb_error;
    a.devices = d.devices;
    const ready = d.devices.find((x) => !x.problem);
    if (!ready) { a.serial = null; a.facts = null; a.factsFor = null; a.factsError = null; }
    else if (a.factsFor !== ready.serial) {
      a.serial = a.factsFor = ready.serial;
      a.facts = null;
      a.factsError = null;
      invoke("android_analyzers").then((l) => { a.analyzers = l; a.picked = new Set(l.map((x) => x.name)); render(); }).catch(() => { a.analyzers = []; });
      invoke("plugins_info").then((i) => { a.unfit = i.plugins.filter((p) => p.kind !== "source" && fitFor(p, "android") !== null); render(); }).catch(() => {});
      invoke("android_facts", { serial: ready.serial })
        .then((f) => { if (a.factsFor === ready.serial) { a.facts = f; render(); } })
        .catch((e) => { a.factsError = errText(e); render(); });
    }
    render();
  }

  function render() {
    const key = JSON.stringify([a.step, a.adbError, a.devices, a.facts, a.factsError, a.error, a.everything, a.anyway, a.what, a.result, a.analyzers, a.unfit, a.picked && [...a.picked], a.reports]);
    if (key === a.last) return;
    a.last = key;
    setKids(sheet, ({ pick: pickView, working: workingView, "done-pull": pulledView, "done-image": imagedView, "done-all": allView, "done-analysis": analysisView })[a.step]());
  }

  const close = h("button.btn", { on: { click: () => o.close() } }, "Close");

  function pickView() {
    const head = h("h2", { text: "Android phone" });
    if (a.adbError) {
      const cmd = "brew install android-platform-tools";
      return [head,
        h("p.lead", { text: "Reperio talks to the phone through a free tool from Google called adb. It isn't installed on this computer yet." }),
        h("ol.steps", {},
          h("li", {}, S.platform === "macos" ? ["Open ", h("b", { text: "Terminal" }), " and run:"] : "Install it with your package manager, for example:"),
          h("li.code", {}, h("code", { text: cmd }), h("button.btn", { on: { click: () => { navigator.clipboard?.writeText(cmd); toast("Copied."); } } }, icon("copy"), "Copy")),
          h("li", {}, "Come back here. Reperio checks again by itself.")),
        h("p.fine", { text: "No Homebrew? Download \"SDK Platform-Tools\" from developer.android.com/tools/releases/platform-tools and unzip it into your home folder as \"platform-tools\"." }),
        h("div.actions", {}, close)];
    }
    const phone = a.devices.find((d) => !d.problem);
    if (!phone) {
      const stuck = a.devices.find((d) => d.problem);
      return [head,
        h("p.lead", { text: stuck ? stuck.problem : "Connect the phone with its USB cable." }),
        !stuck && h("ol.steps", {},
          h("li", {}, "On the phone: Settings → About phone, then tap ", h("b", { text: "Build number" }), " seven times."),
          h("li", {}, "Settings → System → Developer options → turn on ", h("b", { text: "USB debugging" }), "."),
          h("li", {}, "Unlock the phone and tap ", h("b", { text: "Allow" }), " when it asks about this computer.")),
        h("div.listening", {}, h("div.spinner"), h("span", { text: "Looking for a phone…" })),
        h("p.fine", { text: "Reperio only reads from the phone. Nothing on it is changed." }),
        h("div.actions", {}, close)];
    }
    if (a.factsError) return [head, h("p.lead", { text: a.factsError }), h("div.actions", {}, close)];
    if (!a.facts) return [head, h("div.listening", {}, h("div.spinner"), h("span", { text: "Reading the phone…" })), h("div.actions", {}, close)];
    const f = a.facts;
    const chip = (text, tone) => h("span.chip-tag" + (tone ? "." + tone : ""), { text });
    const canImage = f.root && imageChoices(f).length > 0;
    return [head,
      h("div.phone", {},
        h("div.dicon", {}, icon("phone")),
        h("div", {},
          h("b", { text: f.name || "Android phone" }),
          h("span", { text: `Android ${f.android}${f.patch ? ` · security patch ${f.patch}` : ""}` })),
        h("div.tags", {},
          chip(f.encrypted ? "Encrypted" : "Not encrypted", f.encrypted ? "warn" : "good"),
          chip(f.root ? "Rooted" : "Not rooted"))),
      h("p.lead", { text: f.verdict }),
      a.error && h("div.error", { text: a.error }),
      h("div.a-cards", {},
        h("div.a-card", {},
          h("b", { text: "Bring back what the phone kept" }),
          h("p", { text: "Android keeps deleted photos, videos and files in a trash for 30 days, and leaves small previews of photos. This copies them to your computer and puts the trashed ones back under their own names. Works on any phone." }),
          h("label.check", {}, h("input", { type: "checkbox", checked: a.everything, on: { change: (e) => { a.everything = e.target.checked; render(); } } }),
            h("span", { text: "Also copy everything in the phone's storage" })),
          h("button.btn.primary", { on: { click: pull } }, "Choose a folder and copy")),
        h("div.a-card" + (canImage ? "" : ".off"), {},
          h("b", { text: "Make a full image of the phone" }),
          h("p", { text: f.root
            ? `Copies the phone's storage byte for byte, read-only, with fingerprints to prove it is exact. Then it can be scanned like any drive.${f.encrypted ? " This phone is encrypted, so the image would be unreadable." : ""}`
            : "Needs a rooted phone, and this one isn't. Rooting can erase a phone, so don't do it to one you are recovering from." }),
          f.root && h("div.choices.tight", {}, imageChoices(f).map(([k, title, sub]) =>
            h("div.choice" + (a.what === k ? ".on" : ""), { on: { click: () => { a.what = k; render(); } } },
              h("div.radio"), h("div", {}, h("b", { text: title }), h("span", { text: sub }))))),
          f.root && f.encrypted && h("label.check", {}, h("input", { type: "checkbox", checked: a.anyway, on: { change: (e) => { a.anyway = e.target.checked; render(); } } }),
            h("span", { text: "Copy it anyway (it can't be scanned)" })),
          h("button.btn", { disabled: !canImage || (f.encrypted && !a.anyway), on: { click: image } }, "Choose a folder and image"))),
      h("div.a-card", {},
        h("b", { text: "Forensic plug-ins" }),
        h("p", { text: "Your own analysis code, loaded from the plug-ins folder. Reperio does not limit what a plug-in does: it can run any shell, root or adb command on the phone, so use only code you trust. Everything it asks is written to an audit log beside its report." }),
        a.analyzers && a.analyzers.length > 0 && h("div.a-plugins", {}, a.analyzers.map((p) =>
          h("label.check", {}, h("input", { type: "checkbox", checked: a.picked?.has(p.name), on: { change: (e) => { e.target.checked ? a.picked.add(p.name) : a.picked.delete(p.name); render(); } } }),
            h("span", {}, h("b", { text: p.name }), p.description ? ` · ${p.description}` : "")))),
        a.unfit?.length > 0 && h("div.a-unfit", {}, h("div.section-label", { text: "Installed, but not for a phone" }),
          a.unfit.map((p) => h("div.pitem.off", {}, h("div", {}, h("b", { text: p.name }), h("span", { text: fitFor(p, "android") }))))),
        a.analyzers && a.analyzers.length === 0 && h("p.fine", { text: "No forensic plug-ins installed yet. Put one in the plug-ins folder (see docs/forensic-plugins.md); examples are in plugins/examples." }),
        a.analyzers && a.analyzers.length > 0
          ? h("button.btn", { disabled: !a.picked?.size, on: { click: analyze } }, "Choose a folder and run")
          : h("button.btn", { on: { click: () => invoke("open_plugins_folder").catch((e) => toast(errText(e), true)) } }, icon("folder"), "Open plug-ins folder")),
      h("p.fine", { text: "Not there? Also check Google Photos' Bin (60 days), the gallery app's Recycle bin, Files by Google's Trash, and your cloud backups." }),
      h("div.actions", {}, close)];
  }

  /// What can be imaged on this phone: [key, title, description], best first.
  function imageChoices(f) {
    const out = [];
    const total = (f.partitions || []).reduce((n, p) => n + p.size, 0);
    if (f.storage) out.push(["whole", "The whole storage chip", `${size(f.storage.size)}. Everything in one file, partition table included. Recommended.`]);
    if (f.partitions?.length) out.push(["all", "Every partition, one file each", `${f.partitions.length} files, ${size(total)}, with a list of fingerprints.`]);
    if (f.userdata_size) out.push(["userdata", "User data only", `${size(f.userdata_size)}. Photos, apps and settings, without the system.`]);
    if (!out.some((c) => c[0] === a.what) && out.length) a.what = out[0][0];
    return out;
  }

  const imageSize = () => ({ whole: a.facts.storage?.size, userdata: a.facts.userdata_size,
    all: (a.facts.partitions || []).reduce((n, p) => n + p.size, 0) })[a.what] || 0;

  function workingView() {
    const what = a.job === "pull" ? "Copying from the phone" : a.job === "analysis" ? "Running forensic plug-ins" : "Imaging the phone";
    return [h("h2", { text: what }),
      h("p.lead", { text: a.job === "image" || a.job === "all" ? "Keep the phone connected and unlocked. This can take a long time." : "Keep the phone connected and unlocked." }),
      h("div.progress", {}, h("div", { id: "a-bar" })),
      h("div.progress-text", {}, h("span", { id: "a-name", text: "" }), h("span", { id: "a-pct", text: "" })),
      h("div.actions", {}, h("button.btn", { on: { click: () => invoke("android_cancel") } }, "Stop"))];
  }

  function updateProgress() {
    const p = a.prog;
    const bar = document.getElementById("a-bar");
    if (!bar || !p) return;
    const pct = p.total ? Math.min(100, Math.floor((p.done / p.total) * 100)) : 0;
    bar.style.width = pct + "%";
    document.getElementById("a-pct").textContent = (a.job === "image" || a.job === "all") ? `${pct}% · ${size(p.done)} of ${size(p.total)}` : (p.total ? `${p.done} of ${p.total} files` : "");
    document.getElementById("a-name").textContent = p.name ? (a.job === "all" ? `Partition ${p.name}` : p.name) : "";
  }

  const doneIcon = () => h("div.done-icon", { html: '<svg viewBox="0 0 76 76"><circle cx="38" cy="38" r="30"/><path d="M26 39l8.5 8.5L51 31"/></svg>' });

  function pulledView() {
    const r = a.result;
    const found = r.trash + r.thumbnails + r.gallery_trash;
    if (!found) {
      return [h("h2", { text: "Nothing was left on the phone" }),
        h("p.lead", { text: "The phone's trash and thumbnail folders are empty, so Android no longer has these files. Check Google Photos' Bin, the gallery's Recycle bin and your cloud backups next." }),
        h("div.actions", {}, close)];
    }
    return [h("div.center", {}, doneIcon()),
      h("h2", { text: r.trash ? `${plural(r.trash, "file")} put back` : `${plural(found, "file")} copied` }),
      h("p.lead", { text: [
        r.trash ? `Trashed files are back under their own names.${r.expires ? ` Android deletes the oldest for good on ${r.expires}.` : ""}` : "",
        r.gallery_trash ? `${plural(r.gallery_trash, "file")} came from gallery trash folders.` : "",
        r.thumbnails ? `Also kept: ${plural(r.thumbnails, "small preview")} of photos (low quality, some of deleted ones).` : "",
        r.failed ? `${plural(r.failed, "file")} could not be copied.` : "",
      ].filter(Boolean).join(" ") }),
      r.restored.length > 0 && h("ul.a-list", {}, r.restored.slice(0, 8).map((n) => h("li", { text: n })),
        r.restored.length > 8 && h("li.more", { text: `and ${r.restored.length - 8} more` })),
      h("div.actions", {},
        h("button.btn.primary", { on: { click: () => invoke("reveal", { path: r.trash ? r.restored_dir : r.left_dir }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
        close)];
  }

  function imagedView() {
    const r = a.result;
    return [h("div.center", {}, doneIcon()),
      h("h2", { text: r.complete ? "The phone is imaged" : "The image is incomplete" }),
      h("p.lead", { text: r.complete
        ? `Saved ${size(r.received)}. A report with fingerprints (MD5 and SHA-256) is saved beside it, to show the image is exact.`
        : `Only ${size(r.received)} of ${size(r.size)} arrived. Check the cable and try again.` }),
      h("p.fine", { text: r.path }),
      h("div.actions", {},
        r.received > 0 && h("button.btn.primary", { on: { click: () => scanImage() } }, "Scan the image"),
        h("button.btn", { on: { click: () => invoke("reveal", { path: r.path.replace(/\/[^/]*$/, "") }) } }, icon("folder"), "Show the file"),
        close)];
  }

  function analysisView() {
    const reps = a.reports;
    return [h("h2", { text: reps.length === 1 ? `${reps[0].plugin}` : `${reps.length} plug-ins ran` }),
      h("p.lead", { text: "Findings, the files each plug-in wrote, and an audit log of everything it asked the phone are saved in the folder below, with fingerprints (SHA-256)." }),
      reportsList(reps),
      h("div.actions", {},
        h("button.btn.primary", { on: { click: () => invoke("reveal", { path: a.analysisDir }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
        close)];
  }

  function allView() {
    const r = a.result;
    const ok = !r.failed.length;
    return [h("div.center", {}, doneIcon()),
      h("h2", { text: ok ? `${plural(r.done, "partition")} imaged` : `${plural(r.done, "partition")} imaged, ${r.failed.length} not complete` }),
      h("p.lead", { text: `Saved ${size(r.bytes)}, one file per partition. MANIFEST.txt beside them lists each one's size and fingerprints (MD5 and SHA-256).${ok ? "" : ` Not complete: ${r.failed.join(", ")}.`}` }),
      h("p.fine", { text: r.dir }),
      h("div.actions", {},
        !r.failed.includes("userdata") && h("button.btn.primary", { on: { click: () => scanImage(`${r.dir}/userdata.img`) } }, "Scan the data partition"),
        h("button.btn", { on: { click: () => invoke("reveal", { path: r.dir }) } }, icon("folder"), "Show the folder"),
        close)];
  }

  const stamp = () => new Date().toISOString().slice(0, 16).replace(/[:T]/g, "-");

  async function chooseFolder() {
    try { return await invoke("choose_destination"); }
    catch (e) { a.error = errText(e); render(); return null; }
  }

  async function run(job, folder, call) {
    a.error = null;
    a.job = job;
    a.step = "working";
    a.prog = { done: 0, total: 0, name: "" };
    render();
    unlisten = await listen("android-progress", (p) => { a.prog = p; updateProgress(); });
    try {
      const result = await call();
      unlisten?.(); unlisten = null;
      if (result.cancelled || (Array.isArray(result) && result.some((r) => r.cancelled))) { a.step = "pick"; a.last = ""; toast("Stopped."); render(); return; }
      a.result = result;
      a.step = ({ image: "done-image", all: "done-all", analysis: "done-analysis" })[job] || "done-pull";
      if (job === "analysis") a.reports = result;
    } catch (e) {
      unlisten?.(); unlisten = null;
      a.step = "pick";
      a.error = errText(e);
    }
    a.last = "";
    render();
  }

  async function pull() {
    const d = await chooseFolder();
    if (!d) return;
    run("pull", d.path, () => invoke("android_pull", { serial: a.serial, dest: `${d.path}/Android recovery ${stamp()}`, everything: a.everything }));
  }

  async function analyze() {
    const d = await chooseFolder();
    if (!d) return;
    const dest = `${d.path}/Android analysis ${stamp()}`;
    a.analysisDir = dest;
    run("analysis", d.path, () => invoke("android_analyze", { serial: a.serial, dest, names: [...a.picked] }));
  }

  async function image() {
    const d = await chooseFolder();
    if (!d) return;
    const need = imageSize();
    if (d.free != null && need && d.free < need * 1.02) {
      a.error = `There isn't enough room there: the image needs ${size(need)} and ${size(d.free)} is free. Choose another drive.`;
      a.last = ""; render();
      return;
    }
    if (a.what === "all") run("all", d.path, () => invoke("android_image_all", { serial: a.serial, dir: d.path, evenIfEncrypted: a.anyway }));
    else run("image", d.path, () => invoke("android_image", { serial: a.serial, dir: d.path, what: a.what, evenIfEncrypted: a.anyway }));
  }

  async function scanImage(path = a.result.path) {
    try {
      const src = await invoke("open_image_path", { path });
      o.close();
      S.source = src;
      S.medium = null;
      beginScan();
    } catch (e) {
      toast(errText(e), true);
    }
  }

  render_first();
  function render_first() { a.last = "x"; a.step = "pick"; poll(); render(); timer = setInterval(poll, 2500); }
}

/// Drives from a NAS, a RAID enclosure or a striped set, put back together
/// as one drive. The layout is read from the drives or worked out; the user
/// only ticks the drives and checks what Reperio found.
function openRaid() {
  const ticked = new Set();
  let found = null, seq = 0, pick = 0, busyOpening = false;
  const list = h("div.raid-list");
  const result = h("div.raid-result");
  const go = h("button.btn.primary", { disabled: true, on: { click: start } }, "Scan combined drive");
  const o = overlay(h("div.sheet", {},
    h("h2", { text: "Combine drives (RAID)" }),
    h("p.lead", { text: "Tick every drive from the set, in any order. Reperio works out how they were arranged, then scans them as one drive. A drive that is missing is fine for RAID 5. Nothing on them is changed." }),
    list, result,
    h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Cancel"), go)));

  for (const d of S.disks) {
    const box = h("input", { type: "checkbox", on: { change: () => { box.checked ? ticked.add(d.id) : ticked.delete(d.id); detect(); } } });
    list.append(h("label.raid-row", {}, box, h("span.rname", { text: d.name }), h("span.rmeta", { text: [d.volumes.join(", "), size(d.size)].filter(Boolean).join(" · ") })));
  }
  if (!S.disks.length) list.append(h("p.fine", { text: "No drives are connected. Plug in the drives from the set." }));

  const sure = (c) => (c >= 0.8 ? "Confident" : c >= 0.5 ? "Likely" : "Unsure");

  function show() {
    if (!found) return;
    const c = found.candidates[pick];
    setKids(result,
      found.candidates.length > 1 && h("select", { on: { change: (e) => { pick = +e.target.value; show(); } } },
        found.candidates.map((x, i) => h("option", { value: i, selected: i === pick, text: `${x.description} (${sure(x.confidence)})` }))),
      c && h("p.raid-best", {}, h("b", { text: c.description }), ` · ${sure(c.confidence)}`),
      c && c.found.length > 0 && h("p.fine", { text: `Holds ${c.found.join("; ")}.` }),
      c && !c.found.length && h("p.fine", { text: "No file system was recognised on it. It may still hold files a deep scan can find." }),
      c && c.notes.map((n) => h("p.fine", { text: n })),
      found.notes.map((n) => h("p.fine", { text: n })),
      !c && h("p.fine", { text: "Reperio could not work out how these drives fit together. Check that every drive from the set is ticked." }));
    go.disabled = !c || busyOpening;
  }

  async function detect() {
    const mine = ++seq;
    found = null;
    go.disabled = true;
    if (!ticked.size) { setKids(result); return; }
    setKids(result, h("p.fine", { text: "Looking at the drives…" }));
    try {
      const r = await invoke("detect_raid", { ids: [...ticked] });
      if (mine !== seq) return;
      found = r; pick = 0; show();
    } catch (e) {
      if (mine === seq) setKids(result, h("p.fine", { text: errText(e) }));
    }
  }

  async function start() {
    busyOpening = true; go.disabled = true;
    const c = found.candidates[pick];
    try {
      S.source = await invoke("open_raid", { ids: [...ticked], layout: c.layout });
      S.medium = "drive";
      o.close();
      beginScan();
    } catch (e) {
      busyOpening = false; go.disabled = false;
      toast(errText(e), true);
    }
  }
}

async function openResults() {
  const done = busy("Opening saved scan");
  try {
    const loaded = await invoke("open_results");
    done();
    if (!loaded) return;
    S.source = loaded.source;
    S.medium = null;
    S.items = loaded.items;
    S.volumes = loaded.volumes;
    S.freeSpace = loaded.free_space;
    S.scan = { running: false, deep: true };
    resetView();
    selectDefaults();
    renderResults();
  } catch (e) {
    done();
    toast(errText(e), true);
  }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// When a vault record was last refreshed, in words.
function ago(secs) {
  if (!secs) return "not yet";
  const m = Math.round((Date.now() / 1000 - secs) / 60);
  if (m < 1) return "just now";
  if (m < 60) return plural(m, "minute") + " ago";
  if (m < 48 * 60) return plural(Math.round(m / 60), "hour") + " ago";
  return fmtDate.format(new Date(secs * 1000));
}

/// The Recovery Vault part of Settings: one switch per drive.
function vaultSection() {
  const list = h("div.vault-list", {}, h("div.fine", { text: "Looking for drives…" }));
  const load = async () => {
    let drives = [];
    try { drives = await invoke("vault_drives"); } catch { /* shown as none */ }
    setKids(list, drives.length
      ? drives.map((d) => h("div.vault-drive" + (d.mounted ? "" : ".away"), {},
        h("div.what", {},
          h("b", { text: d.label }),
          h("span", { text: d.protected
            ? `Protected · ${plural(d.files, "file")} recorded · updated ${ago(d.updated)}${d.mounted ? "" : " · not connected"}`
            : "Not protected" })),
        h("button.switch" + (d.protected ? ".on" : ""), {
          title: `Protect ${d.label}`, role: "switch", "aria-checked": String(d.protected),
          disabled: !d.mounted && !d.protected,
          on: { click: async (e) => {
            e.currentTarget.disabled = true;
            try { await invoke("vault_protect", { id: d.id, on: !d.protected }); }
            catch (err) { toast(errText(err), true); }
            load();
          } },
        })))
      : h("div.fine", { text: "Connect a memory card or drive to protect it." }));
  };
  load();
  return h("div.vault", {},
    h("div.what", {},
      h("b", { text: "Recovery Vault" }),
      h("span", { text: "Reperio keeps a small record of a protected drive's files: names, folders and a fingerprint of each. If they are ever deleted or the drive is formatted, files found by the deep scan get their names back. The record refreshes whenever the drive is connected." })),
    list);
}

/// One row of a settings page: what it is, and the control for it.
function settingRow(title, desc, control) {
  return h("div.setting", {},
    h("div.what", {}, h("b", { text: title }), desc && h("span", { text: desc })),
    control);
}

const PAGES = [
  ["general", "General", "gear"],
  ["scanning", "Scanning", "scan"],
  ["results", "Results", "grid"],
  ["recovery", "Recovery", "save"],
  ["privacy", "Access and storage", "key"],
  ["plugins", "Plug-ins", "open"],
  ["about", "About", "info"],
];

function openSettings(page = "general") {
  const body = h("div.settings-body");
  const nav = h("nav.settings-nav", { "aria-label": "Settings" });
  let current = page;
  const save = () => { applySettings(); };
  const toggle = (key, after) => h("button.switch" + (settings[key] ? ".on" : ""), {
    role: "switch", "aria-checked": String(!!settings[key]), title: key,
    on: { click: (e) => {
      settings[key] = !settings[key];
      e.currentTarget.classList.toggle("on", settings[key]);
      e.currentTarget.setAttribute("aria-checked", String(settings[key]));
      save(); after?.();
    } },
  });
  const choice = (key, options, after) => h("div.seg.text", {}, options.map(([value, label, ic]) =>
    h("button" + (settings[key] === value ? ".on" : ""), {
      title: label,
      on: { click: (e) => {
        settings[key] = value;
        e.currentTarget.parentElement.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b === e.currentTarget));
        save(); after?.();
      } },
    }, ic ? icon(ic) : label)));
  const select = (key, options, after) => h("select.select", {
    on: { change: (e) => { settings[key] = e.target.value; save(); after?.(); } },
  }, options.map(([value, label]) => h("option", { value, selected: String(settings[key]) === String(value), text: label })));

  const pages = {
    general: () => [
      settingRow("Appearance", "Automatic follows your system.",
        choice("theme", [["system", "Automatic", "auto"], ["light", "Light", "sun"], ["dark", "Dark", "moon"]])),
      settingRow("Text size", "Makes everything in Reperio larger or smaller.",
        select("textSize", [[90, "Small"], [100, "Default"], [112, "Large"], [125, "Extra large"]], () => { settings.textSize = Number(settings.textSize); applySettings(); })),
      settingRow("Reduce motion", "Keep every update, without the movement.",
        h("button.switch" + (MOTION.reduced ? ".on" : ""), { role: "switch", "aria-checked": String(MOTION.reduced), title: "Reduce motion", on: { click: (e) => { settings.motion = MOTION.reduced ? "full" : "reduced"; applySettings(); e.currentTarget.classList.toggle("on", MOTION.reduced); e.currentTarget.setAttribute("aria-checked", String(MOTION.reduced)); } } })),
    ],
    scanning: () => [
      settingRow("Start with", "A quick scan reads the drive's own list of deleted files, with their names, in minutes. A deep scan searches every part of the drive.",
        choice("scanFirst", [["quick", "Quick scan"], ["deep", "Deep scan"]])),
      settingRow("Then search deeper automatically", "When the quick scan finds nothing deleted, go straight on to the deep scan.", toggle("autoDeep")),
      settingRow("Always look for lost partitions", "Also searches the whole drive for volumes the drive no longer lists. Slower, but finds drives that were repartitioned or formatted.", toggle("findPartitions")),
      settingRow("Offer to continue a stopped scan", "If a scan of the same drive stopped partway, ask whether to pick up from there.", toggle("offerResume")),
      settingRow("Ask before stopping a scan", null, toggle("confirmStop")),
      settingRow("Choose plug-ins before each scan", "When you have plug-ins installed, a card asks which to use. Turn off to reuse your last choice.", toggle("askPlugins")),
    ],
    results: () => [
      settingRow("Show files as", null, choice("view", [["grid", "Thumbnails", "grid"], ["list", "List", "list"]], () => { S.mode = settings.view; })),
      settingRow("Sort by", null, select("sort", [["date", "Newest first"], ["name", "Name"], ["size", "Largest first"], ["chance", "Best chance first"]], () => { S.sort = settings.sort; })),
      settingRow("Include files that are still on the drive", "Normally the list shows only deleted files.", toggle("showExisting")),
      settingRow("Hide damaged files", "Leaves out files that are partly overwritten or cannot be opened.", toggle("hideDamaged")),
      settingRow("Select good files automatically", "After a scan, every deleted file in good condition is ticked for you.", toggle("autoSelect")),
      h("p.fine", { text: "These take effect on the next scan." }),
    ],
    recovery: () => [
      settingRow("Arrange recovered files", "You can still change this each time you recover.",
        select("layout", [["original", "Original folders and names"], ["by_date", "By date"], ["by_type", "By type"]])),
      settingRow("Open the folder when done", "Shows the recovered files in " + (S.platform === "macos" ? "Finder" : "your file manager") + " as soon as they are back.", toggle("openWhenDone")),
      h("p.fine", { text: "Reperio never writes to the drive being recovered, and will not save onto it." }),
    ],
    privacy: () => {
      const storage = h("div");
      const refresh = async () => {
        let info = { saved_scans: 0, saved_bytes: 0 };
        try { info = await invoke("app_info"); } catch { /* old build */ }
        setKids(storage, settingRow("Stopped scans", info.saved_scans
          ? `${plural(info.saved_scans, "scan")} kept so they can be continued (${size(info.saved_bytes)}).`
          : "None kept.",
        h("button.btn", { disabled: !info.saved_scans, on: { click: async () => { await invoke("clear_saved_scans"); toast("Stopped scans forgotten."); refresh(); } } }, "Forget them")));
      };
      refresh();
      return [
        S.platform === "macos" && settingRow("Card access", "macOS protects memory cards. Allow Reperio to read them, once.",
          h("button.btn", { on: { click: () => { o.close(); showFullDiskAccess(""); } } }, "Set up…")),
        storage,
        vaultSection(),
        h("p.fine", { text: "Reperio keeps nothing about your files except this. Settings and these records stay on this computer." }),
      ];
    },
    plugins: () => {
      const list = h("div.plugin-list");
      (async () => {
        let info = { folders: [], plugins: [] };
        try { info = await invoke("plugins_info"); } catch { /* old build */ }
        setKids(list,
          info.plugins.length
            ? info.plugins.map((p) => h("div.plugin", {},
              h("b", {}, p.name, p.version && h("small.pver", { text: ` ${p.version}` })),
              h("span", { text: `${p.file.split(/[\\/]/).pop()} · ${p.kind === "analyzer" ? `forensic analyzer for ${p.targets.join(", ")}` : p.kind === "tool" ? "tool" : p.kind === "source" ? "source of devices" : `reads ${p.volumes.join(", ")} volumes`}` }),
              p.permissions?.length > 0 && h("span.pperm", { text: `Says it: ${p.permissions.join(" · ")}` }),
              p.last_run && h("span.plast" + (p.last_run.ok ? "" : ".bad"), { text: `Last run ${fmtDateTime.format(new Date(p.last_run.when))}, ${p.last_run.seconds < 60 ? `${p.last_run.seconds} s` : `${Math.round(p.last_run.seconds / 60)} min`}: ${p.last_run.note}` }),
              p.trust !== "known" && h("span.ptrust" + (p.trust === "changed" ? ".changed" : ""), {},
                p.trust === "new" ? "New: you haven't looked at this one yet. " : "Changed since you last used it. ",
                h("code", { text: `SHA-256 ${p.sha256.slice(0, 12)}…`, title: p.sha256 }), " ",
                h("button.linkish", { on: { click: async () => { await invoke("plugin_trust", { name: p.name }).catch(() => {}); o.close(); openSettings("plugins"); } } }, "Keep")),
              h("div.pactions", {},
                h("button.btn.ghost.small", { title: `Check that ${p.name} follows the plug-in protocol`, on: { click: () => testPlugin(p) } }, "Test"),
                h("button.btn.ghost.small", { on: { click: () => removePlugin(p, () => { o.close(); openSettings("plugins"); }) } }, "Remove"))))
            : h("div.fine", { text: info.problems?.length ? "No plug-in works yet." : "No plug-ins installed." }),
          h("p.fine", { text: "Looked for in:" }),
          info.problems?.length > 0 && [h("p.fine", { text: "Could not be used:" }), problemList(info.problems)],
          info.folders.map((f) => h("code.path", { text: f })));
      })();
      return [
        h("div.vault", {}, h("div.what", {},
          h("b", { text: "Your own recovery code" }),
          h("span", { text: "Put a script (.py, .js, .sh), a source file (.rs, .c, .cpp, .go, .zig) or a finished program in the plug-ins folder and Reperio uses it on the next scan. Source files are compiled for you the first time. See docs/plugins.md." })),
        h("div.actions.inline", {},
          h("button.btn.primary", { on: { click: () => installPluginSheet(() => { o.close(); openSettings("plugins"); }) } }, icon("save"), "Install a plug-in…"),
          h("button.btn", { on: { click: () => invoke("open_plugins_folder").catch((e) => toast(errText(e), true)) } }, icon("folder"), "Open plug-ins folder"))),
        settingRow("Stop a plug-in that goes quiet", "Reperio doesn't limit plug-ins. If you want one stopped when it has said nothing for a while, choose how long.",
          select("pluginIdleMin", [[0, "Never"], [5, "After 5 minutes"], [15, "After 15 minutes"], [60, "After an hour"]], () => applyPluginLimits())),
        settingRow("Longest a plug-in may run", "Off by default. Stop is always there.",
          select("pluginMaxHours", [[0, "No limit"], [1, "1 hour"], [6, "6 hours"], [24, "24 hours"]], () => applyPluginLimits())),
        h("div.actions.inline", {}, h("button.btn", { on: { click: () => { o.close(); togglePluginLog(true); } } }, icon("list"), "Show plug-in activity")),
        list,
      ];
    },
    about: () => {
      const version = h("span", {}, "Version 0.1.0 · Made by Abhivir");
      invoke("app_info").then((i) => { version.textContent = `Version ${i.version} · Made by Abhivir`; }).catch(() => {});
      return [
        h("div.about", {}, h("img.appicon", { src: "icon.png", alt: "" }), h("div", {}, h("b", { text: "Reperio" }), version)),
        h("p.fine", { text: "Reperio only ever reads from the drive you scan. Nothing on it is changed, and nothing is sent anywhere." }),
        settingRow("Open settings", "From anywhere in Reperio.", h("span.kbd", { text: (S.platform === "macos" ? "⌘" : "Ctrl+") + "," })),
        settingRow("Reset all settings", "Puts every setting on this page back to how it came.",
          h("button.btn", { on: { click: () => { Object.assign(settings, DEFAULTS); S.mode = settings.view; S.sort = settings.sort; applySettings(); show_(); toast("Settings reset."); } } }, "Reset")),
      ];
    },
  };
  function show_() {
    setKids(nav, PAGES.map(([k, label, ic]) =>
      h("button" + (k === current ? ".on" : ""), { on: { click: () => { current = k; show_(); } } }, icon(ic), label)));
    setKids(body, h("h2", { text: PAGES.find((p) => p[0] === current)[1] }), pages[current]());
    body.scrollTop = 0;
  }
  show_();
  const o = overlay(h("div.sheet.settings", {},
    nav, h("div.settings-main", {}, body,
      h("div.actions", {}, h("button.btn.primary", { on: { click: () => o.close() } }, "Done")))),
  { onClose: () => document.removeEventListener("keydown", onKey, true) });
  const onKey = (e) => { if (e.key === "Escape") o.close(); };
  document.addEventListener("keydown", onKey, true);
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

function resetView() {
  S.filter = { kind: "all", origin: settings.showExisting ? "all" : "lost", dir: null, year: null, size: null, goodOnly: settings.hideDamaged, grouped: false, q: "" };
  S.groupOf = new Map();
  S.stacks = new Map();
  S.selected = new Set();
  S.touchedSelection = false;
  S.limit = 400;
  S.animate = true;
  cardCache.clear();
}

/// Files in the plug-ins folder that could not be used, and why.
function problemList(problems) {
  return h("div.pproblems", {}, problems.map((p) =>
    h("div.pproblem", {}, h("b", { text: p.file }), h("pre", { text: p.reason }))));
}

/// Before a scan: which of the installed plug-ins to use, if any are
/// installed. File-system readers add to what Reperio reads itself;
/// forensic analyzers run on the opened drive or image once it is scanned.
/// Resolves to { volume: names | null, analyzers: names }, or null if the
/// person backs out. Nothing is asked when no plug-in is installed.
/// The settings a plug-in asked for, as a form. Changes are remembered and
/// handed to the plug-in on its next run.
function pluginOptions(p) {
  const values = { ...p.values };
  const save = () => invoke("plugin_set_options", { name: p.name, values }).catch((e) => toast(errText(e), true));
  const field = (o) => {
    const set = (v) => { values[o.id] = v; save(); };
    let control;
    if (o.kind === "toggle") control = h("button.switch" + (values[o.id] ? ".on" : ""), { role: "switch", "aria-checked": String(!!values[o.id]), title: o.label, on: { click: (e) => { const v = !values[o.id]; e.currentTarget.classList.toggle("on", v); e.currentTarget.setAttribute("aria-checked", String(v)); set(v); } } });
    else if (o.kind === "choice") control = h("select.select", { on: { change: (e) => set(e.target.value) } }, o.choices.map(([v, l]) => h("option", { value: v, text: l, selected: values[o.id] === v })));
    else control = h("input.textin", { type: o.kind === "number" ? "number" : "text", value: values[o.id] ?? "", on: { change: (e) => set(o.kind === "number" ? Number(e.target.value) : e.target.value) } });
    return h("div.popt", {}, h("div", {}, h("b", { text: o.label }), o.help && h("span", { text: o.help })), control);
  };
  return h("div.popts", {}, p.options.map(field));
}

/// What a plug-in works on, in words.
const WORKS_ON = { android: "a connected Android phone", image: "a drive or disk image", none: "nothing connected (use Run a plug-in…)", any: "anything" };
const worksOn = (p) => (p.targets || []).map((t) => WORKS_ON[t] || t).join(" or ") || "something else";

/// Whether a plug-in can work on what is open: `null` if it can, otherwise
/// the reason it can't. `where` is "volume" (a card, drive, image, RAID or
/// plug-in device Reperio reads as bytes) or "android" (a connected phone).
function fitFor(p, where) {
  if (p.kind === "source") return "Provides devices; it is used from the home screen, not per scan.";
  if (p.kind === "volume") return where === "volume" ? null : "Reads file systems on drives and images, not on a phone.";
  const want = where === "android" ? "android" : "image";
  if ((p.targets || []).length && (p.targets || []).every((t) => t === "none")) return "Runs on its own: start it from Run a plug-in… on the home screen.";
  return (p.targets || []).some((t) => t === want || t === "any") ? null : `Works on ${worksOn(p)}, not on ${WORKS_ON[want]}.`;
}

async function pickPlugins() {
  let info;
  try { info = await invoke("plugins_info"); } catch { return { volume: null, analyzers: [] }; }
  const fits = info.plugins.filter((p) => fitFor(p, "volume") === null);
  const readers = fits.filter((p) => p.kind === "volume");
  const analyzers = fits.filter((p) => p.kind === "analyzer" || p.kind === "tool");
  // Installed, but not for this: shown greyed with the reason, never silently hidden.
  const unfit = info.plugins.filter((p) => p.kind !== "source" && fitFor(p, "volume") !== null);
  const problems = info.problems || [];
  if (!readers.length && !analyzers.length) {
    if (!problems.length || !settings.askPlugins) return { volume: null, analyzers: [] };
    // Something is in the plug-ins folder but cannot be used: say why, here,
    // where a plug-in was expected to be offered.
    return new Promise((resolve) => {
      const o = overlay(h("div.sheet.wide.plugin-card", {},
        h("h2", { text: "No plug-in could be used" }),
        h("p.lead", { text: `${plural(problems.length, "file")} in your plug-ins folder can't be used yet.` }),
        problemList(problems),
        h("p.fine", { text: "Fix it and the plug-in appears here on the next scan. Scripts (.py, .js, .sh), source files (.rs, .c, .cpp) and finished programs all work." }),
        h("div.actions", {},
          h("button.btn", { on: { click: () => { o.close(); resolve(null); } } }, "Cancel"),
          h("button.btn", { on: { click: () => invoke("open_plugins_folder").catch(() => {}) } }, icon("folder"), "Open plug-ins folder"),
          h("button.btn.primary", { on: { click: () => { o.close(); resolve({ volume: null, analyzers: [] }); } } }, "Scan without plug-ins"))));
    });
  }
  const off = new Set(settings.pluginsOff);
  const on = new Set([...readers, ...analyzers].map((p) => p.name).filter((n) => !off.has(n)));
  const result = () => ({
    volume: readers.length ? readers.map((p) => p.name).filter((n) => on.has(n)) : null,
    analyzers: analyzers.map((p) => p.name).filter((n) => on.has(n)),
  });
  const remember = () => {
    settings.pluginsOff = [...readers, ...analyzers].map((p) => p.name).filter((n) => !on.has(n));
    applySettings();
  };
  if (!settings.askPlugins) return result();

  return new Promise((resolve) => {
    let ask = true;
    const sheet = h("div.sheet.wide.plugin-card");
    const o = overlay(sheet, { onClose: () => document.removeEventListener("keydown", onKey, true) });
    const finish = (value) => { if (value) { remember(); if (!ask) { settings.askPlugins = false; applySettings(); } } o.close(); resolve(value); };
    const onKey = (e) => { if (e.key === "Escape") finish(null); };
    document.addEventListener("keydown", onKey, true);
    const group = (title, note, list, kindLabel) => list.length > 0 && h("div.pgroup", {},
      h("div.section-label", { text: title }),
      h("p.fine", { text: note }),
      list.map((p) => h("div.pwrap", {},
        h("label.pitem", {},
          h("input", { type: "checkbox", checked: on.has(p.name), on: { change: (e) => { e.target.checked ? on.add(p.name) : on.delete(p.name); render(); } } }),
          h("div", {},
            h("b", {}, p.name, p.version && h("small.pver", { text: ` ${p.version}` })),
            h("span", { text: p.description || (p.kind !== "volume" ? `Works on: ${p.targets.join(", ")}` : `Reads: ${p.volumes.join(", ")}`) }),
            p.permissions?.length > 0 && h("span.pperm", { text: `Says it: ${p.permissions.join(" · ")}` })),
          h("span.chip-tag", { text: kindLabel })),
        on.has(p.name) && p.options?.length > 0 && pluginOptions(p))));
    function render() {
      const all = [...readers, ...analyzers];
      const n = all.filter((p) => on.has(p.name)).length;
      setKids(sheet,
        h("h2", { text: "Scan with plug-ins" }),
        h("p.lead", { text: "Reperio's own readers always run. Choose which of your plug-ins to add to this scan; you can pick several." }),
        group("File-system readers", "They add files from volumes Reperio can't read itself.", readers, "Reader"),
        group("Forensic analyzers", "They run on the drive once it has been scanned, and their reports appear with the results.", analyzers, "Analyzer"),
        unfit.length > 0 && h("div.pgroup", {}, h("div.section-label", { text: "Not for this device" }),
          unfit.map((p) => h("div.pitem.off", {}, h("div", {}, h("b", { text: p.name }), h("span", { text: fitFor(p, "volume") })), h("span.chip-tag", { text: p.kind === "volume" ? "Reader" : p.kind === "tool" ? "Tool" : "Analyzer" })))),
        problems.length > 0 && h("div.pgroup", {}, h("div.section-label", { text: "Could not be used" }), problemList(problems)),
        h("div.pbar", {},
          h("button.linkish", { on: { click: () => { all.forEach((p) => on.add(p.name)); render(); } } }, "Select all"),
          h("button.linkish", { on: { click: () => { on.clear(); render(); } } }, "None"),
          h("label.check", {}, h("input", { type: "checkbox", on: { change: (e) => { ask = !e.target.checked; } } }), h("span", { text: "Don't ask again (change in Settings → Scanning)" }))),
        h("div.actions", {},
          h("button.btn", { on: { click: () => finish(null) } }, "Cancel"),
          h("button.btn.primary", { on: { click: () => finish(result()) } }, n ? `Scan with ${plural(n, "plug-in")}` : "Scan without plug-ins")));
    }
    render();
  });
}

/// Plug-ins that need no drive or phone: tools. Any program you made, with
/// the arguments you type. It runs with full access (Reperio does not limit
/// plug-ins), and everything it prints is shown.
async function openTools() {
  let info;
  try { info = await invoke("plugins_info"); } catch { info = { plugins: [], problems: [] }; }
  const tools = info.plugins.filter((p) => (p.kind === "tool" || p.kind === "analyzer") && p.targets.some((t) => t === "none" || t === "any"));
  const t = { picked: tools[0]?.name, args: "", running: false, reports: null, dest: null, error: null };
  const sheet = h("div.sheet.wide.tools");
  let unlisten = null;
  const o = overlay(sheet, { onClose: () => { unlisten?.(); document.removeEventListener("keydown", onKey, true); } });
  const onKey = (e) => { if (e.key === "Escape" && !t.running) o.close(); };
  document.addEventListener("keydown", onKey, true);
  const splitArgs = (s) => (s.match(/"[^"]*"|'[^']*'|\S+/g) || []).map((x) => x.replace(/^(["'])(.*)\1$/, "$2"));

  function render() {
    if (t.reports) {
      setKids(sheet, h("h2", { text: t.reports[0].plugin }), reportsList(t.reports),
        h("div.actions", {},
          t.dest && h("button.btn", { on: { click: () => invoke("reveal", { path: t.dest }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
          h("button.btn", { on: { click: () => { t.reports = null; render(); } } }, "Run again"),
          h("button.btn.primary", { on: { click: () => o.close() } }, "Done")));
      return;
    }
    if (t.running) {
      setKids(sheet, h("h2", { text: `Running ${t.picked}` }), h("div.listening", {}, h("div.spinner"), h("span", { id: "tool-now", text: "Working…" })),
        h("div.actions", {}, h("button.btn", { on: { click: () => invoke("android_cancel") } }, "Stop")));
      return;
    }
    setKids(sheet,
      h("h2", { text: "Run a plug-in" }),
      h("p.lead", { text: "Tools are programs of your own that need no drive or phone. They run with full access to this computer: Reperio does not limit them. Everything they print is shown here and kept." }),
      tools.length === 0
        ? [h("p.fine", { text: info.problems?.length ? "No plug-in works yet. See Settings → Plug-ins for why." : "No tool plug-ins installed. Put one in the plug-ins folder (a plug-in whose info says \"kind\":\"tool\"); see docs/forensic-plugins.md." }),
          h("div.actions", {}, h("button.btn", { on: { click: () => invoke("open_plugins_folder").catch(() => {}) } }, icon("folder"), "Open plug-ins folder"), h("button.btn.primary", { on: { click: () => o.close() } }, "Close"))]
        : [h("div.choices.tight", {}, tools.map((p) =>
            h("div.choice" + (t.picked === p.name ? ".on" : ""), { on: { click: () => { t.picked = p.name; render(); } } },
              h("div.radio"), h("div", {}, h("b", { text: p.name }), h("span", { text: p.description || p.file.split(/[\\/]/).pop() }))))),
          h("div.field", {}, h("label", { text: "Arguments (optional)" }),
            h("input.textfield", { type: "text", value: t.args, placeholder: "for example: --depth 3 \"my folder\"", on: { input: (e) => { t.args = e.target.value; } } })),
          t.error && h("div.error", { text: t.error }),
          h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Cancel"), h("button.btn.primary", { on: { click: go } }, "Choose a folder and run"))]);
  }

  async function go() {
    let d;
    try { d = await invoke("choose_destination"); } catch (e) { t.error = errText(e); render(); return; }
    if (!d) return;
    t.dest = `${d.path}/Plug-in ${t.picked} ${new Date().toISOString().slice(0, 16).replace(/[:T]/g, "-")}`;
    t.running = true; t.error = null; render();
    unlisten = await listen("android-progress", (p) => { const el = document.getElementById("tool-now"); if (el) el.textContent = p.name || "Working…"; });
    try {
      t.reports = await invoke("run_tool", { name: t.picked, args: splitArgs(t.args), dest: t.dest });
    } catch (e) { t.error = errText(e); }
    unlisten?.(); unlisten = null;
    t.running = false;
    render();
  }
  render();
}

/// The reports of forensic plug-ins: findings, files, and the audit log.
function reportsList(reps) {
  const chip = (sev) => h("span.chip-tag" + (sev === "important" ? ".warn" : ""), { text: sev === "important" ? "Important" : sev === "warn" ? "Note" : "Info" });
  return h("div.a-reports", {}, reps.map((r) => h("div.a-report", {},
    reps.length > 1 && h("b", { text: r.plugin }),
    r.error && h("div.error", { text: r.error }),
    r.findings.length > 0 && h("ul.a-findings", {}, r.findings.slice(0, 30).map((f) =>
      h("li", {}, chip(f.severity), h("span", {}, h("b", { text: f.title + (f.count != null ? ` (${f.count.toLocaleString()})` : "") }), f.detail ? h("em", { text: f.detail }) : null)))),
    r.findings.length === 0 && !r.error && !r.console && h("p.fine", { text: "Nothing to report." }),
    r.console && h("div.a-console", {}, h("div.section-label", { text: "What the plug-in printed" }), h("pre", { text: r.console })),
    r.outputs.length > 0 && h("ul.a-list", {}, r.outputs.map((o) => h("li", { text: `${o.path} · ${size(o.size)}${o.title ? ` · ${o.title}` : ""}` }))),
    h("p.fine", { text: `${r.requests} requests${r.refused ? `, ${r.refused} refused` : ""}. Audit log: ${r.audit_log}` }))));
}

/// After a scan, runs the forensic plug-ins that were chosen, once.
async function runAnalyzers() {
  const names = S.plugins?.analyzers || [];
  if (!names.length || S.analyzed) return;
  S.analyzed = true;
  toast(`Running ${plural(names.length, "forensic plug-in")}…`);
  try {
    const out = await invoke("analyze_source", { names });
    S.reports = out.reports;
    // Files a plug-in offered join the results as if Reperio had found them.
    for (const it of out.items || []) { S.items[it.id] = it; cardCache.delete(it.id); }
    if (out.items?.length) {
      if (S.view === "results") scheduleResults();
      toast(`${plural(out.items.length, "recoverable file")} found by plug-ins. Plug-in reports are ready.`);
    } else toast("Plug-in reports are ready.");
  } catch (e) {
    toast(errText(e), true);
  }
  updateReportsButton();
}

function updateReportsButton() {
  const slot = document.getElementById("reports-slot");
  if (!slot) return;
  setKids(slot, S.reports?.length && h("button.btn", { title: "Plug-in reports", on: { click: showReports } }, icon("info"), "Plug-in reports"));
}

function showReports() {
  const o = overlay(h("div.sheet.wide", {},
    h("h2", { text: "Plug-in reports" }),
    h("p.lead", { text: "Findings, the files each plug-in wrote, and an audit log are saved with fingerprints (SHA-256)." }),
    reportsList(S.reports),
    h("div.actions", {},
      S.reports[0]?.dir && h("button.btn", { on: { click: () => invoke("reveal", { path: S.reports[0].dir.replace(/[\\/][^\\/]*$/, "") }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
      h("button.btn.primary", { on: { click: () => o.close() } }, "Done"))));
}

/// Starts scanning the opened card, offering to continue a scan of it that
/// stopped partway.
async function beginScan() {
  const choice = await pickPlugins();
  if (choice === null) { renderHome(); return; }
  S.plugins = choice;
  S.analyzed = false;
  S.reports = null;
  let saved = null;
  try { saved = await invoke("scan_checkpoint"); } catch { /* none */ }
  // First the quick scan: the card's own list of deleted files, with their
  // names, in minutes. The deep scan is offered with the results.
  const deepFirst = settings.scanFirst === "deep";
  if (!saved || !settings.offerResume) { startScan(deepFirst); return; }
  const when = fmtDateTime.format(new Date(saved.saved * 1000));
  const o = overlay(h("div.sheet", {},
    h("h2", { text: "Continue where you left off?" }),
    h("p.lead", { text: `A scan of this ${medium()} stopped at ${Math.floor(saved.percent)}% on ${when}, with ${plural(saved.found, "file")} found by the deep scan. Continuing picks up from there.` }),
    h("div.actions", {},
      h("button.btn", { on: { click: () => { o.close(); startScan(true, false); } } }, "Start over"),
      h("button.btn.primary", { on: { click: () => { o.close(); startScan(true, true, saved.percent / 100); } } }, "Continue"))));
}

/// `resumeAt`: for a continued scan, how far it had got (0 to 1), shown
/// while the file list is read again.
async function startScan(deep, resume = false, resumeAt = 0, findPartitions = settings.findPartitions) {
  S.items = [];
  stripIds = [];
  stripQueue = [];
  resetView();
  S.scan = { running: true, phase: "file_system", done: 0, total: 0, started: Date.now(), deepStarted: 0, deepStartDone: 0, deep, resumed: resume, resumeAt, findPartitions };
  S.volumes = [];
  S.freeSpace = null;
  renderScan();
  try {
    await invoke("start_scan", { deep, resume, findPartitions, plugins: S.plugins?.volume ?? null });
  } catch (e) {
    S.scan.running = false;
    toast(errText(e), true);
    renderHome();
  }
}

function onScanUpdate(u) {
  const deepBegins = u.phase === "deep_scan" && S.scan.phase !== "deep_scan";
  if (deepBegins) {
    S.scan.deepStarted = Date.now();
    S.scan.deepStartDone = u.done;
    // The engine only continues if the drive's searchable areas are what
    // they were; otherwise (it changed, or Reperio now reads more of it)
    // it starts again, and says so.
    if (S.scan.resumed && S.scan.resumeAt > 0.02 && u.total && u.done < u.total * S.scan.resumeAt * 0.5) {
      S.scan.resumeAt = 0;
      toast(`This ${medium()} has changed since the last scan, or Reperio now reads more of it, so the scan starts from the beginning.`);
    }
  }
  if (u.done !== S.scan.done || u.phase !== S.scan.phase) S.scan.moved = Date.now();
  Object.assign(S.scan, { phase: u.phase, done: u.done, total: u.total });
  if (u.at != null) S.scan.at = u.at;
  if (u.free_space) S.freeSpace = u.free_space;
  for (const v of u.volumes || []) S.volumes.push(v);
  for (const it of u.items) S.items[it.id] = it;
  for (const it of u.updated) { S.items[it.id] = it; cardCache.delete(it.id); }
  if (S.view === "scan") updateScan(u.items);
  else if (S.view === "results") scheduleResults();
}

function onScanDone(d) {
  S.scan.running = false;
  S.items = S.items.filter(Boolean);
  // Tiles made during the scan show icons for videos; now they can have thumbnails.
  for (const i of S.items) if (i.kind === "video" || i.ext === "pdf") cardCache.delete(i.id);
  if (d.bad_sectors > 0) toast(`${plural(d.bad_sectors, "sector")} could not be read. The ${medium()} may be failing: back it up (in the sidebar), then scan the copy.`, true);
  else if (d.slow_reads > 0) toast(`Parts of the ${medium()} were very slow to read, which can mean it is wearing out. Consider backing it up before doing anything else with it.`, true);
  selectDefaults();
  if (!d.cancelled) setTimeout(runAnalyzers, 600);
  // The file list remembers nothing deleted (or could not be read): the
  // deep scan is the only way on, so go straight into it.
  if (settings.autoDeep && S.scan.deep === false && !d.cancelled && !S.items.some((i) => i.origin !== "existing")) {
    if (S.view !== "scan") { startScan(true); return; }
    setPhase(`No deleted files in the ${medium()}'s file list`, "Searching every part of it instead…");
    setTimeout(() => { if (S.view === "scan" && !S.scan.running) startScan(true); }, 1400);
    return;
  }
  if (S.view !== "scan" || MOTION.reduced || d.cancelled) {
    if (S.view !== "results") S.animate = true;
    renderResults();
    return;
  }
  // Let the bar fill and the counts land before moving on.
  const found = S.items.filter((i) => i.origin !== "existing").length;
  scanView?.finish();
  setPhase(found ? "Scan complete" : "Scan complete, but nothing deleted was found",
    found ? `${plural(found, "file")} can be brought back` : "");
  setTimeout(() => { if (S.view === "scan") { S.animate = true; renderResults(); } }, 1900);
}

/// After a scan, pre-select what people almost always want: every deleted
/// file in good condition.
function selectDefaults() {
  if (S.touchedSelection || !settings.autoSelect) return;
  S.selected = new Set(S.items.filter((i) => i.origin !== "existing" && usable(i)).map((i) => i.id));
}

function counts(items) {
  const c = { photo: 0, video: 0, raw: 0, other: 0 };
  for (const i of items) {
    if (i.origin === "existing") continue;
    if (i.kind in c) c[i.kind]++; else c.other++;
  }
  return c;
}

const STAT_KINDS = [["photo", "Photos"], ["video", "Videos"], ["raw", "RAW photos"], ["other", "Other files"]];
const statKey = (kind) => (kind === "photo" || kind === "video" || kind === "raw" ? kind : "other");

let scanView = null;
let elapsedTimer = null;

/// The drive as a grid of squares, each an equal slice of it, start to end
/// along the rows. A square takes the colour of the kind of file that fills
/// most of it; squares already searched are shaded, and the one being read
/// now pulses. The grid is laid out to the width it has, so it stays true
/// at any window size.
class SectorMap {
  static PITCH = 17; // square plus gap, in points
  static GAP = 3;
  static HEIGHT = 272; // about sixteen rows

  constructor(canvas, total, onLayout) {
    this.canvas = canvas;
    this.total = Math.max(1, total);
    this.onLayout = onLayout;
    this.spans = []; // [from, to, kind]
    this.head = 0;
    this.full = false;
    this.pending = false;
    this.cols = 0;
    this.resize = new ResizeObserver(() => this.draw());
    this.resize.observe(canvas.parentElement);
    this.tick = this.tick.bind(this);
    requestAnimationFrame(this.tick);
  }
  add(item) {
    const kind = statKey(item.kind);
    for (const e of item.extents || []) this.spans.push([e.offset, e.offset + e.len, kind]);
    this.schedule();
  }
  setHead(at) {
    if (at == null) return;
    this.head = Math.min(this.total, Math.max(this.head, at));
    this.schedule();
  }
  finish() {
    this.full = true;
    this.schedule();
  }
  schedule() {
    if (this.pending) return;
    this.pending = true;
    requestAnimationFrame(() => { this.pending = false; this.draw(); });
  }
  // The square being read breathes while the scan runs.
  tick(t) {
    if (!this.canvas.isConnected) { this.resize.disconnect(); return; }
    if (!this.full && S.scan.running && !MOTION.reduced) {
      this.pulse = 0.5 + 0.5 * Math.sin(t / 380);
      this.draw();
    }
    setTimeout(() => requestAnimationFrame(this.tick), 60);
  }
  draw() {
    const c = this.canvas;
    if (!c.isConnected) return;
    const { PITCH, GAP, HEIGHT } = SectorMap;
    const w = c.parentElement.clientWidth;
    if (!w) return;
    const cols = Math.max(8, Math.floor((w + GAP) / PITCH));
    const rows = Math.max(4, Math.floor((HEIGHT + GAP) / PITCH));
    const cells = cols * rows;
    const per = this.total / cells;
    if (cols !== this.cols) {
      this.cols = cols;
      this.onLayout?.(per);
    }
    // What fills each square, by kind.
    const fill = new Map();
    for (const [from, to, kind] of this.spans) {
      const first = Math.floor(from / per), last = Math.min(cells - 1, Math.floor((to - 1) / per));
      for (let i = first; i <= last; i++) {
        const lo = Math.max(from, i * per), hi = Math.min(to, (i + 1) * per);
        let f = fill.get(i);
        if (!f) fill.set(i, (f = {}));
        f[kind] = (f[kind] || 0) + (hi - lo);
      }
    }
    const dpr = window.devicePixelRatio || 1;
    const width = cols * PITCH - GAP, height = rows * PITCH - GAP;
    c.style.width = width + "px";
    c.style.height = height + "px";
    if (c.width !== Math.round(width * dpr) || c.height !== Math.round(height * dpr)) {
      c.width = Math.round(width * dpr);
      c.height = Math.round(height * dpr);
    }
    const g = c.getContext("2d");
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, width, height);
    const empty = cssVar("--map-empty"), done = cssVar("--map-done"), accent = cssVar("--accent");
    const at = this.full ? cells : Math.floor(this.head / per);
    const size = PITCH - GAP;
    for (let i = 0; i < cells; i++) {
      const f = fill.get(i);
      let color = i < at ? done : empty;
      if (f) {
        let best = 0;
        for (const k in f) if (f[k] > best) { best = f[k]; color = KIND_COLOR[k] || KIND_COLOR.other; }
      }
      g.fillStyle = color;
      g.globalAlpha = 1;
      if (i === at && !this.full && S.scan.running) {
        g.fillStyle = accent;
        g.globalAlpha = 0.35 + 0.65 * (this.pulse ?? 1);
      }
      g.beginPath();
      g.roundRect((i % cols) * PITCH, Math.floor(i / cols) * PITCH, size, size, 3);
      g.fill();
    }
    g.globalAlpha = 1;
  }
}

function renderScan() {
  S.view = "scan";
  show(h("div.view", {},
    h("div.toolbar", {},
      h("div.crumb", {}, h("div.t", { text: S.source?.label || "" }), h("div.s", { text: S.source ? size(S.source.size) : "" })),
      h("div.spacer")),
    h("div.scan", {},
      h("div.scan-inner", { id: "scan-inner" },
        h("div.meter", {},
          h("div.meter-top", {},
            h("div.pct", { id: "pct" }, h("span", { id: "pct-n", text: "0" }), h("small", { text: "%" }), h("span.done-mark", {}, icon("check"), "Done")),
            h("div.elapsed", { id: "elapsed", text: "0:00" })),
          h("div.meter-bar", {}, h("div", { id: "meter-fill" }))),
        h("div.scan-phase", { id: "phase-box", role: "status", "aria-live": "polite" }, h("h2", { id: "phase" }), h("p", { id: "detail" })),
        h("div.trim-note", { id: "trim-note", role: "status" }),
        h("div.mapcard", {},
          h("div.map-wrap", {}, h("canvas.sectors", { id: "sectors", role: "img", "aria-label": `Map of the ${medium()}` })),
          h("div.legend", {},
            [["photo", "Photos"], ["video", "Videos"], ["raw", "RAW"], ["other", "Other"]].map(([k, label]) =>
              h("span", {}, h("i", { style: { background: KIND_COLOR[k] } }), label)),
            h("span", {}, h("i.searched"), "Searched"),
            h("span.note", { id: "map-note" }))),
        h("div.counters", {},
          STAT_KINDS.map(([k, label]) => h("div.counter", { id: "stat-" + k },
            h("b", { text: "0" }), h("span", {}, h("i", { style: { background: KIND_COLOR[k] } }), label)))),
        h("div.strip", { id: "strip" }),
        h("div.scan-actions", {},
          h("button.btn", { on: { click: () => { S.animate = true; renderResults(); } } }, "View files found"),
          h("button.btn.ghost.danger", { on: { click: () => confirmStop(() => invoke("cancel")) } }, "Stop"))))));

  const map = new SectorMap(document.getElementById("sectors"), S.source?.size || 1, (per) => {
    const note = document.getElementById("map-note");
    if (note) note.textContent = `Each square is ${size(per)} of the ${medium()}`;
  });
  scanView = {
    map,
    finish() {
      document.getElementById("scan-inner")?.classList.add("finished");
      setProgress(1);
      map.finish();
    },
  };
  for (const it of S.items) if (it && it.origin !== "existing") map.add(it);
  if (S.scan.phase === "deep_scan") map.setHead(S.scan.at);
  const c = counts(S.items.filter(Boolean));
  for (const [k] of STAT_KINDS) {
    const b = document.querySelector(`#stat-${k} b`);
    b.textContent = c[k].toLocaleString();
    b.dataset.value = String(c[k]);
  }
  const strip = document.getElementById("strip");
  for (const id of stripIds) strip.append(h("img.still", { src: thumbUrl(id), alt: "", on: { error: (e) => e.target.remove() } }));
  clearInterval(elapsedTimer);
  const tickClock = () => {
    const el = document.getElementById("elapsed");
    if (!el) { clearInterval(elapsedTimer); return; }
    if (!S.scan.running) return;
    el.textContent = clock((Date.now() - S.scan.started) / 1000);
    if (S.scan.phase === "deep_scan") setPhase(...scanText());
  };
  tickClock();
  elapsedTimer = setInterval(tickClock, 1000);
  // Coming back to a running scan: show where it has got to at once.
  setProgress(scanProgress());
  updateScan([]);
}

/// A plug-in reading a volume says how far it has got. The big number stays
/// with the scan's own progress (it only goes up, and the deep scan follows
/// the file list); this fills the line below it.
function onPluginProgress(p) {
  if (!S.scan.running) return;
  S.scan.plugin = p.finished ? null : p;
  if (!p.finished) S.scan.moved = Date.now();
  if (scanView && document.getElementById("pct") && S.scan.phase !== "deep_scan") setPhase(...scanText());
}

/// The percentage, straight from the scan's own numbers.
function setProgress(p) {
  const n = document.getElementById("pct-n");
  if (!n) return;
  p = Math.max(0, Math.min(1, p));
  n.textContent = String(Math.floor(p * 100 + 1e-9));
  document.getElementById("pct")?.setAttribute("aria-label", `${Math.round(p * 100)} percent`);
  const fill = document.getElementById("meter-fill");
  if (fill) fill.style.width = `${(p * 100).toFixed(2)}%`;
}

/// How far the scan has got, as one number that only goes up: the part of
/// the drive the deep scan has searched, the same part the map shades.
/// The file list, read first, takes seconds: the number holds at 0 (or
/// where a continued scan had got to) while it is read, with the entries
/// counted in the line below, so a deep scan after it never sends the
/// number back to 0.
function scanProgress() {
  const s = S.scan;
  if (s.phase === "deep_scan") return s.total ? s.done / s.total : 0;
  if (s.phase === "repair") return 1;
  return s.resumeAt || 0;
}

function scanText() {
  const s = S.scan;
  if (s.phase === "repair") {
    return ["Repairing photos and videos", s.total ? `Checking ${Math.min(s.done + 1, s.total).toLocaleString()} of ${plural(s.total, "broken file")}` : ""];
  }
  if (s.phase === "deep_scan") {
    const secs = (Date.now() - s.deepStarted) / 1000;
    const rate = (s.done - s.deepStartDone) / Math.max(secs, 0.5);
    const remaining = (s.total - s.done) / rate;
    const left = rate > 0 && secs > 3 ? ` · ${size(rate)}/s · ${remaining < 60 ? "" : "about "}${duration(remaining)} left` : "";
    // Nothing new for a while: say so rather than look frozen.
    const still = (Date.now() - (s.moved || Date.now())) / 1000;
    // Free space only when the file system could be read.
    const where = s.total < (S.source?.size || 0) * 0.98 ? `Looking through the ${medium()}'s free space` : `Looking through every part of the ${medium()}`;
    if (still > 15) return [where, `${size(s.done)} of ${size(s.total)} · this part is slow to read${still < 60 ? "" : ` (${duration(still)} so far)`}, still working`];
    return [where, `${size(s.done)} of ${size(s.total)}${left}`];
  }
  if (s.findPartitions && s.phase === "file_system") return [`Searching the whole ${medium()} for lost partitions`, s.total ? `${size(s.done)} of ${size(s.total)}` : "Looking at every part of it"];
  if (s.plugin) {
    const p = s.plugin;
    const bits = [p.plugin];
    if (p.text) bits.push(p.total ? `${p.text} ${Math.floor((100 * p.done) / p.total)}%` : p.text);
    else if (p.total) bits.push(`at ${size(p.done)} of ${size(p.total)}`);
    if (p.files) bits.push(plural(p.files, "file"));
    return [`Reading the ${medium()}'s file list`, bits.join(" · ")];
  }
  return [`Reading the ${medium()}'s file list`, s.total ? `${s.done.toLocaleString()} of ${plural(s.total, "entry", "entries")}` : "Finding partitions"];
}

function setPhase(title, detail) {
  const box = document.getElementById("phase-box");
  if (!box) return;
  const t = document.getElementById("phase");
  if (t.textContent !== title) {
    t.textContent = title;
    box.classList.remove("fade-swap");
    void box.offsetWidth;
    box.classList.add("fade-swap");
  }
  document.getElementById("detail").textContent = detail;
}

let stripQueue = [];
let stripShown = 0;
/// The photos in the strip, newest first, to put back when returning.
let stripIds = [];

function updateScan(newItems) {
  if (!scanView || !document.getElementById("pct")) return;
  setProgress(scanProgress());
  const [phase, detail] = scanText();
  setPhase(phase, detail);
  if (S.scan.phase === "deep_scan") scanView.map.setHead(S.scan.at);
  updateTrimOnScan();

  const c = counts(S.items.filter(Boolean));
  for (const [k] of STAT_KINDS) {
    const b = document.querySelector(`#stat-${k} b`);
    if (Number(b.dataset.value || 0) !== c[k]) countUp(b, c[k], 700);
  }

  // Mark each new find where it lies on the drive.
  for (const it of newItems) {
    if (it.origin === "existing") continue;
    scanView.map.add(it);
    if (it.kind === "photo" || it.kind === "raw") stripQueue.push(it.id);
  }
  // Refresh the strip at most once a second, with only the latest finds.
  if (stripQueue.length && Date.now() - stripShown > 1000) {
    stripShown = Date.now();
    const strip = document.getElementById("strip");
    for (const id of stripQueue.slice(-3)) {
      const img = h("img", { src: thumbUrl(id), alt: "", on: { error: (e) => e.target.remove() } });
      strip.prepend(img);
      stripIds.unshift(id);
    }
    stripIds = stripIds.slice(0, 12);
    stripQueue = [];
    while (strip.children.length > 12) strip.lastChild.remove();
  }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

function matches(i, f, skip) {
  if (skip !== "kind" && f.kind !== "all" && i.kind !== f.kind) return false;
  if (skip !== "origin" && !originMatches(i, f.origin)) return false;
  if (skip !== "dir" && f.dir != null && i.dir !== f.dir) return false;
  if (skip !== "year" && f.year != null && yearOf(i) !== f.year) return false;
  if (skip !== "size" && f.size != null && sizeClass(i) !== f.size) return false;
  if (f.goodOnly && !usable(i)) return false;
  if (f.q) {
    const q = f.q.toLowerCase();
    if (!i.name.toLowerCase().includes(q) && !i.dir.toLowerCase().includes(q)) return false;
  }
  return true;
}
const SIZES = [["small", "Under 1 MB"], ["medium", "1 MB to 100 MB"], ["large", "Over 100 MB"]];
const sizeClass = (i) => (i.size < 1e6 ? "small" : i.size <= 100e6 ? "medium" : "large");

/// How likely a file is to come back whole, 1 (poor) to 4 (excellent).
function chance(i) {
  if (i.health === "overwritten") return [1, "Low", "Part of it has been written over"];
  if (i.health === "damaged") return [1, "Low", "Part of it could not be found"];
  if (i.health === "repaired") return repairChance(i);
  if (i.health === "unknown") return [3, "Likely", `Its data is where the ${medium()} says`];
  if (i.origin === "carved") return [3, "Very good", "Intact, but its original name is lost"];
  if (i.origin === "snapshot") return [4, "Excellent", "Intact, kept by a snapshot of the drive"];
  return [4, "Excellent", "Intact, with its original name"];
}

/// What a repaired file is worth: a rebuilt photo, video or document is as good
/// as an intact one; a preview, a cut-off photo or rescued text is less.
function repairChance(i) {
  const f = i.fix || {};
  if (f.kind === "video") return f.silent ? [3, "Good", "Repaired; plays without sound"] : [3, "Good", "Repaired, with sound"];
  if (f.kind === "photo" && (f.how === "preview" || f.how === "heic_thumbnail")) return [2, "Fair", "Only a smaller preview of the photo"];
  if (f.kind === "photo" && (f.how === "partial" || f.how === "heic_partial")) return [2, "Fair", "Only the part of the photo that survived"];
  if (f.kind === "document" && f.how === "text") return [2, "Fair", "Only the text of the document"];
  return [3, "Good", "Repaired"];
}

function chanceMeter(i) {
  const [n, label] = chance(i);
  return h("span.chance", { title: `Recovery chance: ${label}`, "aria-label": `Recovery chance: ${label}` },
    [1, 2, 3, 4].map((k) => h("i" + (k <= n ? ".on" : ""))), h("span", { text: label }));
}

/// "lost" is the default view: everything that was deleted, however found.
function originMatches(i, origin) {
  if (origin === "all") return true;
  if (origin === "lost") return i.origin !== "existing";
  return i.origin === origin;
}
const yearOf = (i) => (itemDate(i) ? itemDate(i).slice(0, 4) : "Undated");

function visible() {
  const list = S.items.filter((i) => i && matches(i, S.filter));
  const byDate = (a, b) => {
    const da = itemDate(a), db = itemDate(b);
    if (da && db) return da < db ? 1 : da > db ? -1 : a.id - b.id;
    return da ? -1 : db ? 1 : a.id - b.id;
  };
  const cmp = {
    date: byDate,
    name: (a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }),
    size: (a, b) => b.size - a.size,
    chance: (a, b) => chance(b)[0] - chance(a)[0] || byDate(a, b),
  }[S.sort];
  return collapse(list.sort(cmp));
}

/// With "Group similar photos" on, each group shows as one card: its best
/// member among those the other filters leave, standing for the rest.
function collapse(list) {
  S.stacks = new Map();
  if (!S.filter.grouped || !S.groupOf.size) return list;
  const shown = new Set(list.map((i) => i.id));
  return list.filter((i) => {
    const g = S.groupOf.get(i.id);
    if (!g) return true;
    const members = g.members.map((m) => m.id).filter((id) => shown.has(id));
    if (members[0] !== i.id) return false;
    if (members.length > 1) S.stacks.set(i.id, members);
    return true;
  });
}

/// The files a card stands for: a group's members shown, or just itself.
const idsOf = (id) => S.stacks.get(id) || [id];
const expand = (ids) => ids.flatMap(idsOf);

/// "all", "some" or "none" of the files a card stands for are selected.
function selState(id) {
  const ids = idsOf(id);
  const n = ids.filter((x) => S.selected.has(x)).length;
  return n === 0 ? "none" : n === ids.length ? "all" : "some";
}

function showSel(el, id) {
  const st = selState(id);
  el.classList.toggle("selected", st === "all");
  el.classList.toggle("partial", st === "some");
  el.setAttribute("aria-checked", st === "all" ? "true" : st === "some" ? "mixed" : "false");
}

const byId = (id) => (S.items[id]?.id === id ? S.items[id] : S.items.find((i) => i && i.id === id));

function groups(list) {
  if (S.sort !== "date") return [[null, list]];
  const out = [];
  let key, cur;
  for (const i of list) {
    const d = itemDate(i);
    const k = d ? d.slice(0, 7) : "undated";
    if (k !== key) {
      key = k;
      cur = [];
      out.push([d ? fmtMonth.format(parseDate(d)) : "Date unknown", cur]);
    }
    cur.push(i);
  }
  return out;
}

const cardCache = new Map();

/// Thumbnails load as their tile nears the screen. WebKit's own lazy
/// loading stalls for tiles inserted while they fade in.
const thumbIO = new IntersectionObserver((entries) => {
  for (const e of entries) {
    if (!e.isIntersecting) continue;
    thumbIO.unobserve(e.target);
    e.target.src = e.target.dataset.src;
  }
}, { rootMargin: "600px 0px" });

/// Files that will open: intact, unchecked, or repaired.
const usable = (i) => i.health === "good" || i.health === "unknown" || i.health === "repaired";

function badgeFor(i) {
  if (i.health === "repaired") return ["Repaired", "good"];
  if (i.health === "damaged") return ["Damaged", "bad"];
  if (i.health === "overwritten") return ["Partly overwritten", "warn"];
  if (i.origin === "existing") return [`On ${medium()}`, ""];
  return null;
}

function thumbable(i) {
  // Video and PDF thumbnails copy the whole file off the card first, so
  // they wait until the scan has finished rather than slow it down.
  return i.kind === "photo" || i.kind === "raw" || (S.platform === "macos" && !S.scan.running && (i.kind === "video" || i.ext === "pdf"));
}

function placeholder(i) {
  return [icon(KIND_ICON[i.kind] || "file"), h("span.ext", { text: i.ext })];
}

function card(i) {
  let el = cardCache.get(i.id);
  if (el) return el;
  const badge = badgeFor(i);
  const thumb = h("div.thumb", {},
    thumbable(i)
      ? h("img", { dataset: { src: thumbUrl(i.id) }, alt: "", draggable: false, on: {
        load: (e) => { e.target.classList.add("loaded"); el.style.setProperty("--ar", ratio(e.target).toFixed(3)); },
        error: (e) => e.target.parentNode?.replaceChildren(...placeholder(i)),
      } })
      : placeholder(i));
  const img = thumb.querySelector("img");
  if (img) thumbIO.observe(img);
  const date = itemDate(i);
  const ratio = (img) => Math.max(0.6, Math.min(2.2, img.naturalWidth / img.naturalHeight || 1));
  el = h("div.card", { tabindex: 0, role: "checkbox", "aria-label": i.name, dataset: { id: i.id }, title: i.dir ? `${i.dir}/${i.name}` : i.name },
    h("div.check", {}, icon("check")),
    thumb,
    badge && h("div.badge" + (badge[1] ? "." + badge[1] : ""), { text: badge[0] }),
    h("div.meta", {},
      h("div.name", { text: i.name }),
      h("div.info", { text: [size(i.size), date && fmtDate.format(parseDate(date))].filter(Boolean).join(" · ") })));
  // Photos sit in rows that keep their shape; the estimate holds until the
  // thumbnail loads and tells the true proportions.
  el.style.setProperty("--ar", thumbable(i) ? "1.5" : "1");
  cardCache.set(i.id, el);
  return el;
}

/// The "+3" on a card standing for a group, and its stacked look.
function stackBadge(el, i) {
  const ids = S.stacks.get(i.id);
  el.classList.toggle("stacked", !!ids);
  let b = el.querySelector(".stack-n");
  if (!ids) { b?.remove(); return; }
  const text = `+${ids.length - 1}`;
  if (!b) {
    b = h("div.stack-n");
    el.querySelector(".thumb").append(b);
  }
  b.textContent = text;
  b.title = `${plural(ids.length - 1, "similar photo")} with it. Open it to choose.`;
  el.setAttribute("aria-label", `${i.name}, and ${plural(ids.length - 1, "similar file")}`);
}

function row(i) {
  const health = i.health === "damaged" ? h("span.pill.bad", { text: "Damaged" })
    : i.health === "overwritten" ? h("span.pill.warn", { text: "Partly overwritten" })
    : i.health === "repaired" ? h("span.pill.good", { text: "Repaired" })
    : i.origin === "existing" ? h("span.pill", { text: `On ${medium()}` })
    : i.found_by ? h("span.pill.good", { text: `Found by ${i.found_by}` })
    : i.origin === "carved" ? h("span.pill.good", { text: "Found by deep scan" })
    : i.origin === "snapshot" ? h("span.pill.good", { text: "From a snapshot" })
    : h("span.pill.good", { text: "Deleted" });
  const date = itemDate(i);
  const stack = S.stacks.get(i.id);
  return h("tr", { dataset: { id: i.id }, class: { all: "selected", some: "partial", none: "" }[selState(i.id)] },
    h("td", { style: "width:34px" }, h("span.cb", {}, icon("check"))),
    h("td.ico", {}, icon(KIND_ICON[i.kind] || "file")),
    h("td", {}, i.name, stack && h("span.pill.stack", { text: `+${stack.length - 1} similar` })),
    h("td.dim", { text: i.dir || (i.origin === "carved" ? "Deep scan" : "/") }),
    h("td.dim", { text: date ? fmtDateTime.format(parseDate(date)) : "" }),
    h("td.dim.num", { text: size(i.size) }),
    h("td", {}, chanceMeter(i)),
    h("td", {}, health));
}

function renderResults() {
  S.view = "results";
  clearTimeout(resultsTimer);
  resultsTimer = null;
  if (!document.querySelector(".results")) {
    show(h("div.view", {},
      h("div.toolbar", {},
        h("button.btn.icon.ghost", { title: "Back", on: { click: resultsBack } }, icon("back")),
        h("div.crumb", {}, h("div.t", { text: S.source?.label || "Results" }), h("div.s", { id: "found-line" })),
        h("div.spacer"),
        h("label.searchbox", {}, icon("search"),
          h("input", { id: "q", placeholder: "Search", value: S.filter.q, on: { input: (e) => { S.filter.q = e.target.value; S.limit = 400; updateResults(); } } })),
        h("select.select", { id: "sort", title: "Sort", on: { change: (e) => { S.sort = e.target.value; S.animate = true; updateResults(); } } },
          h("option", { value: "date", text: "Newest first" }),
          h("option", { value: "name", text: "Name" }),
          h("option", { value: "size", text: "Largest first" }),
          h("option", { value: "chance", text: "Best chance first" })),
        h("div.seg", {},
          h("button", { id: "mode-grid", title: "Thumbnails", on: { click: () => { S.mode = "grid"; S.animate = true; updateResults(); } } }, icon("grid")),
          h("button", { id: "mode-list", title: "List", on: { click: () => { S.mode = "list"; S.animate = true; updateResults(); } } }, icon("list"))),
        h("span", { id: "reports-slot" }),
        settingsButton()),
      h("div.results", {},
        h("div.sidebar", { id: "sidebar" }),
        h("div.content", {},
          h("div.grid-scroll", { id: "scroller", role: "grid", "aria-label": "Files found", on: { click: onGridClick, dblclick: onGridDblClick, keydown: onGridKey } }),
          h("div.bottombar", { id: "bottombar" })))));
    document.getElementById("sort").value = S.sort;
  }
  updateResults();
  // Reports that finished while the scan screen was still showing.
  updateReportsButton();
}

let resultsTimer;
function scheduleResults() {
  if (resultsTimer) return;
  resultsTimer = setTimeout(() => { resultsTimer = null; if (S.view === "results") updateResults(); }, 900);
}

function updateResults() {
  document.getElementById("mode-grid").classList.toggle("on", S.mode === "grid");
  document.getElementById("mode-list").classList.toggle("on", S.mode === "list");
  const lost = S.items.filter((i) => i && i.origin !== "existing").length;
  document.getElementById("found-line").textContent = S.scan.running
    ? `Scanning · ${plural(lost, "file")} found so far`
    : `${plural(lost, "deleted file")} found`;
  updateSidebar();
  updateList();
  updateBottom();
}

function sideItem(label, count, on, onClick, iconName) {
  return h("button.side-item" + (on ? ".on" : ""), { "aria-pressed": String(on), on: { click: onClick } },
    iconName && icon(iconName), h("span.label", { text: label }), h("span.count", { text: count.toLocaleString() }));
}

/// "Group similar photos": works out the groups the first time (photos
/// already shown are quick, others are decoded small), then shows one
/// card for each.
async function toggleGroups() {
  if (S.grouping) return;
  if (!S.filter.grouped && !S.groupOf.size) {
    S.grouping = { done: 0, total: 0 };
    updateSidebar();
    let groups;
    try { groups = await invoke("similar_groups"); }
    catch (e) { toast(errText(e), true); return; }
    finally { S.grouping = null; }
    S.groupOf = new Map();
    for (const g of groups) for (const m of g.members) S.groupOf.set(m.id, g);
    if (!groups.length) { toast("No similar photos or copies of the same file were found."); updateSidebar(); return; }
  }
  S.filter.grouped = !S.filter.grouped;
  S.animate = true;
  updateResults();
}

listen("group-progress", (p) => {
  if (!S.grouping) return;
  S.grouping = p;
  const note = document.getElementById("group-note");
  if (note) note.textContent = groupNote();
});

function groupNote() {
  const g = S.grouping;
  return g.total ? `Comparing photos… ${Math.floor((100 * g.done) / g.total)}%` : "Comparing photos…";
}

/// The sidebar's two top views: what was deleted (the default), and
/// everything, files still on the drive included.
function showOrigin(origin) {
  Object.assign(S.filter, { kind: "all", origin });
  S.limit = 400;
  S.animate = true;
  updateResults();
}

function setFilter(k, v) {
  const reset = { dir: null, year: null, size: null, kind: "all", origin: "lost" }[k];
  S.filter[k] = S.filter[k] === v ? reset : v;
  S.limit = 400;
  S.animate = true;
  updateResults();
}

function updateSidebar() {
  const f = S.filter;
  const all = S.items.filter(Boolean);
  const count = (skip, pred) => all.filter((i) => matches(i, f, skip) && pred(i)).length;
  const countWith = (over) => all.filter((i) => matches(i, { ...f, ...over })).length;
  const deleted = countWith({ kind: "all", origin: "lost" });
  const everything = countWith({ kind: "all", origin: "all" });

  const kinds = KINDS.map(([k, label, ic]) => [k, label, ic, count("kind", (i) => i.kind === k)]).filter((k) => k[3] > 0 || f.kind === k[0]);
  const origins = [["deleted", "With original names", "folder"], ["carved", "Found by deep scan", "scan"], ["snapshot", "From snapshots", "folder"], ["existing", `Still on the ${medium()}`, "on-card"]]
    .map(([k, label, ic]) => [k, label, ic, count("origin", (i) => i.origin === k)]).filter((o) => o[3] > 0 || f.origin === o[0]);

  // How they were found, once there is more than one way: with a single
  // one, it would only repeat "Everything found".
  const byOrigin = origins.filter((o) => o[3] > 0).length > 1 || f.origin !== "lost";
  const dirs = new Map();
  for (const i of all) if (i.origin !== "carved" && matches(i, f, "dir")) dirs.set(i.dir, (dirs.get(i.dir) || 0) + 1);
  const years = new Map();
  for (const i of all) if (matches(i, f, "year")) years.set(yearOf(i), (years.get(yearOf(i)) || 0) + 1);
  const yearList = [...years].sort((a, b) => (a[0] === "Undated" ? 1 : b[0] === "Undated" ? -1 : b[0].localeCompare(a[0])));

  const side = document.getElementById("sidebar");
  const scroll = side.scrollTop;
  side.replaceChildren(...[
    h("div.side-group", {},
      h("h4", { text: "Library" }),
      sideItem("Deleted files", countWith({ kind: "all", origin: "lost" }), f.kind === "all" && f.origin === "lost", () => showOrigin("lost"), "trash"),
      // Only when it is more than the deleted files: files still on the
      // drive were read too.
      everything > deleted && sideItem("Everything found", everything, f.kind === "all" && f.origin === "all", () => showOrigin("all"), "all"),
      kinds.map(([k, label, ic, n]) => sideItem(label, n, f.kind === k, () => setFilter("kind", k), ic))),
    h("div.side-group", {},
      h("h4", { text: byOrigin ? "Found" : "Show" }),
      byOrigin && origins.map(([k, label, ic, n]) => sideItem(label, n, f.origin === k, () => setFilter("origin", k), ic)),
      h("div.toggle", {}, "Hide damaged files",
        h("button.switch" + (f.goodOnly ? ".on" : ""), { title: "Hide damaged files", role: "switch", "aria-checked": String(f.goodOnly), on: { click: () => { f.goodOnly = !f.goodOnly; S.animate = true; updateResults(); } } })),
      h("div.toggle", { title: "Show one card for each set of similar photos (burst shots, edits, copies), with the rest a click away" }, "Group similar photos",
        h("button.switch" + (f.grouped ? ".on" : ""), { title: "Group similar photos", role: "switch", "aria-checked": String(f.grouped), disabled: S.scan.running || !!S.grouping, on: { click: toggleGroups } })),
      S.grouping && h("div.toggle-note", { id: "group-note", role: "status", text: groupNote() }),
      f.grouped && !S.grouping && h("div.toggle-note", { text: `${plural(new Set([...S.groupOf.values()]).size, "group")} · open one to choose` })),
    dirs.size > 0 && h("div.side-group", {},
      h("h4", { text: "Folders" }),
      [...dirs].sort((a, b) => a[0].localeCompare(b[0])).map(([d, n]) => sideItem(d || "Top level", n, f.dir === d, () => setFilter("dir", d), "folder"))),
    h("div.side-group", {},
      h("h4", { text: "Size" }),
      SIZES.map(([k, label]) => [k, label, count("size", (i) => sizeClass(i) === k)])
        .filter(([k, , n]) => n > 0 || f.size === k)
        .map(([k, label, n]) => sideItem(label, n, f.size === k, () => setFilter("size", k), "file"))),
    yearList.length > 1 && h("div.side-group", {},
      h("h4", { text: "Years" }),
      yearList.map(([y, n]) => sideItem(y, n, f.year === y, () => setFilter("year", y), "calendar"))),
    h("div.side-footer", {},
      h("button.side-item", { on: { click: saveResults }, disabled: S.scan.running }, icon("save"), h("span.label", { text: "Save scan results…" })),
      h("button.side-item", { on: { click: copyDetails } }, icon("copy"), h("span.label", { text: "Copy scan details" })),
      S.source?.is_disk && h("button.side-item", { on: { click: backupDisk }, disabled: S.scan.running }, icon("copy"), h("span.label", { text: `Back up the whole ${medium()}…` })),
      S.source?.is_disk && h("button.side-item", { on: { click: () => openDriveHealth(null) } }, icon("pulse"), h("span.label", { text: "Drive health…" })),
      h("div.signature", {}, "Made by ", h("b", { text: "Abhivir" }))),
  ].filter(Boolean));
  side.scrollTop = scroll;
}

let currentList = [];

/// What Reperio could read of the drive's file system, when that changes
/// how the results should be read; null when there is nothing to say.
function fsNotice() {
  const vols = S.volumes || [];
  if (S.scan.running && S.scan.phase === "file_system") return null;
  const readable = vols.filter((v) => v.readable);
  const found = vols.filter((v) => v.found);
  let title, text, action;
  // Where the search for lost partitions is worth offering: nothing could
  // be read, or the file list came up empty, and it was not already the
  // full search.
  const searchAgain = !S.scan.findPartitions && !S.scan.running
    ? { label: "Search for lost partitions", run: () => startScan(S.scan.deep !== false, false, 0, true) }
    : null;
  if (found.length && !S.scan.running) {
    title = found.length === 1 ? `Reperio found a partition missing from this ${medium()}'s partition table` : `Reperio found ${found.length} partitions missing from this ${medium()}'s partition table`;
    text = `${found.map((v) => `${v.fs}${v.label ? ` "${v.label}"` : ""} (${size(v.len)})`).join(", ")}. It read ${found.length === 1 ? "it" : "them"} as usual, so deleted files come back with their names.`;
  } else if (!vols.length) {
    title = `No file system Reperio knows on this ${medium()}`;
    text = `So it searched all of it. Files still on the ${medium()} may be listed here too.`;
    action = searchAgain;
  } else if (!readable.length) {
    const kinds = [...new Set(vols.map((v) => v.fs).filter((k) => k !== "Unknown"))].join(", ");
    title = kinds ? `Reperio can't read this ${medium()}'s file system (${kinds})` : `Reperio doesn't recognise this ${medium()}'s file system`;
    text = `So it searched all of it. Files still on the ${medium()} may be listed here too. If its partition table was damaged, a search of the whole ${medium()} may find the partitions that were lost.`;
    action = searchAgain;
  } else if (!readable.some((v) => v.files > 0)) {
    title = `This ${medium()}'s file list is empty`;
    text = `As it is after being erased or formatted. Everything here was found by the deep scan in its free space.`;
    action = searchAgain;
  } else return null;
  // The list is rebuilt as files come in: keep the same notice so its
  // entrance plays once.
  if (fsNotice.el?.dataset.title !== title) {
    fsNotice.el = h("div.deeper.notice", { dataset: { title } },
      h("div", {}, h("b", { text: title }), h("span", { text })),
      h("div.notice-actions", {},
        action && h("button.btn", { on: { click: action.run } }, action.label),
        h("button.btn.ghost", { on: { click: copyDetails } }, icon("copy"), "Copy details")));
    fsNotice.el.addEventListener("animationend", (e) => { if (e.target === e.currentTarget) e.target.style.animation = "none"; });
  }
  return fsNotice.el;
}

/// A plain-text summary of the scan, to send when asking for help.
function scanDetails() {
  const count = (o) => S.items.filter((i) => i && i.origin === o).length;
  const lines = [
    `Reperio scan of ${S.source?.label || "?"} (${size(S.source?.size || 0)}, ${S.source?.is_disk ? medium() : "image"})`,
    `Scan: ${S.scan.deep ? "deep" : "quick"}${S.scan.running ? ", running" : ""}`,
    ...(S.volumes || []).map((v, n) =>
      `Volume ${n + 1}: ${v.fs}${v.label ? ` "${v.label}"` : ""}${v.found ? ` (${v.found})` : ""} at ${v.offset}, ${size(v.len)}, ` +
      (v.readable ? `read: ${v.files} files (${v.deleted} deleted), ${size(v.free)} free` : `not readable${v.signature ? ` [${v.signature}]` : ""}`) +
      (v.names ? `; reused records: ${v.names.logfile || 0} whole from $LogFile, ${v.names.journal || 0} names from $UsnJrnl, ${v.names.index || 0} from $I30 slack, ${v.names.named_finds || 0} deep-scan finds named` : "")),
    (S.volumes || []).length ? "" : "No volumes found",
    `Found: ${count("deleted")} with names, ${count("carved")} by deep scan, ${count("existing")} still there`,
    S.scan.running ? `At ${size(S.scan.done)} of ${size(S.scan.total)} (${S.scan.phase})` : "",
  ];
  return lines.filter(Boolean).join("\n");
}

async function copyDetails() {
  const text = scanDetails();
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const t = h("textarea", { text, style: "position:fixed;opacity:0" });
    document.body.append(t);
    t.select();
    document.execCommand("copy");
    t.remove();
  }
  toast("Scan details copied. Paste them into a message to get help.");
}

/// After a quick scan: the offer to search deeper.
function deeperBanner() {
  return h("div.deeper", {},
    h("div", {},
      h("b", { text: "Not finding what you're looking for?" }),
      h("span", { text: `These are the deleted files the ${medium()}'s file list still remembers. A deep scan also searches every free part of the ${medium()}, which finds files after a format or deleted long ago. On a large ${medium()} it can take a while; you can stop it and continue later.` })),
    h("button.btn.primary", { on: { click: deepScan } }, "Deep scan"));
}

function deepScan() {
  S.touchedSelection = false;
  startScan(true);
}

function updateList() {
  const scroller = document.getElementById("scroller");
  const top = scroller.scrollTop;
  const list = visible();
  currentList = list;
  const animate = S.animate && !MOTION.reduced;
  S.animate = false;
  if (!list.length) {
    const scanning = S.scan.running;
    // Keep the message that is already showing: rebuilding it on every
    // refresh during a scan replays its fade-in and restarts the spinner.
    const kind = scanning ? "scanning" : S.items.length ? "nomatch" : "none";
    const shownEmpty = scroller.querySelector(":scope > .empty");
    if (shownEmpty && shownEmpty.dataset.kind === kind) return;
    scroller.replaceChildren(h("div.empty", { dataset: { kind } }, h("div", {},
      scanning ? h("div.spinner", { style: "margin:0 auto 14px;width:28px;height:28px" })
        : h("div.empty-art", {}, h("span.ring"), h("span.ring.two"), icon(S.items.length ? "search" : "logo", "mark")),
      h("h3", { text: scanning ? "Still looking" : S.items.length ? "Nothing matches" : "No deleted files found" }),
      h("p", { text: scanning ? "Files appear here as they are found."
        : S.items.length ? "Try another category, or clear the search."
        : S.scan.deep === false ? `The ${medium()}'s file list doesn't remember any deleted files. A deep scan searches every free part of the ${medium()} for them.`
        : `If the ${medium()} was used a lot after the files were deleted, they may have been overwritten.` }),
      !scanning && S.scan.deep === false && h("button.btn.primary", { style: "margin-top:16px", on: { click: deepScan } }, "Deep scan"))));
    return;
  }
  const shown = list.slice(0, S.limit);
  const nodes = [];
  let n = 0;
  if (!S.scan.running && S.scan.deep === false) nodes.push(deeperBanner());
  const notice = fsNotice();
  if (notice) nodes.push(notice);
  const trim = trimNotice();
  if (trim) nodes.push(trim);
  if (S.mode === "grid") {
    for (const [title, items] of groups(shown)) {
      if (title) {
        const ids = expand(items.map((i) => i.id));
        nodes.push(h("div.group-head", {},
          h("h3", { text: title }), h("span.n", { text: plural(items.length, "file") }),
          h("button.sel", { text: ids.every((id) => S.selected.has(id)) ? "Deselect" : "Select all", on: { click: (e) => { e.stopPropagation(); toggleMany(ids); } } })));
      }
      const grid = h("div.grid");
      for (const i of items) {
        const el = card(i);
        showSel(el, i.id);
        stackBadge(el, i);
        el.classList.toggle("in", animate && n < 40);
        if (animate && n < 40) el.style.animationDelay = `${n * 22}ms`;
        n++;
        grid.append(el);
      }
      nodes.push(grid);
    }
  } else {
    nodes.push(h("table.list", {}, h("tbody", {}, shown.map(row))));
  }
  if (list.length > shown.length) {
    const more = h("div", { style: "height:40px" });
    nodes.push(more);
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) { io.disconnect(); S.limit += 400; updateList(); }
    }, { root: scroller, rootMargin: "600px" });
    io.observe(more);
  }
  scroller.replaceChildren(...nodes);
  scroller.scrollTop = animate ? 0 : top;
}

/// The floating bar is built once and updated in place, so its spinner
/// keeps turning smoothly through the refreshes a scan sends.
let bar = null;

function updateBottom() {
  const root = document.getElementById("bottombar");
  if (!bar || !root.contains(bar.summary)) {
    bar = {
      chip: h("span.scanning-chip", {}, h("span.spinner"), h("span")),
      progress: h("button.btn.ghost", { on: { click: renderScan } }, "Progress"),
      summary: h("span.summary"),
      all: h("button.btn.ghost", { on: { click: () => toggleMany(expand(currentList.map((i) => i.id))) } }),
      recover: h("button.btn.primary.large", { on: { click: openRecover } }),
    };
    root.replaceChildren(bar.chip, bar.progress, bar.summary, h("div.flex"), bar.all, bar.recover);
  }
  const sel = S.items.filter((i) => i && S.selected.has(i.id));
  const bytes = sel.reduce((a, i) => a + i.size, 0);
  const visibleIds = expand(currentList.map((i) => i.id));
  const allOn = visibleIds.length > 0 && visibleIds.every((id) => S.selected.has(id));
  const running = S.scan.running;
  bar.chip.hidden = !running;
  bar.progress.hidden = !running;
  bar.chip.lastChild.textContent = `${Math.floor(scanProgress() * 100)}%`;
  setKids(bar.summary, sel.length ? [h("b", { text: plural(sel.length, "file") }), ` selected · ${size(bytes)}`] : "Nothing selected");
  bar.all.hidden = visibleIds.length === 0;
  bar.all.textContent = allOn ? "Deselect all" : "Select all";
  bar.recover.disabled = sel.length === 0 || running;
  bar.recover.title = running ? "Wait for the scan to finish, or stop it" : "";
  bar.recover.textContent = sel.length ? `Recover ${plural(sel.length, "file")}` : "Recover";
}

function toggleMany(ids) {
  const allOn = ids.every((id) => S.selected.has(id));
  for (const id of ids) allOn ? S.selected.delete(id) : S.selected.add(id);
  S.touchedSelection = true;
  updateList();
  updateBottom();
}

function idFrom(e) {
  const el = e.target.closest("[data-id]");
  return el ? Number(el.dataset.id) : null;
}

function onGridClick(e) {
  const id = idFrom(e);
  if (id == null) return;
  if (e.shiftKey && S.lastClicked != null) {
    const ids = currentList.map((i) => i.id);
    const a = ids.indexOf(S.lastClicked), b = ids.indexOf(id);
    if (a >= 0 && b >= 0) for (const x of expand(ids.slice(Math.min(a, b), Math.max(a, b) + 1))) S.selected.add(x);
  } else {
    // A card standing for a group selects the whole group, or none of it.
    const on = selState(id) !== "all";
    for (const x of idsOf(id)) on ? S.selected.add(x) : S.selected.delete(x);
  }
  S.lastClicked = id;
  S.touchedSelection = true;
  const el = e.target.closest("[data-id]");
  if (el.tagName === "TR") el.className = { all: "selected", some: "partial", none: "" }[selState(id)];
  else showSel(el, id);
  if (e.shiftKey) updateList();
  updateBottom();
}

/// Keyboard use of the grid: arrows move between files, space selects,
/// Enter opens the preview.
function onGridKey(e) {
  const card = e.target.closest?.(".card");
  if (!card) return;
  const cards = [...document.querySelectorAll("#scroller .card")];
  const at = cards.indexOf(card);
  // Rows are justified, so up and down go to the card nearest in the row above or below.
  const vertical = (dir) => {
    const others = cards.filter((c) => (dir > 0 ? c.offsetTop > card.offsetTop + 4 : c.offsetTop < card.offsetTop - 4));
    if (!others.length) return 0;
    const rowTop = dir > 0 ? Math.min(...others.map((c) => c.offsetTop)) : Math.max(...others.map((c) => c.offsetTop));
    const cx = card.offsetLeft + card.offsetWidth / 2;
    const mid = (c) => c.offsetLeft + c.offsetWidth / 2;
    const row = others.filter((c) => Math.abs(c.offsetTop - rowTop) <= 4);
    const best = row.reduce((p, c) => (Math.abs(mid(c) - cx) < Math.abs(mid(p) - cx) ? c : p));
    return cards.indexOf(best) - at;
  };
  const move = { ArrowRight: 1, ArrowLeft: -1 }[e.key] ?? (e.key === "ArrowDown" ? vertical(1) : e.key === "ArrowUp" ? vertical(-1) : 0);
  if (move) {
    e.preventDefault();
    cards[Math.min(cards.length - 1, Math.max(0, at + move))]?.focus();
  } else if (e.key === " ") {
    e.preventDefault();
    card.click();
  } else if (e.key === "Enter") {
    e.preventDefault();
    openItem(Number(card.dataset.id));
  }
}

function onGridDblClick(e) {
  const id = idFrom(e);
  if (id != null) openItem(id);
}

/// Opens a file in the viewer, or a group's card in the group sheet.
function openItem(id) {
  if (S.stacks.has(id)) openGroup(id);
  else openViewer(id);
}

/// The back arrow: back to the scan while one is running (it keeps going),
/// otherwise back to the drives.
function resultsBack() {
  if (S.scan.running) renderScan();
  else goHome();
}

function goHome() {
  if (S.scan.running) {
    confirmStop(() => { invoke("cancel"); renderHome(); refreshDisks(); });
    return;
  }
  renderHome();
  refreshDisks();
}

/// Asks before throwing away a running scan.
function confirmStop(stop) {
  if (!settings.confirmStop) { stop(); return; }
  const o = overlay(h("div.sheet", {},
    h("h2", { text: "Stop the scan?" }),
    h("p.lead", { text: "Files the scan hasn't reached yet won't be listed, and you'd need to scan again to find them." }),
    h("div.actions", {},
      h("button.btn", { on: { click: () => { o.close(); stop(); } } }, "Stop scan"),
      h("button.btn.primary", { on: { click: () => o.close() } }, "Keep scanning"))));
}

async function saveResults() {
  try {
    const path = await invoke("save_results");
    if (path) toast("Scan saved. Open it later to recover without scanning again.");
  } catch (e) {
    toast(errText(e), true);
  }
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

const HEALTH = {
  good: ["", "Complete.", "Checked from start to end: this file is intact."],
  damaged: ["bad", "Damaged.", "Part of this file could not be found. It may open partly, or not at all."],
  overwritten: ["warn", "Partly overwritten.", "Newer files reused some of this file's space, so part of it is lost."],
  repaired: ["", "Repaired.", "Reperio rebuilt this file so it opens."],
  unknown: ["unknown", "Not checked.", `Reperio can't check this kind of file, but its data is where the ${medium()} says it should be.`],
};

/// The viewer's explanation of a repaired file, by what was done to it.
function repairedText(it, fallback) {
  const f = it.fix || {};
  if (f.kind === "video") {
    const how = f.reference_name
      ? `using ${f.reference_name}, from the same camera, as a guide`
      : `from the settings the camera repeats inside the video${f.camera ? ` (${f.camera})` : ""}`;
    return `This video's index was missing, so it wouldn't play. Reperio rebuilt it ${how}. ${f.silent ? "It plays without sound." : "Its sound was found and rebuilt too."}`;
  }
  if (f.kind === "photo") {
    return {
      header: "This photo's header was lost, so it wouldn't open. Reperio put back the header of a photo from the same camera.",
      partial: "Only part of this photo survived. Reperio kept that part; the rest shows grey.",
      preview: "This photo is too damaged to repair, so Reperio rescued the smaller preview stored inside it.",
      heic_partial: "Only the top of this photo survived. Reperio kept it as a shorter photo.",
      heic_thumbnail: "This photo is too damaged to repair, so Reperio rescued the thumbnail stored inside it.",
    }[f.how] || fallback;
  }
  if (f.kind === "document") {
    if (f.how === "text") return "This document is too damaged to repair, so Reperio rescued its text as a plain text file.";
    return `This document wouldn't open because its ${f.how === "pdf" ? "table of contents" : "index"} was damaged. Reperio rebuilt it.${f.has_text ? " Text from the parts that could not be rebuilt is saved beside it as a .txt file." : ""}`;
  }
  return fallback;
}

/// The viewer's report of a file being copied off the drive to be shown.
let onPreviewProgress = null;
listen("preview-progress", (p) => onPreviewProgress?.(p));

/// `list`: the files the arrows move through; the results by default.
function openViewer(id, list = currentList) {
  let index = list.findIndex((i) => i.id === id);
  if (index < 0) return;
  // Where the picture grows from: the card it was opened from.
  const origin = document.querySelector(`#scroller .card[data-id="${id}"]`)?.getBoundingClientRect();
  let firstOpen = true;
  // Opened from a group's sheet, it goes over the sheet.
  const viewer = h("div.viewer" + (list === currentList ? "" : ".over"), { tabindex: -1 });
  document.body.append(viewer);
  let token = 0;
  let zoom = null;

  /// Stops sound and frees the copy being played before showing the next.
  const stopMedia = () => {
    for (const m of viewer.querySelectorAll("video, audio")) {
      m.pause();
      m.removeAttribute("src");
      m.load();
    }
  };
  const close = () => {
    document.removeEventListener("keydown", onKey, true);
    onPreviewProgress = null;
    token++;
    stopMedia();
    invoke("cancel_preview").catch(() => {});
    viewer.classList.add("closing");
    setTimeout(() => viewer.remove(), MOTION.reduced ? 0 : 250);
    if (list === currentList) {
      updateList();
      updateBottom();
    } else {
      list.onChange?.();
    }
  };
  const go = (d) => { const n = index + d; if (n >= 0 && n < list.length) { index = n; render(); } };
  const onKey = (e) => {
    if (e.key === "Escape") { e.preventDefault(); close(); }
    else if (e.key === "ArrowLeft") { e.preventDefault(); go(-1); }
    else if (e.key === "ArrowRight") { e.preventDefault(); go(1); }
    else if (e.key === " ") { e.preventDefault(); toggleSel(); }
    else if (zoom && zoom.key(e)) e.preventDefault();
  };
  document.addEventListener("keydown", onKey, true);

  // In the results, a card standing for a group selects all of it.
  const ids = (it) => (list === currentList ? idsOf(it.id) : [it.id]);
  const toggleSel = () => {
    const it = list[index];
    const on = !ids(it).every((x) => S.selected.has(x));
    for (const x of ids(it)) on ? S.selected.add(x) : S.selected.delete(x);
    S.touchedSelection = true;
    render(true);
  };

  async function render(panelOnly) {
    const it = list[index];
    let [cls, title, text] = HEALTH[it.health] || HEALTH.unknown;
    if (it.health === "repaired") text = repairedText(it, text);
    const taken = it.taken ? fmtDateTime.format(parseDate(it.taken)) : null;
    const modified = it.modified ? fmtDateTime.format(parseDate(it.modified)) : null;
    const where = it.origin === "carved" ? "Found by deep scan (original name unknown)" : (it.dir ? it.dir + "/" : "/") + (it.origin === "existing" ? ` (still on the ${medium()})` : "");
    const group = list === currentList && S.stacks.get(it.id);
    const dims = S.groupOf.get(it.id)?.members.find((m) => m.id === it.id);
    const on = ids(it).every((x) => S.selected.has(x));
    const panel = h("div.panel", {},
      h("h3", { text: it.name }),
      h("div.health" + (cls ? "." + cls : ""), {}, h("b", { text: title + " " }), text),
      h("dl", {},
        h("dt", { text: "Location" }), h("dd", { text: where }),
        h("dt", { text: "Size" }), h("dd", { text: size(it.size) }),
        dims?.width && [h("dt", { text: "Pixels" }), h("dd", { text: `${dims.width} × ${dims.height}` })],
        taken && [h("dt", { text: "Taken" }), h("dd", { text: taken })],
        modified && [h("dt", { text: "Modified" }), h("dd", { text: modified })],
        h("dt", { text: "Type" }), h("dd", { text: (it.ext || "unknown").toUpperCase() }),
        it.extents.length > 1 && [h("dt", { text: "Stored" }), h("dd", { text: `In ${it.extents.length} pieces; Reperio rejoined them.` })],
        h("dt", { text: "Chance" }), h("dd", {}, chanceMeter(it), h("div.chance-why", { text: chance(it)[2] }))),
      group && h("button.group-btn", { on: { click: () => { close(); openGroup(it.id); } } },
        icon("grid"), `See all ${group.length} similar`),
      h("button.select-btn" + (on ? ".on" : ""), { on: { click: toggleSel } },
        on ? "Selected · click to remove" : group ? `Select all ${group.length} to recover` : "Select to recover"),
      h("div.keys", { text: `${index + 1} of ${list.length} · ← → to move · space to select · esc to close` }));
    if (panelOnly) { viewer.querySelector(".panel").replaceWith(panel); return; }

    const t = ++token;
    stopMedia();
    zoom = null;
    const status = h("div.loading-text", { text: "Opening…" });
    const bar = h("div.progress", { hidden: true }, h("div"));
    const media = h("div.media", {}, h("div.loading", {}, h("div.spinner", { style: "width:26px;height:26px" }), status, bar));
    const tools = h("div.vtools", {},
      h("button.vbtn", { title: "Show the file's raw bytes", on: { click: () => { stopMedia(); zoom = null; stage.querySelector(".ztools")?.remove(); showBytes(it, media); } } }, "Bytes"),
      h("button.vbtn", { on: { click: close } }, icon("x"), "Close"));
    // A strip of neighbours, to move along without going back to the grid.
    const from = Math.max(0, Math.min(index - 4, list.length - 9)), to = Math.min(list.length, from + 9);
    const strip = list.length > 1 && h("div.filmstrip", { role: "listbox", "aria-label": "Nearby files" },
      list.slice(from, to).map((n, k) => h("button.film" + (from + k === index ? ".on" : ""), {
        title: n.name, "aria-label": n.name, "aria-selected": String(from + k === index),
        on: { click: () => { index = from + k; render(); } },
      }, thumbable(n)
        ? h("img", { src: thumbUrl(n.id), alt: "", draggable: false, on: { error: (e) => e.target.replaceWith(icon(KIND_ICON[n.kind] || "file")) } })
        : icon(KIND_ICON[n.kind] || "file"))));
    const stage = h("div.stage", {},
      media,
      strip,
      tools,
      h("button.nav.prev", { title: "Previous", on: { click: () => go(-1) }, disabled: index === 0 }, icon("back")),
      h("button.nav.next", { title: "Next", on: { click: () => go(1) }, disabled: index === list.length - 1 }, icon("next")));
    viewer.replaceChildren(stage, panel);
    if (firstOpen && origin && !MOTION.reduced) {
      firstOpen = false;
      const dx = origin.left + origin.width / 2 - (innerWidth - 320) / 2, dy = origin.top + origin.height / 2 - innerHeight / 2;
      media.style.setProperty("--dx", `${dx}px`);
      media.style.setProperty("--dy", `${dy}px`);
      media.classList.add("from-card");
    }
    viewer.focus();
    // Videos are copied off the drive before they play: say how far.
    onPreviewProgress = (p) => {
      if (t !== token || p.id !== it.id) return;
      status.textContent = `Copying the ${it.kind === "video" ? "video" : "file"} from the ${medium()} to play it · ${size(p.done)} of ${size(p.total)}`;
      bar.hidden = false;
      bar.firstChild.style.width = `${(100 * p.done) / Math.max(1, p.total)}%`;
    };
    let p;
    try {
      p = await invoke("preview", { id: it.id });
    } catch (e) {
      if (t === token) media.replaceChildren(noPreview(it, errText(e)));
      return;
    }
    if (t !== token) return;
    onPreviewProgress = null;
    const fail = (msg) => () => { if (t === token) { zoom = null; stage.querySelector(".ztools")?.remove(); media.className = "media"; media.replaceChildren(noPreview(it, msg)); } };
    if (p.kind === "image" || p.kind === "thumb") {
      const src = p.kind === "image" ? fileUrl(p.path) : thumbUrl(it.id);
      zoom = zoomable(src, it.name, fail(p.note || "This file could not be shown. It may be damaged."));
      media.className = "media zoomable";
      media.replaceChildren(zoom.el);
      stage.append(zoom.controls);
      if (p.note) stage.append(h("div.vnote", { text: p.note }));
    } else if (p.kind === "video") {
      media.replaceChildren(h("video", { src: p.url || fileUrl(p.path), controls: true, autoplay: true, playsInline: true, preload: "auto",
        on: { error: fail("This video could not be played here. It may be damaged, or be in a format this computer can't play. It can still be recovered.") } }));
    } else if (p.kind === "audio") {
      media.replaceChildren(h("div.audio-card", {},
        icon("audio"),
        h("div.audio-name", { text: it.name }),
        h("audio", { src: p.url || fileUrl(p.path), controls: true, autoplay: true,
          on: { error: fail("This recording could not be played here. It may be damaged. It can still be recovered.") } })));
    } else if (p.kind === "pdf") {
      media.className = "media doc";
      media.replaceChildren(h("iframe.pdf", { src: fileUrl(p.path), title: it.name }));
    } else if (p.kind === "text") {
      media.className = "media doc";
      media.replaceChildren(h("div.textwrap", {}, p.note && h("div.textnote", { text: p.note }), h("pre", { text: p.text })));
    } else {
      media.replaceChildren(noPreview(it, p.note || "There's no preview for this kind of file, but it can be recovered."));
    }
  }

  /// The file's raw bytes, for checking what is really there.
  async function showBytes(it, media) {
    media.className = "media";
    const pre = h("pre.hex");
    const more = h("button.vbtn", { text: "Show more" });
    let offset = 0;
    const load = async () => {
      const bytes = await invoke("item_bytes", { id: it.id, offset, len: 4096 }).catch(() => []);
      const lines = [];
      for (let i = 0; i < bytes.length; i += 16) {
        const row = bytes.slice(i, i + 16);
        const hex = row.map((b) => b.toString(16).padStart(2, "0")).join(" ").padEnd(47, " ");
        const text = row.map((b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("");
        lines.push(`${(offset + i).toString(16).padStart(8, "0")}  ${hex}  ${text}`);
      }
      pre.textContent += (pre.textContent ? "\n" : "") + lines.join("\n");
      offset += bytes.length;
      more.hidden = bytes.length < 4096 || offset >= it.size;
    };
    more.addEventListener("click", load);
    media.replaceChildren(h("div.hexwrap", {}, pre, more));
    await load();
  }

  function noPreview(it, msg) {
    return h("div.placeholder", {}, icon(KIND_ICON[it.kind] || "file"), h("div", { text: msg }));
  }

  render();
}

/// A picture that fits the stage, and zooms: to 100% and back with the
/// buttons or a double-click, by pinching a trackpad or with ctrl and the
/// wheel, around the pointer. Drag (or scroll) to move around it.
function zoomable(src, alt, onFail) {
  const img = h("img", { alt, draggable: false });
  const box = h("div.zoombox", {}, img);
  const label = h("span.zlabel");
  const fitBtn = h("button", { text: "Fit", title: "Fit to the window (0)" });
  const fullBtn = h("button", { text: "100%", title: "Actual size (1)" });
  const controls = h("div.ztools", { hidden: true }, h("div.zseg", {}, fitBtn, fullBtn), label);
  const z = { s: 1, fit: 1, x: 0, y: 0, w: 0, h: 0, fitted: true };

  const clamp = () => {
    const mx = Math.max(0, (z.w * z.s - box.clientWidth) / 2);
    const my = Math.max(0, (z.h * z.s - box.clientHeight) / 2);
    z.x = Math.min(mx, Math.max(-mx, z.x));
    z.y = Math.min(my, Math.max(-my, z.y));
  };
  const apply = () => {
    clamp();
    img.style.transform = `translate(-50%, -50%) translate(${z.x}px, ${z.y}px) scale(${z.s})`;
    label.textContent = `${Math.round(z.s * 100)}%`;
    fitBtn.classList.toggle("on", z.fitted);
    fullBtn.classList.toggle("on", Math.abs(z.s - 1) < 0.001);
    box.classList.toggle("pannable", z.w * z.s > box.clientWidth + 1 || z.h * z.s > box.clientHeight + 1);
  };
  const measure = () => {
    if (!z.w || !box.clientWidth) return;
    // Small pictures show at their real size, not blown up.
    z.fit = Math.min(1, box.clientWidth / z.w, box.clientHeight / z.h);
    if (z.fitted) { z.s = z.fit; z.x = z.y = 0; }
    apply();
  };
  /// Zooms to `s`, keeping the point (cx, cy) from the box's centre still.
  const zoomTo = (s, cx = 0, cy = 0) => {
    s = Math.min(8, Math.max(Math.min(z.fit, 1) * 0.5, s));
    z.x = cx - ((cx - z.x) * s) / z.s;
    z.y = cy - ((cy - z.y) * s) / z.s;
    z.s = s;
    z.fitted = Math.abs(s - z.fit) < 0.001;
    apply();
  };
  const fit = () => { z.fitted = true; measure(); };
  const point = (e) => {
    const r = box.getBoundingClientRect();
    return [e.clientX - r.left - r.width / 2, e.clientY - r.top - r.height / 2];
  };

  img.addEventListener("load", () => {
    z.w = img.naturalWidth;
    z.h = img.naturalHeight;
    img.style.width = z.w + "px";
    img.style.height = z.h + "px";
    controls.hidden = false;
    measure();
    img.classList.add("loaded");
  });
  img.addEventListener("error", onFail);
  // Dragging pans the picture; it must not start a drag of the image.
  box.addEventListener("dragstart", (e) => e.preventDefault());
  fitBtn.addEventListener("click", fit);
  fullBtn.addEventListener("click", () => zoomTo(1));
  box.addEventListener("dblclick", (e) => (z.fitted && z.fit < 1 ? zoomTo(1, ...point(e)) : fit()));
  box.addEventListener("wheel", (e) => {
    e.preventDefault();
    // Pinching a trackpad arrives as a wheel with ctrl held.
    if (e.ctrlKey || e.metaKey) zoomTo(z.s * Math.exp(-e.deltaY * 0.01), ...point(e));
    else { z.x -= e.deltaX; z.y -= e.deltaY; apply(); }
  }, { passive: false });
  // Safari's own pinch events, which WebKit on macOS sends.
  let pinchFrom = 1;
  box.addEventListener("gesturestart", (e) => { e.preventDefault(); pinchFrom = z.s; });
  box.addEventListener("gesturechange", (e) => { e.preventDefault(); zoomTo(pinchFrom * e.scale, ...point(e)); });
  let drag = null;
  box.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || !box.classList.contains("pannable")) return;
    drag = { x: e.clientX - z.x, y: e.clientY - z.y };
    box.setPointerCapture(e.pointerId);
    box.classList.add("dragging");
  });
  box.addEventListener("pointermove", (e) => {
    if (!drag) return;
    z.x = e.clientX - drag.x;
    z.y = e.clientY - drag.y;
    apply();
  });
  const endDrag = () => { drag = null; box.classList.remove("dragging"); };
  box.addEventListener("pointerup", endDrag);
  box.addEventListener("pointercancel", endDrag);
  new ResizeObserver(measure).observe(box);
  img.src = src;

  return {
    el: box,
    controls,
    /// Keys: + and - zoom, 0 fits, 1 is actual size. True when used.
    key(e) {
      if (e.metaKey || e.ctrlKey || e.altKey) return false;
      if (e.key === "+" || e.key === "=") zoomTo(z.s * 1.25);
      else if (e.key === "-") zoomTo(z.s / 1.25);
      else if (e.key === "0") fit();
      else if (e.key === "1") zoomTo(1);
      else return false;
      return true;
    },
  };
}

/// A group's sheet: its members side by side, best first, to choose which
/// to recover. Double-click one to look at it closely.
function openGroup(id) {
  const ids = idsOf(id);
  const group = S.groupOf.get(id);
  const members = ids.map(byId).filter(Boolean);
  const dims = new Map((group?.members || []).map((m) => [m.id, m]));
  const photos = members.every((i) => i.kind === "photo" || i.kind === "raw");
  const sheet = h("div.sheet.wide.group-sheet");
  const o = overlay(sheet, { onClose: () => { document.removeEventListener("keydown", onKey, true); updateList(); updateBottom(); } });
  const onKey = (e) => {
    if (document.querySelector(".viewer")) return;
    if (e.key === "Escape") { e.preventDefault(); o.close(); }
  };
  document.addEventListener("keydown", onKey, true);
  const list = members.slice();
  list.onChange = () => draw();

  function tile(i, n) {
    const d = dims.get(i.id);
    const on = S.selected.has(i.id);
    return h("div.gtile" + (on ? ".selected" : ""), { tabindex: 0, role: "checkbox", "aria-checked": String(on), "aria-label": i.name, dataset: { id: i.id },
      on: {
        click: () => { on ? S.selected.delete(i.id) : S.selected.add(i.id); S.touchedSelection = true; draw(); },
        dblclick: () => openViewer(i.id, list),
        keydown: (e) => {
          if (e.key === " ") { e.preventDefault(); e.currentTarget.click(); }
          else if (e.key === "Enter") { e.preventDefault(); openViewer(i.id, list); }
        },
      } },
      h("div.thumb", {},
        thumbable(i) ? h("img", { src: thumbUrl(i.id), alt: "", draggable: false, on: { load: (e) => e.target.classList.add("loaded"), error: (e) => e.target.replaceWith(...placeholder(i)) } }) : placeholder(i),
        h("div.check", {}, icon("check")),
        n === 0 && h("div.best", { text: "Best" })),
      h("div.meta", {},
        h("div.name", { text: i.name, title: i.dir ? `${i.dir}/${i.name}` : i.name }),
        h("div.info", { text: [d?.width && `${d.width} × ${d.height}`, size(i.size)].filter(Boolean).join(" · ") }),
        h("div.info", {}, chanceMeter(i))));
  }

  function draw() {
    const chosen = members.filter((i) => S.selected.has(i.id)).length;
    const all = chosen === members.length;
    sheet.replaceChildren(
      h("h2", { text: photos ? `${members.length} similar photos` : `${members.length} copies of the same file` }),
      h("p.lead", { text: `Reperio put the one it thinks best first: ${photos ? "the largest, " : ""}in the best condition, with its original name where it has one. Choose which to recover; double-click one to look closer.` }),
      h("div.gtiles", {}, members.map(tile)),
      h("div.actions", {},
        h("span.gsum", { text: `${chosen} of ${members.length} selected` }),
        h("div.flex"),
        h("button.btn", { on: { click: () => { for (const i of members) S.selected.delete(i.id); S.selected.add(members[0].id); S.touchedSelection = true; draw(); } } }, "Only the best"),
        h("button.btn", { on: { click: () => { for (const i of members) all ? S.selected.delete(i.id) : S.selected.add(i.id); S.touchedSelection = true; draw(); } } }, all ? "Select none" : "Select all"),
        h("button.btn.primary", { on: { click: () => o.close() } }, "Done")));
  }
  draw();
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

function openRecover() {
  const chosen = S.items.filter((i) => i && S.selected.has(i.id));
  const ids = chosen.map((i) => i.id);
  const bytes = chosen.reduce((a, i) => a + i.size, 0);
  const r = { dest: null, free: null, layout: settings.layout, error: null, running: false };
  const sheet = h("div.sheet.wide");
  const o = overlay(sheet, { onClose: () => document.removeEventListener("keydown", onKey, true) });
  const onKey = (e) => { if (e.key === "Escape" && !r.running) o.close(); };
  document.addEventListener("keydown", onKey, true);

  const LAYOUTS = [
    ["original", "Original folders and names", "Files found by deep scan go in their own folder, sorted by type."],
    ["by_date", "By date", "Photos/2004, Videos/2006 and so on, ready for a photo library."],
    ["by_type", "By type", "Photos, Videos, Documents and so on."],
  ];

  function setup() {
    const lowSpace = r.free != null && r.free < bytes * 1.02;
    sheet.replaceChildren(
      h("h2", { text: `Recover ${plural(ids.length, "file")}` }),
      h("p.lead", { text: `${size(bytes)} will be copied. Choose a place on a different drive from the one you're recovering.` }),
      h("div.field", {},
        h("label", { text: "Save to" }),
        h("div.dest", {},
          icon("folder"),
          h("div.path" + (r.dest ? ".set" : ""), { text: r.dest ? `\u200e${r.dest}\u200e` : "No folder chosen" }),
          h("button.btn", { on: { click: choose } }, r.dest ? "Change…" : "Choose…")),
        r.free != null && h("div.hint", { text: `${size(r.free)} free there.` }),
        lowSpace && h("div.error", { text: "There isn't enough free space there. Choose another drive." }),
        r.error && h("div.error", { text: r.error })),
      h("div.field", {},
        h("label", { text: "Arrange files" }),
        h("div.choices", {}, LAYOUTS.map(([k, title, sub]) =>
          h("div.choice" + (r.layout === k ? ".on" : ""), { on: { click: () => { r.layout = k; setup(); } } },
            h("div.radio"), h("div", {}, h("b", { text: title }), h("span", { text: sub })))))),
      h("div.actions", {},
        h("button.btn", { on: { click: () => o.close() } }, "Cancel"),
        h("button.btn.primary", { disabled: !r.dest || lowSpace, on: { click: start } }, "Recover")));
  }

  async function choose() {
    try {
      const d = await invoke("choose_destination");
      if (d) { r.dest = d.path; r.free = d.free; r.error = null; }
    } catch (e) {
      r.error = errText(e);
      r.dest = null;
      r.free = null;
    }
    setup();
  }

  let unlisten = [];
  async function start() {
    r.running = true;
    const order = ids.map((id) => S.items.find((i) => i && i.id === id)).filter(Boolean);
    sheet.replaceChildren(
      h("h2", { text: `Bringing back ${plural(ids.length, "file")}` }),
      h("p.lead", { text: `Copying to ${r.dest}` }),
      h("canvas.rec-map", { id: "rec-map" }),
      h("div.rec-now", {},
        h("div.rec-thumb", { id: "rec-thumb" }),
        h("div.rec-info", {},
          h("div.rec-name", { id: "rec-name", text: "Starting" }),
          h("div.rec-sub", { id: "rec-sub", text: "" }))),
      h("div.progress", {}, h("div", { id: "rec-bar" })),
      h("div.progress-text", {}, h("span", { id: "rec-speed", text: "" }), h("span", { id: "rec-pct", text: "0%" })),
      h("div.actions", {}, h("button.btn", { on: { click: () => invoke("cancel") } }, "Stop")));

    // The map stands for the bytes being recovered, file after file, so it
    // fills steadily however the files are spread on the card.
    const ends = [];
    let sum = 0;
    const spans = order.map((it) => {
      const span = { kind: it.kind, extents: [{ offset: sum, len: Math.max(1, it.size) }] };
      ends.push((sum += it.size));
      return span;
    });
    const map = new DotMap(document.getElementById("rec-map"), sum || 1, { mode: "recover", pitch: 10 });
    for (const sp of spans) map.mark(sp, false);
    let finishedCount = 0;
    let current = -1;
    const started = performance.now();

    unlisten.push(await listen("recover-progress", (p) => {
      const bar = document.getElementById("rec-bar");
      if (!bar) return;
      const pct = p.total ? Math.floor((p.done / p.total) * 100) : 0;
      bar.style.width = pct + "%";
      document.getElementById("rec-pct").textContent = `${pct}% · ${size(p.done)} of ${size(p.total)}`;
      const secs = (performance.now() - started) / 1000;
      if (secs > 0.5) {
        const rate = p.done / secs;
        const left = rate > 0 ? (p.total - p.done) / rate : 0;
        document.getElementById("rec-speed").textContent = `${size(rate)}/s${left > 5 ? ` · ${duration(left)} left` : ""}`;
      }
      while (finishedCount < order.length && ends[finishedCount] <= p.done) map.markCopied(spans[finishedCount++]);
      const idx = Math.min(finishedCount, order.length - 1);
      if (idx !== current && order[idx]) {
        current = idx;
        const it = order[idx];
        map.setActive(spans[idx]);
        document.getElementById("rec-name").textContent = it.name;
        document.getElementById("rec-sub").textContent = `File ${idx + 1} of ${order.length} · ${size(it.size)}`;
        const img = thumbable(it) ? h("img", { src: thumbUrl(it.id), alt: "", on: { error: (e) => e.target.replaceWith(icon(KIND_ICON[it.kind] || "file")) } }) : icon(KIND_ICON[it.kind] || "file");
        document.getElementById("rec-thumb").replaceChildren(img);
      }
    }));
    unlisten.push(await listen("recover-done", finished));
    try {
      await invoke("recover", { ids, dest: r.dest, layout: r.layout });
    } catch (e) {
      finished({ count: 0, dest: r.dest, bad_sectors: 0, error: errText(e) });
    }
  }

  function finished(d) {
    unlisten.forEach((u) => u());
    unlisten = [];
    r.running = false;
    if (d.error) {
      sheet.replaceChildren(
        h("h2", { text: "Recovery stopped" }),
        h("p.lead", { text: d.error }),
        h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Close")));
      return;
    }
    const partial = d.count < ids.length;
    const count = h("span", { text: "0" });
    sheet.classList.add("center");
    // A fan of the photos that came back, so the moment looks like what it is.
    const pics = ids.map((id) => S.items[id]).filter((i) => i && thumbable(i)).slice(0, 7);
    const mosaic = pics.length > 0 && !partial && h("div.mosaic", { "aria-hidden": "true" }, pics.map((it, k) => {
      const mid = (pics.length - 1) / 2;
      return h("div.tile", { style: `--k:${k};--rot:${(k - mid) * 5}deg;--lift:${Math.abs(k - mid) * 5}px;--x:${(k - mid) * 58}px` },
        h("img", { src: thumbUrl(it.id), alt: "", draggable: false, on: { error: (e) => e.target.parentNode?.remove() } }));
    }));
    sheet.replaceChildren(
      mosaic,
      h("div.done-icon", { html: '<svg viewBox="0 0 76 76"><circle cx="38" cy="38" r="30"/><path d="M26 39l8.5 8.5L51 31"/></svg>' }),
      h("h2", {}, count, d.count === 1 ? " file is back" : " files are back", partial ? " (stopped early)" : ""),
      h("p.lead", { style: "max-width:380px;margin:8px auto 0", text: d.bad_sectors > 0
        ? `Some parts of the ${medium()} couldn't be read (${plural(d.bad_sectors, "sector")}), so a few files may be incomplete.`
        : "They're safe on your computer. A report listing every file is in the same folder." }),
      h("div.actions", {},
        h("button.btn.primary", { on: { click: () => invoke("reveal", { path: d.dest }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
        S.source?.is_disk && h("button.btn", { on: { click: ejectCard } }, icon("eject"), `Eject ${medium()}`),
        h("button.btn", { on: { click: () => o.close() } }, "Done")));
    countUp(count, d.count, 1200);
    if (settings.openWhenDone && !partial && d.count > 0) invoke("reveal", { path: d.dest }).catch(() => {});
  }

  async function ejectCard() {
    try {
      await invoke("eject");
      o.close();
      toast(`The ${medium()} can be removed safely.`);
      S.source = null;
      renderHome();
      refreshDisks();
    } catch (e) {
      toast(errText(e), true);
    }
  }

  setup();
}

async function backupDisk() {
  // Optional forensic details, asked before the save dialog: with any of
  // them the copy is hashed and an acquisition report is written beside it.
  const f = { case_number: "", examiner: "", notes: "", verify: false };
  const input = (key, placeholder) => h("input", { placeholder, value: f[key], on: { input: (e) => (f[key] = e.target.value) } });
  const verifySwitch = h("button.switch", { title: "Check the copy against the drive", role: "switch", "aria-checked": "false", on: { click: () => {
    f.verify = !f.verify;
    verifySwitch.classList.toggle("on", f.verify);
    verifySwitch.setAttribute("aria-checked", String(f.verify));
  } } });
  const sheet = h("div.sheet", {});
  let unlisten = [];
  let o = null;
  const finish = () => { unlisten.forEach((u) => u()); unlisten = []; };
  const close = () => { finish(); o?.close(); };

  sheet.append(
    h("h2", { text: `Back up the whole ${medium()}` }),
    h("p.lead", { text: `Every readable sector is copied to an image file on another drive, healthy areas first. Scan the copy afterwards instead of the ${medium()}.` }),
    h("details.forensic", {},
      h("summary", { text: "Forensic report (optional)" }),
      h("p.hint", { text: "Hashes the copy (MD5, SHA-1, SHA-256) and saves a report of how it was made next to it." }),
      input("case_number", "Case number"),
      input("examiner", "Examiner"),
      h("textarea", { placeholder: "Notes", rows: 2, on: { input: (e) => (f.notes = e.target.value) } }),
      h("div.toggle", {}, "Check the copy against the drive afterwards", verifySwitch)),
    h("div.actions", {},
      h("button.btn", { on: { click: close } }, "Cancel"),
      h("button.btn.primary", { on: { click: start } }, "Choose where to save…")));
  o = overlay(sheet, { onClose: finish });

  function showProgress(path) {
    sheet.replaceChildren(
      h("h2", { text: `Backing up the ${medium()}` }),
      h("p.lead", { text: `Every readable sector is copied to ${path}, healthy areas first, so a failing ${medium()} gives up as much as possible before it gets worse. If it stops, backing up to the same file again continues where it left off.` }),
      h("div.rec-name", { id: "bk-pass", style: "margin-top:18px", text: "Starting" }),
      h("div.progress", {}, h("div", { id: "bk-bar" })),
      h("div.progress-text", {}, h("span", { id: "bk-speed", text: "" }), h("span", { id: "bk-pct", text: "0%" })),
      h("div.actions", {}, h("button.btn", { on: { click: () => invoke("cancel") } }, "Stop")));
  }

  function showReport(d) {
    const verdict = d.verified == null ? null
      : d.verified ? h("p.verdict.good", { text: `Checked: reading the ${medium()} again gives the same hashes.` })
      : h("p.verdict.bad", { text: `The ${medium()} read again does not match the copy. The report lists where.` });
    sheet.replaceChildren(
      h("h2", { text: "Backup saved" }),
      h("p.lead", { text: d.bad_sectors
        ? `${size(d.bad_bytes)} could not be read and was filled with zeros; the report lists those sectors.`
        : "Every sector was copied." }),
      h("div.hashes", {}, Object.entries(d.hashes).map(([k, v]) => h("div", {}, h("b", { text: { md5: "MD5", sha1: "SHA-1", sha256: "SHA-256" }[k] || k }), h("span", { text: v })))),
      verdict,
      h("p.hint", { text: `Report: ${d.report}` }),
      h("div.actions", {},
        h("button.btn", { on: { click: () => invoke("reveal", { path: d.report }) } }, icon("folder"), S.platform === "macos" ? "Show in Finder" : "Open folder"),
        h("button.btn.primary", { on: { click: close } }, "Done")));
  }

  async function start() {
    try {
      unlisten.push(await listen("backup-progress", (p) => {
        const bar = document.getElementById("bk-bar");
        if (!bar) return;
        const pct = Math.floor((p.done / p.total) * 100);
        bar.style.width = pct + "%";
        document.getElementById("bk-pct").textContent = `${pct}% · ${size(p.done)} of ${size(p.total)}`;
        const pass = { copy: "Copying the readable parts first", trim: "Going back to the hard-to-read parts", scrape: "Reading damaged areas sector by sector", retry: "Trying unreadable sectors again", finished: "Finishing" }[p.pass] || "Copying";
        document.getElementById("bk-pass").textContent = pass;
        document.getElementById("bk-speed").textContent = [p.rate ? `${size(p.rate)}/s` : "", p.bad ? `${size(p.bad)} unreadable so far` : ""].filter(Boolean).join(" · ");
      }));
      unlisten.push(await listen("backup-check", (c) => {
        const bar = document.getElementById("bk-bar");
        if (!bar) return;
        const pct = c.total ? Math.floor((c.done / c.total) * 100) : 100;
        bar.style.width = pct + "%";
        document.getElementById("bk-pct").textContent = `${pct}% · ${size(c.done)} of ${size(c.total)}`;
        document.getElementById("bk-pass").textContent = c.stage === "verifying" ? `Hashing the copy and checking it against the ${medium()}` : "Hashing the copy";
        document.getElementById("bk-speed").textContent = "";
      }));
      unlisten.push(await listen("backup-done", (d) => {
        finish();
        if (d.error) { toast(d.error, true); close(); }
        else if (d.cancelled) { toast("Backup stopped."); close(); }
        else if (d.hashes && d.report) showReport(d);
        else {
          close();
          toast(d.bad_sectors
            ? `Backup saved; ${size(d.bad_bytes || d.bad_sectors * 512)} could not be read and was filled with zeros. Back up again later to retry those parts.`
            : `Backup saved${d.resumed ? ", continuing where the last one stopped" : ""}. Open it with “Open a disk image…” to scan the copy.`);
        }
      }));
      const wanted = f.verify || [f.case_number, f.examiner, f.notes].some((v) => v.trim());
      const path = await invoke("backup", { forensic: wanted ? f : null });
      if (!path) { finish(); return; }
      showProgress(path);
    } catch (e) {
      finish();
      toast(errText(e), true);
    }
  }
}

// ---------------------------------------------------------------------------
// Drive health: the SMART report and the surface test
// ---------------------------------------------------------------------------

/// How a surface test block read, by its symbol from the engine, worst
/// last: a square shows the worst of the blocks it covers.
const SURFACE_RANK = { _: 0, ".": 1, s: 2, S: 3, "?": 4, X: 5 };
const SURFACE_COLOR = { _: "--map-empty", ".": "--surf-ok", s: "--surf-slow", S: "--surf-veryslow", "?": "--surf-bad", X: "--surf-bad" };

/// The surface test drawn as a grid of squares like the scan's sector
/// map, each an equal slice of the drive. The engine sends a finer map
/// than the grid has squares; each square takes the worst of its part.
class SurfaceGrid {
  static PITCH = 12;
  static GAP = 2;
  static HEIGHT = 156;

  constructor(canvas, total, onLayout) {
    this.canvas = canvas;
    this.total = Math.max(1, total);
    this.onLayout = onLayout;
    this.map = "";
    this.cols = 0;
    this.resize = new ResizeObserver(() => this.draw());
    this.resize.observe(canvas.parentElement);
  }
  set(map) {
    this.map = map || "";
    this.draw();
  }
  draw() {
    const c = this.canvas;
    if (!c.isConnected) { this.resize.disconnect(); return; }
    const { PITCH, GAP, HEIGHT } = SurfaceGrid;
    const w = c.parentElement.clientWidth;
    if (!w) return;
    const cols = Math.max(8, Math.floor((w + GAP) / PITCH));
    const rows = Math.max(4, Math.floor((HEIGHT + GAP) / PITCH));
    const cells = cols * rows;
    if (cols !== this.cols) { this.cols = cols; this.onLayout?.(this.total / cells); }
    const dpr = window.devicePixelRatio || 1;
    const width = cols * PITCH - GAP, height = rows * PITCH - GAP;
    c.style.width = width + "px";
    c.style.height = height + "px";
    if (c.width !== Math.round(width * dpr)) c.width = Math.round(width * dpr);
    if (c.height !== Math.round(height * dpr)) c.height = Math.round(height * dpr);
    const g = c.getContext("2d");
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, width, height);
    const colors = Object.fromEntries(Object.entries(SURFACE_COLOR).map(([k, v]) => [k, cssVar(v)]));
    const n = this.map.length;
    for (let i = 0; i < cells; i++) {
      let worst = "_";
      if (n) {
        const from = Math.floor((i * n) / cells), to = Math.max(from + 1, Math.floor(((i + 1) * n) / cells));
        for (let j = from; j < to && j < n; j++) if (SURFACE_RANK[this.map[j]] > SURFACE_RANK[worst]) worst = this.map[j];
      }
      g.fillStyle = colors[worst] || colors._;
      g.beginPath();
      g.roundRect((i % cols) * PITCH, Math.floor(i / cols) * PITCH, PITCH - GAP, PITCH - GAP, 2.5);
      g.fill();
    }
  }
}

const VERDICT = {
  good: ["No problems reported", "check"],
  watch: ["Showing signs of wear", "info"],
  failing: ["Failing: copy everything off it first", "info"],
  unknown: ["Health not reported", "info"],
};

/// The drive's report as label and value pairs, leaving out what it did
/// not say.
function healthFacts(r) {
  const n = (v) => (v == null ? null : v.toLocaleString());
  const hours = r.power_on_hours == null ? null
    : `${n(r.power_on_hours)} hours${r.power_on_hours >= 8760 ? ` (about ${plural(Math.round(r.power_on_hours / 8760), "year")} switched on)` : ""}`;
  return [
    ["Model", r.model],
    ["Serial number", r.serial],
    ["Firmware", r.firmware],
    ["Capacity", r.capacity && size(r.capacity)],
    ["Connected by", r.interface],
    ["Kind", r.media && (r.media === "ssd" ? "SSD (flash)" : "Hard drive (spinning)")],
    ["Temperature", r.temperature != null && `${r.temperature} °C`],
    ["Time in use", hours],
    ["Times switched on", n(r.power_cycles)],
    ["Replaced sectors", n(r.reallocated_sectors)],
    ["Sectors waiting to be replaced", n(r.pending_sectors)],
    ["Unreadable sectors", n(r.uncorrectable_sectors)],
    ["Wear", r.percentage_used != null && `${r.percentage_used}% of its rated life used`],
    ["Spare blocks left", r.available_spare != null && `${r.available_spare}%${r.available_spare_threshold != null ? ` (critical at ${r.available_spare_threshold}%)` : ""}`],
    ["Data errors", n(r.media_errors)],
  ].filter(([, v]) => v != null && v !== false && v !== "");
}

/// The Drive health sheet, for a disk on the home screen (`disk`) or the
/// disk that is open (no `disk`). The surface test needs the disk open,
/// so from the home screen it opens it first.
async function openDriveHealth(disk) {
  const name = disk ? disk.name : S.source?.label || "This drive";
  const unlisten = [];
  let running = false, grid = null;
  const verdictBox = h("div.verdict.loading", {}, h("div.spinner"), h("span", { text: "Asking the drive…" }));
  const facts = h("dl.facts");
  const notes = h("div");
  const canvas = h("canvas.sectors", { role: "img", "aria-label": "Surface test map" });
  const note = h("span.note");
  const bar = h("div");
  const status = h("span", { text: "" });
  const pct = h("span", { text: "" });
  const summary = h("p.surface-summary", { text: "Reads every part of the drive and times each read. Slow areas show where it is wearing out, often before anything is lost." });
  const startBtn = h("button.btn.primary", { on: { click: () => (running ? invoke("cancel_surface") : start()) } }, "Surface test");
  const sheet = h("div.sheet.wide.health-sheet", {},
    h("h2", { text: "Drive health" }),
    h("p.lead", { text: name }),
    verdictBox,
    facts,
    notes,
    h("div.surface", {},
      h("h3", { text: "Surface test" }),
      summary,
      h("div.mapcard", {},
        h("div.map-wrap", {}, canvas),
        h("div.legend", {},
          [["--surf-ok", "Normal"], ["--surf-slow", "Slow"], ["--surf-veryslow", "Very slow"], ["--surf-bad", "Unreadable"]].map(([v, label]) =>
            h("span", {}, h("i", { style: { background: `var(${v})` } }), label)),
          note)),
      h("div.progress", {}, bar),
      h("div.progress-text", {}, status, pct)),
    h("p.fine", { text: "Reperio only reads. The drive is unmounted while it is tested; a large hard drive can take hours, and you can stop at any time." }),
    h("div.actions", {},
      h("button.btn", { on: { click: () => o.close() } }, "Close"),
      startBtn));
  const o = overlay(sheet, {
    onClose: () => {
      if (running) invoke("cancel_surface");
      unlisten.forEach((u) => u());
    },
  });
  grid = new SurfaceGrid(canvas, disk ? disk.size : S.source?.size || 1, (per) => (note.textContent = `Each square is ${size(per)}`));
  grid.draw();
  if (S.scan.running && !disk) startBtn.disabled = true;

  const show = (u) => {
    grid.set(u.map);
    const s = u.summary;
    const p = u.total ? u.done / u.total : 0;
    bar.style.width = `${Math.floor(p * 100)}%`;
    pct.textContent = `${Math.floor(p * 100)}% · ${size(u.done)} of ${size(u.total)}`;
    const bad = s.unreadable + s.skipped;
    status.textContent = [s.median_rate ? `${size(s.median_rate)}/s usually` : "",
      s.slow ? `${plural(s.slow, "slow block")}` : "", s.very_slow ? `${s.very_slow.toLocaleString()} very slow` : "",
      bad ? `${bad.toLocaleString()} unreadable` : ""].filter(Boolean).join(" · ");
    return { s, bad };
  };
  const finish = (u) => {
    running = false;
    startBtn.textContent = "Test again";
    const { s, bad } = show(u);
    const area = (blocks) => size(blocks * s.block);
    if (u.cancelled) summary.textContent = `Stopped after ${size(u.done)}.`;
    else if (bad) summary.textContent = `${area(bad)} could not be read, in ${plural(s.damaged_areas, "area")}. This drive is failing: back it up first, then recover from the copy.`;
    else if (s.very_slow || s.slow) summary.textContent = `Everything could be read, but ${area(s.slow + s.very_slow)} was slow to read. The drive is struggling there; back it up soon.`;
    else summary.textContent = `All ${size(u.total)} read at a steady speed${s.median_rate ? ` (about ${size(s.median_rate)}/s)` : ""}. No weak areas found.`;
  };
  async function start() {
    try {
      // From the home screen, always open the disk asked about: whatever
      // is open may be another one.
      if (disk) {
        if (S.scan.running) { toast("Wait for the scan to finish, or stop it, before testing a drive.", true); return; }
        const done = busy(`Opening ${disk.name}`, "You may be asked for your password, so Reperio can read the drive directly.", { onCancel: () => invoke("cancel_open") });
        try {
          S.source = await invoke("open_disk", { id: disk.id });
          S.medium = disk.kind === "card" ? "card" : "drive";
        } finally { done(); }
      }
      if (!unlisten.length) {
        unlisten.push(await listen("surface-update", show));
        unlisten.push(await listen("surface-done", finish));
      }
      await invoke("start_surface");
      running = true;
      startBtn.textContent = "Stop";
      summary.textContent = "Reading every part of the drive…";
      grid.set("");
    } catch (e) {
      const msg = errText(e);
      if (msg.startsWith("Full Disk Access needed.")) { o.close(); showFullDiskAccess(msg); }
      else if (msg !== "Cancelled.") toast(msg, true);
    }
  }

  try {
    const r = await invoke("drive_report", { id: disk ? disk.id : null });
    const [title, ic] = VERDICT[r.verdict] || VERDICT.unknown;
    verdictBox.className = `verdict ${r.verdict}`;
    setKids(verdictBox, h("div.vicon", {}, icon(ic)), h("div", {}, h("b", { text: title }), h("ul", {}, r.reasons.map((why) => h("li", { text: why })))));
    setKids(facts, healthFacts(r).map(([k, v]) => [h("dt", { text: k }), h("dd", { text: v })]));
    setKids(notes, r.notes.map((t) => h("p.fine", { text: t })));
  } catch (e) {
    verdictBox.className = "verdict unknown";
    setKids(verdictBox, h("div.vicon", {}, icon("info")), h("div", {}, h("b", { text: "Health not reported" }), h("span", { text: errText(e) })));
  }
}

/// The drive has already erased its free space (TRIM): the deep scan
/// won't find deleted files there. For an image the blank space may
/// instead be the image leaving it out, so it says so.
function trimText() {
  const f = S.freeSpace;
  if (!f?.erased) return null;
  const pct = Math.round(f.blank_fraction * 100);
  return S.source?.is_disk === false
    ? [`This image's free space is blank`, `${pct}% of the free space sampled holds nothing. Either the drive erased it (TRIM) before the image was made, or the image was made without it. Deleted files are very likely gone; the deep scan is unlikely to find anything.`]
    : [`This ${medium()} has already erased its free space (TRIM)`, `Deleted files are very likely gone; the deep scan is unlikely to find anything. (${pct}% of the free space sampled is blank.)`];
}

/// The TRIM notice above the results.
function trimNotice() {
  const t = trimText();
  if (!t) return null;
  const stoppable = S.scan.running && S.scan.phase === "deep_scan";
  const key = `${t[0]}|${stoppable}`;
  if (trimNotice.el?.dataset.key !== key) {
    trimNotice.el = h("div.deeper.notice.trim", { dataset: { key } },
      h("div", {}, h("b", { text: t[0] }), h("span", { text: t[1] })),
      stoppable && h("button.btn", { on: { click: () => confirmStop(() => invoke("cancel")) } }, "Stop the scan"));
    trimNotice.el.addEventListener("animationend", (e) => { if (e.target === e.currentTarget) e.target.style.animation = "none"; });
  }
  return trimNotice.el;
}

/// The same notice on the scan screen, once the deep scan starts.
function updateTrimOnScan() {
  const box = document.getElementById("trim-note");
  if (!box) return;
  const t = S.scan.phase === "deep_scan" && S.scan.running ? trimText() : null;
  if (!t) { if (box.childElementCount) box.replaceChildren(); return; }
  if (box.dataset.title === t[0] && box.childElementCount) return;
  box.dataset.title = t[0];
  box.replaceChildren(h("div.deeper.notice.trim", {},
    h("div", {}, h("b", { text: t[0] }), h("span", { text: t[1] })),
    h("button.btn", { on: { click: () => confirmStop(() => invoke("cancel")) } }, "Stop the scan")));
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

document.addEventListener("keydown", (e) => {
  if (S.view === "results" && (e.metaKey || e.ctrlKey) && e.key === "a" && !document.querySelector(".overlay, .viewer") && document.activeElement?.tagName !== "INPUT") {
    e.preventDefault();
    toggleMany(currentList.map((i) => i.id));
  }
  if ((e.metaKey || e.ctrlKey) && e.key === ",") {
    e.preventDefault();
    if (!document.querySelector(".overlay")) openSettings();
  }
});
// The title bar: drag it to move the window, double-click it to zoom, as
// in any Mac app. The window has no system title bar of its own.
document.addEventListener("mousedown", (e) => {
  if (e.button !== 0 || !e.target.closest(".toolbar")) return;
  if (e.target.closest("button, input, select, textarea, a, label, [role=switch]")) return;
  const win = window.__TAURI__?.window?.getCurrentWindow?.();
  if (!win) return;
  if (e.detail === 2) win.toggleMaximize();
  else win.startDragging();
});
// No browser menu (Reload, Inspect) on right-click; text fields keep theirs
// for copy and paste.
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input, textarea, [contenteditable]")) e.preventDefault();
});
// Dropped files never navigate the window away.
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => e.preventDefault());
matchMedia("(prefers-reduced-motion: reduce)").addEventListener?.("change", applySettings);

(async function main() {
  applySettings();
  S.mode = settings.view;
  S.sort = settings.sort;
  S.platform = await invoke("platform");
  document.body.classList.add(S.platform);
  await listen("scan-update", onScanUpdate);
  await listen("scan-done", onScanDone);
  await listen("plugin-progress", onPluginProgress);
  renderHome();
  await refreshDisks();
  setInterval(() => { if (S.view === "home") refreshDisks(); }, 3000);
  if (window.__REPERIO_AUTOSTART__) window.__REPERIO_AUTOSTART__();
})();
