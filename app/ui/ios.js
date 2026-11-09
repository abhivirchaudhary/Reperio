"use strict";

// ---------------------------------------------------------------------------
// iPhone and iPad
//
// An iPhone's storage can't be read like a card, but a trusted computer can
// back it up. Reperio makes that backup (libimobiledevice), or opens one
// Finder made, and recovers from it: photos including Recently Deleted,
// messages, and any file. Nothing is written to the phone.
// ---------------------------------------------------------------------------

const IOS = { status: null, at: 0, busy: false, poll: null };
const P = { tab: "photos", filter: "all", q: "", sel: new Set(), photos: [], messages: [], files: [], fragments: null, recovered: null, wa: null, waChat: null, waRecovered: null, notes: null, noteOpen: null, notesRecovered: null, overview: null, name: "", path: "" };
let iosProgress = null;
listen("ios-progress", (p) => iosProgress?.(p));

const IOS_INSTALL = "brew install libimobiledevice";

/// What is connected and what backups exist. Cheap to call often: it asks at
/// most every 8 seconds unless forced.
async function refreshIos(force = false) {
  if (IOS.busy || (!force && Date.now() - IOS.at < 8000)) return;
  IOS.busy = true;
  try {
    const next = await invoke("ios_status");
    if (JSON.stringify(next) !== JSON.stringify(IOS.status)) {
      IOS.status = next;
      if (S.view === "home") updateDisks();
      if (S.view === "iphone") renderIphone();
    }
  } catch { /* no iPhone support here */ }
  IOS.at = Date.now();
  IOS.busy = false;
}

/// iPhones and iPads found, for the home screen.
function iosHomeSection() {
  const devices = IOS.status?.devices || [];
  if (!devices.length) return null;
  return [
    h("div.section-label", { text: "iPhones and iPads" }),
    h("div.disks", {}, devices.map((d, i) => h("div.disk-slot", {},
      h("button.disk", { style: { animationDelay: `${80 + i * 60}ms` }, on: { click: openIphone } },
        h("div.dicon", {}, icon("phone")),
        h("div.dtext", {},
          h("div.dname", { text: d.name }),
          h("div.dmeta", { text: d.paired ? [d.model, d.ios && `iOS ${d.ios}`, d.storage_total && size(d.storage_total)].filter(Boolean).join(" · ") : "Tap Trust on the iPhone to continue" })),
        icon("next", "go"))))),
  ];
}

function openIphone() {
  S.view = "iphone";
  clearInterval(IOS.poll);
  IOS.poll = setInterval(() => { if (S.view === "iphone") refreshIos(true); else clearInterval(IOS.poll); }, 4000);
  renderIphone();
  refreshIos(true);
}

const fmtBackupDate = (s) => (s ? fmtDateTime.format(new Date(s)) : "");
const percentBar = (n) => h("div.progress", {}, h("div", { style: `width:${n}%` }));

function renderIphone() {
  const st = IOS.status;
  const missing = st?.missing || [];
  const devices = st?.devices || [];
  const backups = st?.backups || [];
  show(h("div.view", {},
    h("div.toolbar", {},
      h("button.btn.icon.ghost", { title: "Back", on: { click: () => { clearInterval(IOS.poll); renderHome(); } } }, icon("back")),
      h("div.crumb", {}, h("div.t", { text: "iPhone and iPad" }), h("div.s", { text: "Back up, browse and recover" })),
      h("div.spacer"),
      settingsButton()),
    h("div.iphone", {},
      h("div.iphone-inner", {},
        h("header.welcome", {},
          h("h1", { text: "Your iPhone, within reach" }),
          h("p", { text: "Reperio makes a backup of the phone, then helps you find what's in it: photos (including Recently Deleted), messages and files. Nothing on the phone is ever changed." })),
        !st && h("div.listening", {}, h("div.spinner"), h("span", { text: "Looking for devices…" })),
        missing.length > 0 && h("div.a-card.ios-setup", {},
          h("b", { text: "One thing to install first" }),
          h("p", { text: `Reperio talks to iPhones with the free libimobiledevice tools. In Terminal, run:` }),
          h("div.codeline", {}, h("code", { text: IOS_INSTALL }),
            h("button.btn", { on: { click: () => { navigator.clipboard?.writeText(IOS_INSTALL); toast("Copied"); } } }, icon("copy"), "Copy")),
          h("p.fine", { text: `Missing: ${missing.join(", ")}. Then connect the phone and click Check again.` }),
          h("div.actions", {}, h("button.btn", { on: { click: () => refreshIos(true) } }, icon("refresh"), "Check again"))),
        st?.error && h("div.error", { text: st.error }),
        missing.length === 0 && st && devices.length === 0 && h("div.plug", {},
          h("div.plug-icons", {}, h("span", {}, icon("phone"))),
          h("b", { text: "Connect your iPhone or iPad" }),
          h("p", { text: "Plug it in with a cable and unlock it. If it asks, tap Trust." })),
        devices.map(deviceCard),
        h("div.section-label", { text: "Backups on this computer" }),
        backups.length
          ? h("div.backups", {}, backups.map(backupRow))
          : h("p.fine", { text: "None yet. Make one from a connected phone above, or Finder's backups appear here (Reperio needs Full Disk Access to see them)." }),
        h("p.home-tip", { text: "Backups are stored on this computer and are only read, never changed. Encrypted backups can't be opened: turn off \"Encrypt local backup\" in Finder to make one Reperio can read." })))));
}

function deviceCard(d) {
  const used = d.storage_total && d.storage_free != null ? 1 - d.storage_free / d.storage_total : null;
  return h("div.device", {},
    h("div.dicon.big", {}, icon("phone")),
    h("div.dinfo", {},
      h("div.dname", { text: d.name }),
      h("div.dmeta", { text: d.paired ? [d.model, d.ios && `iOS ${d.ios}`, d.serial && `Serial ${d.serial}`].filter(Boolean).join(" · ") : (d.problem || "Not trusted yet") }),
      d.paired && used != null && h("div.dstore", {},
        percentBar(Math.round(used * 100)),
        h("span", { text: `${size(d.storage_free)} free of ${size(d.storage_total)}${d.battery != null ? ` · battery ${d.battery}%` : ""}` })),
      d.paired && d.encrypts_backups && h("p.fine", { text: "This phone encrypts its backups, which Reperio can't read. In Finder, turn off \"Encrypt local backup\" for this phone, then try again." })),
    d.paired
      ? h("button.btn.primary", { on: { click: () => startBackup(d) } }, icon("save"), "Back up and recover")
      : h("button.btn.primary", { on: { click: () => trustDevice(d) } }, icon("key"), "Trust this computer"));
}

function backupRow(b) {
  return h("button.backup" + (b.encrypted ? ".off" : ""), { disabled: b.encrypted, title: b.encrypted ? "Encrypted: Reperio can't read this backup." : "", on: { click: () => openBackup(b.path, b.name) } },
    h("div.dicon", {}, icon(b.encrypted ? "key" : "disc")),
    h("div.dtext", {},
      h("div.dname", { text: b.name || "iPhone" }),
      h("div.dmeta", { text: [b.model, b.ios && `iOS ${b.ios}`, fmtBackupDate(b.date), b.encrypted && "encrypted"].filter(Boolean).join(" · ") })),
    !b.encrypted && icon("next", "go"));
}

async function trustDevice(d) {
  const done = busy("Trust this computer", "Unlock the phone and tap Trust. If it asks for its passcode, enter it.");
  try {
    await invoke("ios_pair", { udid: d.udid });
    done();
    toast("This computer is trusted.");
  } catch (e) {
    done();
    toast(errText(e), true);
  }
  refreshIos(true);
}

function startBackup(d) {
  const text = h("div.rec-sub", { text: "Starting…" });
  const bar = h("div", { style: "width:0%" });
  let stopped = false;
  const sheet = h("div.sheet", {},
    h("h2", { text: `Backing up ${d.name}` }),
    h("p.lead", { text: "Keep the phone connected and unlocked. A first backup can take several minutes." }),
    h("div.progress", {}, bar),
    text,
    h("div.actions", {}, h("button.btn", { on: { click: () => { stopped = true; invoke("ios_cancel"); } } }, "Stop")));
  const o = overlay(sheet);
  iosProgress = (p) => {
    if (p.percent > 0) bar.style.width = `${p.percent}%`;
    text.textContent = p.percent > 0 ? `${p.percent}% · ${p.text}` : p.text;
  };
  invoke("ios_backup", { udid: d.udid })
    .then((dir) => { iosProgress = null; o.close(); refreshIos(true); openBackup(dir, d.name); })
    .catch((e) => { iosProgress = null; o.close(); if (!stopped) toast(errText(e), true); });
}

async function openBackup(path, name) {
  const done = busy("Opening backup", "Reading its photos and messages…");
  try {
    P.overview = await invoke("ios_open_backup", { path });
    [P.photos, P.messages] = await Promise.all([invoke("ios_photos").catch(() => []), invoke("ios_messages").catch(() => [])]);
    Object.assign(P, { name: name || "iPhone", path, sel: new Set(), filter: "all", q: "", files: [], fragments: null, recovered: null, wa: null, waChat: null, waRecovered: null, notes: null, noteOpen: null, notesRecovered: null, tab: P.overview.photos || !P.overview.messages ? "photos" : "messages" });
    done();
    renderBackup();
  } catch (e) {
    done();
    toast(errText(e), true);
  }
}

// ---- the open backup -------------------------------------------------------

const thumbOf = (id) => window.__REPERIO_THUMB__ ? window.__REPERIO_THUMB__(id.length) : (navigator.userAgent.includes("Windows") ? `http://thumb.localhost/ios/${id}` : `thumb://localhost/ios/${id}`);

function renderBackup() {
  S.view = "iphone-backup";
  const o = P.overview;
  const tab = (id, label, count) => h("button" + (P.tab === id ? ".on" : ""), { on: { click: () => { P.tab = id; P.sel = new Set(); P.q = ""; P.filter = "all"; renderBackup(); } } }, label, count != null && h("span.n", { text: ` ${count.toLocaleString()}` }));
  show(h("div.view", {},
    h("div.toolbar", {},
      h("button.btn.icon.ghost", { title: "Back", on: { click: openIphone } }, icon("back")),
      h("div.crumb", {}, h("div.t", { text: P.name }), h("div.s", { text: "Backup" })),
      h("div.spacer"),
      h("div.seg.tabs", {}, tab("photos", "Photos", o.photos), tab("messages", "Messages", o.messages), o.whatsapp && tab("whatsapp", "WhatsApp"), o.notes && tab("notes", "Notes"), tab("files", "Files")),
      h("button.btn.ghost", { title: "Run your analyzer plug-ins on this backup", on: { click: iosPlugins } }, icon("open"), "Plug-ins"),
      settingsButton()),
    h("div.content.ios-content", {},
      h("div.grid-scroll", { id: "ios-scroll" }),
      h("div.bottombar", { id: "ios-bar" }))));
  renderBackupTab();
}

function renderBackupTab() {
  const scroll = document.getElementById("ios-scroll");
  if (!scroll) return;
  ({ photos: photosTab, messages: messagesTab, files: filesTab, whatsapp: whatsappTab, notes: notesTab })[P.tab](scroll);
  updateIosBar();
}

const chips = (items, current, set) => h("div.fchips", {}, items.map(([id, label]) =>
  h("button.chip" + (current === id ? ".on" : ""), { on: { click: () => set(id) } }, label)));

function photosTab(scroll) {
  const o = P.overview;
  if (o.photos_error && !P.photos.length) {
    scroll.replaceChildren(h("div.empty", {}, h("div", {}, icon("photo"), h("h3", { text: "No photo library in this backup" }), h("p", { text: o.photos_error }))));
    return;
  }
  const list = P.photos.filter((p) =>
    (P.filter === "all" || (P.filter === "deleted" && p.deleted) || (P.filter === "videos" && p.video)) &&
    (!P.q || p.name.toLowerCase().includes(P.q.toLowerCase())));
  const lost = o.deleted_photos - o.recoverable_photos;
  const nodes = [];
  if (o.deleted_photos > 0) {
    nodes.push(h("div.deeper.notice", {},
      h("div", {},
        h("b", { text: `${plural(o.deleted_photos, "photo")} in Recently Deleted` }),
        h("span", { text: `${o.recoverable_photos} can be recovered from this backup.${lost > 0 ? ` ${lost} ${lost === 1 ? "was" : "were"} listed but not included in the backup, so can't come back from it.` : ""}` })),
      h("div.notice-actions", {}, o.recoverable_photos > 0 && h("button.btn", { on: { click: () => { P.filter = "deleted"; P.sel = new Set(P.photos.filter((p) => p.deleted && p.file_id).map((p) => p.file_id)); renderBackupTab(); } } }, "Select them"))));
  }
  nodes.push(h("div.ios-filter", {},
    chips([["all", `All ${o.photos}`], ["deleted", `Recently Deleted ${o.deleted_photos}`], ["videos", "Videos"]], P.filter, (f) => { P.filter = f; renderBackupTab(); }),
    h("label.searchbox", {}, icon("search"), h("input", { placeholder: "Search", value: P.q, on: { input: (e) => { P.q = e.target.value; clearTimeout(photosTab.t); photosTab.t = setTimeout(renderBackupTab, 200); } } }))));
  if (!list.length) {
    nodes.push(h("div.empty", { style: "height:50%" }, h("div", {}, icon("photo"), h("h3", { text: "Nothing here" }), h("p", { text: "No photos match." }))));
    scroll.replaceChildren(...nodes);
    return;
  }
  const grid = h("div.grid");
  for (const p of list.slice(0, 600)) grid.append(photoCard(p));
  nodes.push(grid);
  if (list.length > 600) nodes.push(h("p.fine", { style: "text-align:center;margin:18px", text: `Showing the first 600 of ${list.length.toLocaleString()}. Use the search or a filter to narrow it.` }));
  scroll.replaceChildren(...nodes);
}

function photoCard(p) {
  const id = p.file_id;
  const on = id && P.sel.has(id);
  const ratio = (img) => Math.max(0.6, Math.min(2.2, img.naturalWidth / img.naturalHeight || 1));
  const el = h("div.card" + (on ? ".selected" : ""), { tabindex: 0, role: "checkbox", "aria-checked": String(!!on), "aria-label": p.name, title: id ? p.name : `${p.name}: in Recently Deleted, but the backup did not include its file` },
    h("div.check", {}, icon("check")),
    h("div.thumb", {}, id
      ? h("img", { src: thumbOf(id), alt: "", draggable: false, on: {
        load: (e) => { e.target.classList.add("loaded"); el.style.setProperty("--ar", ratio(e.target).toFixed(3)); },
        error: (e) => e.target.parentNode?.replaceChildren(icon(p.video ? "video" : "photo"), h("span.ext", { text: p.name.split(".").pop() })),
      } })
      : [icon(p.video ? "video" : "photo"), h("span.ext", { text: "not in backup" })]),
    p.deleted && h("div.badge.warn", { text: "Recently deleted" }),
    h("div.meta", {}, h("div.name", { text: p.name }), h("div.info", { text: [p.size ? size(p.size) : null, p.date && fmtDate.format(new Date(p.date))].filter(Boolean).join(" · ") })));
  el.style.setProperty("--ar", "1.33");
  if (!id) el.classList.add("missing");
  else el.addEventListener("click", () => { P.sel.has(id) ? P.sel.delete(id) : P.sel.add(id); el.classList.toggle("selected", P.sel.has(id)); el.setAttribute("aria-checked", String(P.sel.has(id))); updateIosBar(); });
  return el;
}

function messagesTab(scroll) {
  const o = P.overview;
  if (o.messages_error && !P.messages.length) {
    scroll.replaceChildren(h("div.empty", {}, h("div", {}, icon("doc"), h("h3", { text: "No messages in this backup" }), h("p", { text: o.messages_error }))));
    return;
  }
  const q = P.q.toLowerCase();
  const source = P.filter === "recovered" ? (P.recovered || []) : P.messages;
  const list = source.filter((m) => (P.filter !== "deleted" || m.deleted) && (!q || m.text.toLowerCase().includes(q) || m.with.toLowerCase().includes(q)));
  const nodes = [h("div.ios-filter", {},
    chips([["all", `All ${o.messages}`], ["deleted", `Recently Deleted ${o.deleted_messages}`], ...(P.recovered ? [["recovered", `Recovered ${P.recovered.length}`]] : [])], P.filter, (f) => { P.filter = f; renderBackupTab(); }),
    h("label.searchbox", {}, icon("search"), h("input", { placeholder: "Search messages", value: P.q, on: { input: (e) => { P.q = e.target.value; clearTimeout(messagesTab.t); messagesTab.t = setTimeout(renderBackupTab, 200); } } })))];
  nodes.push(h("div.msgs", {}, list.slice(0, 500).map((m) =>
    h("div.msg" + (m.from_me ? ".me" : ""), {},
      h("div.mhead", {}, h("b", { text: m.from_me ? `To ${m.with}` : m.with || "Unknown" }), h("span", { text: m.date ? fmtDateTime.format(new Date(m.date)) : "" }), m.recovered ? h("span.pill.bad", { text: "Recovered, may be incomplete" }) : m.deleted && h("span.pill.warn", { text: "Recently deleted" })),
      h("div.mtext", { text: m.text })))));
  if (!list.length) nodes.push(h("p.fine", { style: "text-align:center;margin:24px", text: "No messages match." }));
  if (list.length > 500) nodes.push(h("p.fine", { style: "text-align:center;margin:12px", text: `Showing the first 500 of ${list.length.toLocaleString()}. Search to narrow it.` }));
  // Text the phone deleted and has not yet overwritten.
  nodes.push(h("div.deeper.notice", {},
    h("div", {}, h("b", { text: "Recover messages deleted for good" }), h("span", { text: "When a message is deleted, the phone often keeps its bytes until something writes over them. Reperio rebuilds whole messages from those leftovers, with text, sender and date where they survive. What comes back is partial: some will be missing, and a few may be incomplete." })),
    h("div.notice-actions", {},
      h("button.btn.primary", { on: { click: recoverDeleted } }, P.recovered ? "Search again" : "Recover deleted messages"),
      h("button.btn", { id: "frag-btn", on: { click: findFragments } }, "Raw text only"))));
  if (P.fragments) {
    nodes.push(h("div.section-label", { text: `${plural(P.fragments.length, "fragment")} found` }));
    nodes.push(h("div.msgs", {}, P.fragments.slice(0, 300).map((f) => h("div.msg.frag", {}, h("div.mtext", { text: f.text })))));
  }
  scroll.replaceChildren(...nodes);
}

async function recoverDeleted() {
  const done = busy("Recovering deleted messages", "Looking through the message database for what the phone left behind…");
  try {
    P.recovered = await invoke("ios_deleted_messages");
    P.filter = "recovered";
    if (!P.recovered.length) toast("No deleted messages could be recovered from this backup.");
  } catch (e) { toast(errText(e), true); }
  done();
  renderBackupTab();
}

async function findFragments() {
  const done = busy("Searching the message database", "Looking through space the phone marked as free…");
  try { P.fragments = await invoke("ios_fragments"); } catch (e) { toast(errText(e), true); }
  done();
  renderBackupTab();
}

async function filesTab(scroll) {
  const nodes = [h("div.ios-filter", {},
    h("p.fine", { style: "margin:0" }, "Every file the backup holds, by its path on the phone. Type at least 2 letters."),
    h("label.searchbox", {}, icon("search"), h("input", { id: "files-q", placeholder: "Search files", value: P.q, on: { input: (e) => { P.q = e.target.value; clearTimeout(filesTab.t); filesTab.t = setTimeout(loadFiles, 250); } } })))];
  nodes.push(h("div.msgs.files", { id: "files-list" }, fileRows()));
  scroll.replaceChildren(...nodes);
}

async function loadFiles() {
  if (P.q.length < 2) { P.files = []; } else {
    try { P.files = await invoke("ios_files", { search: P.q }); } catch (e) { toast(errText(e), true); }
  }
  const list = document.getElementById("files-list");
  if (list) list.replaceChildren(...fileRows());
  updateIosBar();
}

function fileRows() {
  if (!P.files.length) return [h("p.fine", { style: "text-align:center;margin:24px", text: P.q.length < 2 ? "Search to see files." : "No files match." })];
  return P.files.map((f) => {
    const on = P.sel.has(f.id);
    const el = h("label.frow" + (on ? ".selected" : ""), {},
      h("input", { type: "checkbox", checked: on, on: { change: (e) => { e.target.checked ? P.sel.add(f.id) : P.sel.delete(f.id); el.classList.toggle("selected", e.target.checked); updateIosBar(); } } }),
      h("div", {}, h("b", { text: f.path.split("/").pop() }), h("span", { text: `${f.domain} / ${f.path}` })),
      h("span.sz", { text: size(f.size) }));
    return el;
  });
}

function updateIosBar() {
  const bar = document.getElementById("ios-bar");
  if (!bar) return;
  const n = P.sel.size;
  bar.replaceChildren(
    h("span.summary", {}, n ? [h("b", { text: plural(n, P.tab === "files" ? "file" : "item") }), " selected"] : "Nothing selected"),
    h("div.flex"),
    P.tab === "photos" && h("button.btn.ghost", { on: { click: () => { const all = P.photos.filter((p) => p.file_id && (P.filter !== "deleted" || p.deleted) && (P.filter !== "videos" || p.video)); const allOn = all.every((p) => P.sel.has(p.file_id)); all.forEach((p) => (allOn ? P.sel.delete(p.file_id) : P.sel.add(p.file_id))); renderBackupTab(); } } }, "Select all"),
    P.tab !== "messages" && h("button.btn.primary.large", { disabled: n === 0, on: { click: exportSelected } }, n ? `Export ${plural(n, "item")}` : "Export"));
  bar.hidden = ["messages", "whatsapp", "notes"].includes(P.tab);
}

async function exportSelected() {
  const dest = await invoke("ios_choose_folder").catch(() => null);
  if (!dest) return;
  const ids = [...P.sel];
  const photos = P.tab === "photos" ? ids : [];
  const files = P.tab === "files" ? ids : [];
  const done = busy("Copying", `Saving ${plural(ids.length, "item")}…`);
  try {
    const n = await invoke("ios_export", { photos, files, dest });
    done();
    toast(`${plural(n, "file")} saved to ${dest.split("/").pop()}.`);
    invoke("reveal", { path: dest }).catch(() => {});
  } catch (e) {
    done();
    toast(errText(e), true);
  }
}

// Look for devices soon after start, so the home screen shows a connected iPhone.
setTimeout(() => refreshIos(), 400);

/// Runs your analyzer plug-ins (those that work on `ios-backup`) on the open backup.
async function iosPlugins() {
  let info;
  try { info = await invoke("plugins_info"); } catch { info = { plugins: [] }; }
  const list = info.plugins.filter((p) => (p.kind === "analyzer" || p.kind === "tool") && p.targets.some((t) => t === "ios-backup" || t === "any"));
  const picked = new Set(list.map((p) => p.name));
  const sheet = h("div.sheet.wide");
  const o = overlay(sheet);
  const render = () => setKids(sheet,
    h("h2", { text: "Plug-ins for this backup" }),
    list.length
      ? [h("p.lead", { text: "Your analyzer plug-ins that work on iPhone backups. They can read any file in it, ask for deleted rows of its databases, and have Reperio repair damaged photos and videos." }),
        list.map((p) => h("label.pitem", {},
          h("input", { type: "checkbox", checked: picked.has(p.name), on: { change: (e) => { e.target.checked ? picked.add(p.name) : picked.delete(p.name); render(); } } }),
          h("div", {}, h("b", {}, p.name, p.version && h("small.pver", { text: ` ${p.version}` })), h("span", { text: p.description || "" })))),
        h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Cancel"),
          h("button.btn.primary", { disabled: !picked.size, on: { click: run } }, `Run ${plural(picked.size, "plug-in")}`))]
      : [h("p.lead", { text: "No plug-in works on iPhone backups yet. A plug-in says it does by listing \"ios-backup\" in its targets (see docs/forensic-plugins.md)." }),
        h("div.actions", {}, h("button.btn", { on: { click: () => invoke("open_plugins_folder").catch(() => {}) } }, icon("folder"), "Open plug-ins folder"), h("button.btn.primary", { on: { click: () => o.close() } }, "Done"))]);
  const run = async () => {
    o.close();
    const done = busy("Running plug-ins", "On the backup…");
    try { S.reports = await invoke("ios_analyze", { names: [...picked] }); done(); showReports(); } catch (e) { done(); toast(errText(e), true); }
  };
  render();
}

// ---- WhatsApp ----------------------------------------------------------------

async function whatsappTab(scroll) {
  if (!P.wa) {
    scroll.replaceChildren(h("div.listening", { style: "margin:40px" }, h("div.spinner"), h("span", { text: "Reading WhatsApp…" })));
    try { P.wa = await invoke("ios_whatsapp"); } catch (e) { scroll.replaceChildren(h("div.empty", {}, h("div", {}, icon("doc"), h("h3", { text: "No WhatsApp data" }), h("p", { text: errText(e) })))); return; }
    if (P.tab !== "whatsapp") return;
    P.waChat = P.wa.chats[0]?.id ?? null;
  }
  const recovered = P.waFilter === "recovered";
  const chats = P.wa.chats;
  const list = recovered ? (P.waRecovered || []) : P.wa.messages.filter((m) => m.chat === P.waChat);
  const chat = chats.find((c) => c.id === P.waChat);
  const q = P.q.toLowerCase();
  const shown = list.filter((m) => !q || m.text.toLowerCase().includes(q) || (m.media_name || "").toLowerCase().includes(q));
  const media = (m) => !m.media ? (m.kind !== "text" && h("span.chip-tag", { text: m.media_name ? `${m.kind}: ${m.media_name}` : m.kind }))
    : (m.kind === "photo" || m.kind === "sticker")
      ? h("img.wa-img", { src: thumbOf(m.media), alt: m.media_name || "photo", on: { error: (e) => e.target.replaceWith(h("span.chip-tag", { text: m.media_name || m.kind })) } })
      : h("span.chip-tag", { text: m.media_name ? `${m.kind}: ${m.media_name}` : m.kind });
  const bubble = (m) => h("div.msg.wa" + (m.from_me ? ".me" : ""), {},
    h("div.mhead", {}, h("b", { text: recovered ? `${m.chat_name || "Unknown chat"} · ${m.who}` : m.who }), h("span", { text: m.date ? fmtDateTime.format(new Date(m.date)) : "" }),
      m.recovered && h("span.pill.bad", { text: "Recovered, may be incomplete" })),
    media(m), m.text && h("div.mtext", { text: m.text }));
  const mediaIds = !recovered ? list.filter((m) => m.media).map((m) => m.media) : [];
  scroll.replaceChildren(h("div.split", {},
    h("div.split-list", {},
      h("button.side-item" + (!recovered ? ".on" : ""), { on: { click: () => { P.waFilter = "all"; renderBackupTab(); } } }, h("span.label", { text: "Chats" }), h("span.count", { text: String(chats.length) })),
      !recovered && chats.map((c) => h("button.chat" + (c.id === P.waChat ? ".on" : ""), { on: { click: () => { P.waChat = c.id; renderBackupTab(); } } },
        h("div.dicon", {}, icon(c.group ? "all" : "phone")), h("div", {}, h("b", { text: c.name }), h("span", { text: `${plural(c.messages, "message")}${c.last ? ` · ${fmtDate.format(new Date(c.last))}` : ""}` })))),
      h("button.side-item" + (recovered ? ".on" : ""), { on: { click: () => P.waRecovered ? (P.waFilter = "recovered", renderBackupTab()) : recoverWhatsapp() } },
        h("span.label", { text: "Recovered messages" }), P.waRecovered && h("span.count", { text: String(P.waRecovered.length) }))),
    h("div.split-main", {},
      h("div.ios-filter", {},
        h("div", {}, h("h3.serif", { text: recovered ? "Messages deleted for good" : chat?.name || "WhatsApp" }),
          recovered && h("p.fine", { style: "margin:2px 0 0", text: "Rebuilt from what WhatsApp's database still held: some are missing, a few may be incomplete." })),
        h("label.searchbox", {}, icon("search"), h("input", { placeholder: "Search", value: P.q, on: { input: (e) => { P.q = e.target.value; clearTimeout(whatsappTab.t); whatsappTab.t = setTimeout(renderBackupTab, 200); } } })),
        mediaIds.length > 0 && h("button.btn", { on: { click: () => exportMedia(mediaIds) } }, icon("save"), `Save ${plural(mediaIds.length, "photo/file")}`)),
      h("div.msgs.wa-list", {}, shown.slice(0, 600).map(bubble)),
      !shown.length && h("p.fine", { style: "text-align:center;margin:24px", text: recovered && !P.waRecovered?.length ? "Nothing could be recovered." : "No messages." }),
      shown.length > 600 && h("p.fine", { style: "text-align:center;margin:12px", text: `Showing the first 600 of ${shown.length.toLocaleString()}. Search to narrow it.` }))));
}

async function recoverWhatsapp() {
  const done = busy("Recovering WhatsApp messages", "Looking through WhatsApp's database for what it left behind…");
  try { P.waRecovered = await invoke("ios_whatsapp_deleted"); P.waFilter = "recovered"; if (!P.waRecovered.length) toast("No deleted WhatsApp messages could be recovered."); } catch (e) { toast(errText(e), true); }
  done();
  renderBackupTab();
}

async function exportMedia(ids) {
  const dest = await invoke("ios_choose_folder").catch(() => null);
  if (!dest) return;
  const done = busy("Copying", `Saving ${plural(ids.length, "file")}…`);
  try { const n = await invoke("ios_export", { photos: [], files: ids, dest }); done(); toast(`${plural(n, "file")} saved.`); invoke("reveal", { path: dest }).catch(() => {}); } catch (e) { done(); toast(errText(e), true); }
}

// ---- Notes -------------------------------------------------------------------

async function notesTab(scroll) {
  if (!P.notes) {
    scroll.replaceChildren(h("div.listening", { style: "margin:40px" }, h("div.spinner"), h("span", { text: "Reading Notes…" })));
    try { P.notes = await invoke("ios_notes"); } catch (e) { scroll.replaceChildren(h("div.empty", {}, h("div", {}, icon("doc"), h("h3", { text: "No notes" }), h("p", { text: errText(e) })))); return; }
    if (P.tab !== "notes") return;
  }
  const f = P.notesFilter || "all";
  const source = f === "recovered" ? (P.notesRecovered || []) : P.notes.filter((n) => f === "all" || n.deleted);
  const q = P.q.toLowerCase();
  const list = source.filter((n) => !q || n.title.toLowerCase().includes(q) || n.text.toLowerCase().includes(q));
  const open = list.find((n) => n.id === P.noteOpen && (n.recovered === (f === "recovered"))) || list[0];
  const chipsRow = chips([["all", `All ${P.notes.length}`], ["deleted", `Recently Deleted ${P.notes.filter((n) => n.deleted).length}`], ...(P.notesRecovered ? [["recovered", `Recovered ${P.notesRecovered.length}`]] : [])], f, (v) => { P.notesFilter = v; renderBackupTab(); });
  scroll.replaceChildren(h("div.split", {},
    h("div.split-list", {},
      list.slice(0, 500).map((n) => h("button.chat" + (n === open ? ".on" : ""), { on: { click: () => { P.noteOpen = n.id; renderBackupTab(); } } },
        h("div.dicon", {}, icon("doc")),
        h("div", {}, h("b", { text: n.title || "Untitled" }),
          h("span", { text: [n.recovered ? "Recovered" : n.deleted ? "Recently Deleted" : n.folder, n.modified && fmtDate.format(new Date(n.modified))].filter(Boolean).join(" · ") })))),
      !list.length && h("p.fine", { style: "padding:14px", text: "No notes." })),
    h("div.split-main", {},
      h("div.ios-filter", {}, chipsRow,
        h("label.searchbox", {}, icon("search"), h("input", { placeholder: "Search notes", value: P.q, on: { input: (e) => { P.q = e.target.value; clearTimeout(notesTab.t); notesTab.t = setTimeout(renderBackupTab, 200); } } }))),
      open ? h("article.note", {},
        h("h2", { text: open.title || "Untitled" }),
        h("div.nmeta", {}, [open.recovered ? h("span.pill.bad", { text: "Recovered, may be incomplete" }) : open.deleted && h("span.pill.warn", { text: "Recently deleted" }), open.modified && `Modified ${fmtDateTime.format(new Date(open.modified))}`]),
        h("div.ntext", { text: open.text }),
        h("div.actions.inline", {},
          h("button.btn", { on: { click: () => { navigator.clipboard?.writeText(open.text); toast("Copied"); } } }, icon("copy"), "Copy"),
          h("button.btn", { on: { click: () => saveNotes(list) } }, icon("save"), `Save these ${list.length} as text files`)))
        : h("p.fine", { style: "text-align:center;margin:40px", text: "Nothing to show." }),
      f !== "recovered" && h("div.deeper.notice", {},
        h("div", {}, h("b", { text: "Recover notes deleted for good" }), h("span", { text: "When a note is deleted its text often stays inside the Notes database until something writes over it. Reperio rebuilds what it can: the text, and the title where it survives." })),
        h("div.notice-actions", {}, h("button.btn.primary", { on: { click: recoverNotes } }, P.notesRecovered ? "Search again" : "Recover deleted notes"))))));
}

async function recoverNotes() {
  const done = busy("Recovering notes", "Looking through the Notes database for what it left behind…");
  try { P.notesRecovered = await invoke("ios_notes_deleted"); P.notesFilter = "recovered"; P.noteOpen = null; if (!P.notesRecovered.length) toast("No deleted notes could be recovered."); } catch (e) { toast(errText(e), true); }
  done();
  renderBackupTab();
}

async function saveNotes(list) {
  const dest = await invoke("ios_choose_folder").catch(() => null);
  if (!dest) return;
  try { const n = await invoke("ios_save_notes", { notes: list.map((n) => [n.title || "Untitled", n.text]), dest }); toast(`${plural(n, "note")} saved.`); invoke("reveal", { path: dest }).catch(() => {}); } catch (e) { toast(errText(e), true); }
}
