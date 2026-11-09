"use strict";

// ---------------------------------------------------------------------------
// Plug-in run control: the limits you chose, and what plug-ins say as they run.
// ---------------------------------------------------------------------------

/// Tells the engine the limits from Settings (none, unless you chose them).
function applyPluginLimits() {
  invoke("plugin_set_limits", { idleSecs: (settings.pluginIdleMin || 0) * 60, maxSecs: (settings.pluginMaxHours || 0) * 3600 }).catch(() => {});
}
applyPluginLimits();

const PLOG = { lines: [], open: false, pill: null, drawer: null };
listen("plugin-log", (m) => {
  PLOG.lines.push(m);
  if (PLOG.lines.length > 600) PLOG.lines.splice(0, PLOG.lines.length - 600);
  if (!PLOG.pill) showPluginPill();
  if (PLOG.open) appendPluginLine(m, true);
});

function showPluginPill() {
  PLOG.pill = h("button.plog-pill", { title: "What plug-ins are saying", on: { click: () => togglePluginLog() } }, h("span.dot"), "Plug-in activity");
  document.body.append(PLOG.pill);
}

function appendPluginLine(m, stick) {
  const box = PLOG.drawer?.querySelector(".plog-lines");
  if (!box) return;
  box.querySelector(".plog-empty")?.remove();
  const near = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  box.append(h("div", {}, h("span.who", { text: m.plugin }), m.line));
  if (stick && near) box.scrollTop = box.scrollHeight;
}

function togglePluginLog(force) {
  const want = force ?? !PLOG.open;
  PLOG.open = want;
  PLOG.drawer?.remove();
  PLOG.drawer = null;
  if (!want) return;
  if (!PLOG.pill) showPluginPill();
  PLOG.drawer = h("div.plog-drawer", { role: "log", "aria-label": "Plug-in activity" },
    h("div.plog-head", {}, h("b", { text: "Plug-in activity" }),
      h("button.btn.ghost", { on: { click: () => { PLOG.lines = []; togglePluginLog(true); } } }, "Clear"),
      h("button.btn.icon.ghost", { title: "Close", on: { click: () => togglePluginLog(false) } }, icon("x"))),
    h("div.plog-lines", {}, h("div.plog-empty", { text: "Nothing yet. What a plug-in says while it runs appears here." })));
  document.body.append(PLOG.drawer);
  for (const m of PLOG.lines) appendPluginLine(m, false);
  const box = PLOG.drawer.querySelector(".plog-lines");
  box.scrollTop = box.scrollHeight;
}

/// Checks a plug-in against the protocol and shows what passed.
async function testPlugin(p) {
  const done = busy(`Testing ${p.name}`, "Starting it and running it on made-up data…");
  let checks;
  try { checks = await invoke("plugin_test", { file: p.file }); } catch (e) { done(); toast(errText(e), true); return; }
  done();
  const bad = checks.filter((c) => !c.ok).length;
  const sheet = h("div.sheet.wide", {},
    h("h2", { text: bad ? `${p.name}: ${bad} ${bad === 1 ? "check" : "checks"} failed` : `${p.name} passes` }),
    h("p.lead", { text: "This shows Reperio can talk to the plug-in. It does not show that it finds the right things." }),
    h("div.checks", {}, checks.map((c) => h("div.check-row" + (c.ok ? "" : ".bad"), {},
      h("span.mark", { text: c.ok ? "✔" : "✘" }), h("div", {}, h("b", { text: c.name }), c.detail && h("span", { text: c.detail }))))),
    h("div.actions", {}, h("button.btn.primary", { on: { click: () => o.close() } }, "Done")));
  const o = overlay(sheet);
}

/// Install a plug-in from a file, a folder (with a plugin.json), a .zip or a URL.
function installPluginSheet(after) {
  const src = h("input.textin", { placeholder: "A file, folder or .zip, or a web address", style: "width:100%" });
  const sha = h("input.textin", { placeholder: "SHA-256 it should have (optional)", style: "width:100%" });
  const pick = async (folder) => { const p = await invoke("plugin_pick", { folder }).catch(() => null); if (p) src.value = p; };
  const go = async () => {
    const source = src.value.trim();
    if (!source) { toast("Choose something to install.", true); return; }
    const done = busy("Installing", "Checking that it starts and says what it is…");
    try {
      const r = await invoke("plugin_install", { source, sha256: sha.value.trim() || null });
      done();
      o.close();
      toast(`${r.updated ? "Updated" : "Installed"} ${r.name}${r.version ? ` ${r.version}` : ""}${r.permissions?.length ? `. It says: ${r.permissions.join(", ")}` : ""}.`);
      after?.();
    } catch (e) { done(); toast(errText(e), true); }
  };
  const sheet = h("div.sheet.wide", {},
    h("h2", { text: "Install a plug-in" }),
    h("p.lead", { text: "Reperio checks that it starts and says what it is, then remembers its fingerprint. It never judges what the plug-in does." }),
    h("div.field", {}, h("label", { text: "What to install" }), src,
      h("div.actions.inline", { style: "margin-top:8px" },
        h("button.btn", { on: { click: () => pick(false) } }, icon("open"), "Choose a file…"),
        h("button.btn", { on: { click: () => pick(true) } }, icon("folder"), "Choose a folder…"))),
    h("div.field", {}, h("label", { text: "Check its SHA-256" }), sha,
      h("div.hint", { text: "If you were given a fingerprint, paste it: a file that differs is not installed." })),
    h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Cancel"), h("button.btn.primary", { on: { click: go } }, "Install")));
  const o = overlay(sheet);
}

/// Removes a plug-in, after asking.
function removePlugin(p, after) {
  const sheet = h("div.sheet", {},
    h("h2", { text: `Remove ${p.name}?` }),
    h("p.lead", { text: `Its file (${p.file.split(/[\\/]/).pop()}) is deleted from the plug-ins folder. Reports it already made stay where they are.` }),
    h("div.actions", {}, h("button.btn", { on: { click: () => o.close() } }, "Cancel"),
      h("button.btn.primary", { on: { click: async () => { try { await invoke("plugin_remove", { name: p.name }); o.close(); toast(`${p.name} removed.`); after?.(); } catch (e) { toast(errText(e), true); } } } }, "Remove")));
  const o = overlay(sheet);
}
