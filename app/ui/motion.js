"use strict";

// Motion: the dot map of what is being recovered, and small number
// animations. Everything eases towards its target in
// requestAnimationFrame, so progress glides instead of jumping. With
// reduced motion every state still updates, without movement.

const MOTION = {
  // "system", "full" or "reduced", from Settings.
  pref: "system",
  get reduced() {
    if (this.pref === "reduced") return true;
    if (this.pref === "full") return false;
    return matchMedia("(prefers-reduced-motion: reduce)").matches;
  },
};

// Calm, slightly muted colours per kind of file.
const KIND_COLOR = {
  photo: "#6f9fd8",
  raw: "#d7a35b",
  video: "#a58ad6",
  audio: "#6db596",
  document: "#8f9bb0",
  other: "#a9aaa6",
};

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function withAlpha(color, a) {
  if (color.startsWith("#")) {
    const n = parseInt(color.slice(1), 16);
    return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
  }
  return color;
}

const easeOut = (t) => 1 - Math.pow(1 - t, 3);

/// The card as a field of dots, each standing for an equal slice of it.
///
/// Scan mode: dots tint softly as the scan passes them, a gentle light
/// marks where it is reading, and dots holding found files bloom in their
/// kind's colour. Recover mode: the selected files' dots start faint and
/// fill in as their bytes are copied out.
class DotMap {
  constructor(canvas, total, { mode = "scan", pitch = 11 } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.total = Math.max(1, total);
    this.mode = mode;
    this.pitch = pitch;
    this.scanned = 0;
    this.shown = 0; // eased scan position, in dots
    this.head = null;
    this.complete = false;
    this.marks = []; // [{cell, kind, at}]
    this.resize();
    this.onResize = () => this.resize();
    window.addEventListener("resize", this.onResize);
    this.frame = this.frame.bind(this);
    requestAnimationFrame(this.frame);
  }

  resize() {
    const rect = this.canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    this.w = Math.max(1, rect.width);
    this.h = Math.max(1, rect.height);
    this.canvas.width = Math.round(this.w * dpr);
    this.canvas.height = Math.round(this.h * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const old = this.n;
    this.cols = Math.max(8, Math.floor(this.w / this.pitch));
    this.rows = Math.max(2, Math.floor(this.h / this.pitch));
    this.n = this.cols * this.rows;
    this.dx = this.w / this.cols;
    this.dy = this.h / this.rows;
    if (old !== this.n) {
      this.kind = new Array(this.n).fill(null);
      this.bloom = new Float64Array(this.n);
      this.copied = new Uint8Array(this.n);
      this.active = new Uint8Array(this.n);
      for (const m of this.marks) this.apply(m, false);
    }
  }

  cellOf(offset) {
    return Math.min(this.n - 1, Math.max(0, Math.floor((offset / this.total) * this.n)));
  }

  cellsOf(extents) {
    const out = [];
    for (const e of extents || []) {
      const a = this.cellOf(e.offset);
      const b = this.cellOf(e.offset + Math.max(0, e.len - 1));
      for (let c = a; c <= b; c++) out.push(c);
    }
    return out;
  }

  center(c) {
    const rect = this.canvas.getBoundingClientRect();
    const col = c % this.cols, row = Math.floor(c / this.cols);
    return [rect.left + (col + 0.5) * this.dx, rect.top + (row + 0.5) * this.dy];
  }

  apply(m, bloom) {
    for (const c of this.cellsOf(m.extents)) {
      this.kind[c] = m.kind;
      if (bloom) this.bloom[c] = performance.now();
    }
  }

  /// A found file (scan) or a file to copy (recover). Returns its first
  /// dot, for particles to start from.
  mark(item, bloom = true) {
    const m = { extents: item.extents, kind: item.kind in KIND_COLOR ? item.kind : "other" };
    this.marks.push(m);
    this.apply(m, bloom && !MOTION.reduced);
    return this.cellsOf(item.extents)[0] ?? 0;
  }

  setScan(at) {
    if (at == null) return;
    this.head = at;
    this.scanned = Math.max(this.scanned, at);
  }

  /// Shows the scanned part at once instead of filling it in.
  snap() {
    this.shown = this.complete ? this.n : (this.scanned / this.total) * this.n;
  }

  finish() {
    this.complete = true;
    this.head = null;
    this.scanned = this.total;
  }

  markCopied(item) {
    const cells = this.cellsOf(item.extents);
    const now = performance.now();
    for (const c of cells) {
      this.copied[c] = 1;
      this.active[c] = 0;
      this.bloom[c] = MOTION.reduced ? 0 : now;
    }
    return cells;
  }

  setActive(item) {
    this.active.fill(0);
    for (const c of this.cellsOf(item.extents)) if (!this.copied[c]) this.active[c] = 1;
  }

  frame(now) {
    if (!this.canvas.isConnected) {
      window.removeEventListener("resize", this.onResize);
      return;
    }
    this.draw(now);
    requestAnimationFrame(this.frame);
  }

  draw(now) {
    const { ctx, cols, n, dx, dy } = this;
    ctx.clearRect(0, 0, this.w, this.h);
    const empty = cssVar("--map-empty");
    const done = cssVar("--map-done");
    const accent = cssVar("--accent");
    const target = this.complete ? n : (this.scanned / this.total) * n;
    this.shown += MOTION.reduced ? target - this.shown : (target - this.shown) * 0.06;
    const headCell = this.head != null ? this.cellOf(this.head) : -1;
    const r = Math.min(dx, dy) * 0.24;

    for (let c = 0; c < n; c++) {
      const x = (c % cols + 0.5) * dx;
      const y = (Math.floor(c / cols) + 0.5) * dy;
      const k = this.kind[c];
      let fill = empty, rad = r;

      if (this.mode === "scan") {
        if (k) fill = KIND_COLOR[k];
        else if (c < this.shown) fill = done;
        if (!k && headCell >= 0 && !MOTION.reduced) {
          // A soft light around the reading position.
          const d = Math.abs(c - headCell);
          if (d < 6) {
            const pulse = 0.5 + 0.5 * Math.sin(now / 260);
            ctx.globalAlpha = (1 - d / 6) * (0.35 + 0.3 * pulse);
            ctx.fillStyle = accent;
            ctx.beginPath();
            ctx.arc(x, y, r * 1.6, 0, Math.PI * 2);
            ctx.fill();
            ctx.globalAlpha = 1;
          }
        }
      } else {
        if (k && this.copied[c]) fill = KIND_COLOR[k];
        else if (k) fill = withAlpha(KIND_COLOR[k], 0.28);
        if (this.active[c] && !MOTION.reduced) {
          const pulse = 0.5 + 0.5 * Math.sin(now / 200 + c * 0.4);
          fill = withAlpha(KIND_COLOR[k] || accent, 0.35 + 0.5 * pulse);
        }
      }

      const b = this.bloom[c];
      if (b) {
        const t = (now - b) / 900;
        if (t < 1) {
          // Bloom: a ring that opens and fades.
          ctx.strokeStyle = withAlpha(KIND_COLOR[k] || accent, (this.mode === "scan" ? 0.6 : 0.3) * (1 - t));
          ctx.lineWidth = 1.5;
          ctx.beginPath();
          ctx.arc(x, y, r + easeOut(t) * r * 3.2, 0, Math.PI * 2);
          ctx.stroke();
          rad = r * (1 + 0.5 * (1 - easeOut(t)));
        } else {
          this.bloom[c] = 0;
        }
      }

      ctx.fillStyle = fill;
      ctx.beginPath();
      ctx.arc(x, y, rad, 0, Math.PI * 2);
      ctx.fill();
    }
  }
}

/// A counter lifts slightly when something lands in it.
function bump(el) {
  if (!el || MOTION.reduced) return;
  const now = performance.now();
  if (now - (el._bumped || 0) < 500) return;
  el._bumped = now;
  el.classList.add("bump");
  setTimeout(() => el.classList.remove("bump"), 420);
}

/// Counts a number up to its value, easing out.
function countUp(el, to, ms = 1000) {
  const from = Number(el.dataset.value || 0);
  el.dataset.value = String(to);
  if (MOTION.reduced || from === to) {
    el.textContent = to.toLocaleString();
    return;
  }
  const t0 = performance.now();
  const step = (now) => {
    const t = Math.min(1, (now - t0) / ms);
    el.textContent = Math.round(from + (to - from) * easeOut(t)).toLocaleString();
    if (t < 1 && el.dataset.value === String(to)) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}
