/* dotz — keyboard command palette (Ctrl/Cmd+K).
 * Split from app.js (C7). No behavior change — pure mechanical split.
 * A single keyboard surface for graph navigation, panel toggle, model/reasoning switching,
 * and step re-run — built on top of the backend `/api/commands` catalog (the single source of
 * truth for the action taxonomy) merged with UI-only panel-toggle entries (panel names live in
 * the UI, not the backend, so they are NOT duplicated server-side). Fuzzy-filtered,
 * arrow-navigable, Enter fires the highlighted action. A power user never reaches for the mouse.
 */
import { $, el, api } from './api.js';
import { state } from './state.js';
import { PANEL_NAMES, PANEL_META, togglePalette } from './panels.js';
import { pushError } from './chat.js';
import {
  onProviderChange, submitModel, setReasoningLevel, togglePanel,
} from './main.js';
import {
  ensureGraphPanel, fitGraph, resetGraph, focusGraphStep, rerunOpenStep,
} from './graph.js';

const KB_PALLETTE_KEY = "dotz.kbCommands.v1";

function bindKbPalette() {
  const card = $("kb-palette");
  // Click on the backdrop (not the card) closes — same idiom as the panel palette.
  card.onclick = (e) => { if (e.target.id === "kb-palette") hideKbPalette(); };
  const input = $("kb-palette-input");
  input.addEventListener("input", () => renderKbPalette(input.value));
  input.addEventListener("keydown", (e) => {
    // Arrow/Tab cycle the list; Enter runs; Esc closes. Handled here (not in the global handler) so
    // the palette keeps working even if focus somehow leaves the input while the overlay is up.
    if (e.key === "ArrowDown" || (e.key === "Tab" && !e.shiftKey)) { e.preventDefault(); moveKbSelection(1); }
    else if (e.key === "ArrowUp" || (e.key === "Tab" && e.shiftKey)) { e.preventDefault(); moveKbSelection(-1); }
    else if (e.key === "Enter") { e.preventDefault(); runKbSelection(); }
    else if (e.key === "Escape") { e.preventDefault(); hideKbPalette(); }
  });
}

// Fetch + cache the backend catalog. Cached in localStorage so the palette opens instantly on
// repeat invocations (the catalog is effectively static per build); a stale cache is refreshed in
// the background and replaced on next open without blocking the first keystroke.
async function loadKbCommands() {
  const cached = (() => { try { return JSON.parse(localStorage.getItem(KB_PALLETTE_KEY) || "null"); } catch { return null; } })();
  if (cached && Array.isArray(cached) && cached.length) state.kbCommands = cached;
  try {
    const { commands } = await api("/api/commands");
    if (Array.isArray(commands) && commands.length) {
      state.kbCommands = commands;
      try { localStorage.setItem(KB_PALLETTE_KEY, JSON.stringify(commands)); } catch {}
    }
  } catch (e) { pushError("command palette: " + e.message); }
}

// The full palette list: backend action commands + UI-only panel-toggle commands (one per panel,
// toggle = open if absent, close if present). Panels are a UI concern so they live here, not backend.
function kbPaletteItems() {
  const items = (state.kbCommands || []).map((c) => ({ ...c, source: "api" }));
  PANEL_NAMES.forEach((name) => {
    const meta = PANEL_META[name] || { icon: "", label: name };
    const open = state.layout.open.includes(name);
    items.push({
      id: "panel.toggle." + name,
      category: "panel",
      label: (open ? "Close panel: " : "Open panel: ") + meta.label,
      description: open ? "Remove the " + meta.label + " panel from the bento." : "Add the " + meta.label + " panel to the bento.",
      key: null,
      action: "panel:toggle",
      arg: name,
      requiresSession: false,
      source: "ui",
    });
  });
  return items;
}

let _kbFiltered = [];
let _kbIndex = 0;

function toggleKbPalette() {
  const card = $("kb-palette");
  if (!card.classList.contains("hidden")) { hideKbPalette(); return; }
  card.classList.remove("hidden");
  const input = $("kb-palette-input");
  input.value = "";
  renderKbPalette("");
  // Refresh the catalog in the background; the cached list paints immediately so there's no
  // first-open latency, and a new provider/thinking level added server-side shows up next time.
  loadKbCommands().then(() => renderKbPalette(input.value));
  input.focus();
}

function hideKbPalette() {
  $("kb-palette").classList.add("hidden");
  _kbFiltered = [];
  _kbIndex = 0;
}

// Fuzzy subset match: each char of the query must appear in order (case-insensitive). Cheap and
// good enough for ~40 entries; no need for a scoring library.
function kbMatches(hay, q) {
  if (!q) return true;
  hay = String(hay || "").toLowerCase();
  let i = 0;
  for (const ch of q.toLowerCase()) {
    i = hay.indexOf(ch, i);
    if (i < 0) return false;
    i++;
  }
  return true;
}

function renderKbPalette(query) {
  const list = $("kb-palette-list");
  const all = kbPaletteItems();
  // Filter on id/label/description/category; keep category grouping stable by preserving order.
  _kbFiltered = all.filter((c) =>
    kbMatches(c.label, query) || kbMatches(c.id, query) || kbMatches(c.description, query) || kbMatches(c.category, query)
  );
  _kbIndex = _kbFiltered.length ? 0 : -1;
  list.innerHTML = "";
  if (!_kbFiltered.length) {
    list.appendChild(el("div", "kb-palette-empty mono dim", "no commands match “" + query + "”"));
    return;
  }
  // Group by category in catalog order (categories appear as the items stream, no pre-sort).
  let lastCat = null;
  _kbFiltered.forEach((c, i) => {
    if (c.category !== lastCat) {
      lastCat = c.category;
      const head = el("div", "kb-palette-cat mono", c.category.toUpperCase());
      list.appendChild(head);
    }
    const row = el("div", "kb-palette-row" + (i === _kbIndex ? " sel" : ""));
    row.setAttribute("role", "option");
    row.dataset.idx = String(i);
    const label = el("span", "kb-palette-label", c.label);
    if (c.requiresSession && !state.sessionId) label.classList.add("dim");
    row.appendChild(label);
    if (c.key) row.appendChild(el("span", "kb-palette-key mono", c.key));
    row.title = c.description;
    row.onclick = () => { _kbIndex = i; runKbSelection(); };
    list.appendChild(row);
  });
}

function moveKbSelection(dir) {
  if (!_kbFiltered.length) return;
  _kbIndex = (_kbIndex + dir + _kbFiltered.length) % _kbFiltered.length;
  const list = $("kb-palette-list");
  list.querySelectorAll(".kb-palette-row").forEach((r) => r.classList.toggle("sel", Number(r.dataset.idx) === _kbIndex));
  const sel = list.querySelector(".kb-palette-row.sel");
  if (sel && sel.scrollIntoView) sel.scrollIntoView({ block: "nearest" });
}

function runKbSelection() {
  const c = _kbFiltered[_kbIndex];
  if (!c) return;
  // Guard session-required actions so a pre-session keystroke can't fire a graph/step command
  // into a void (the command center has no workflow run / open node).
  if (c.requiresSession && !state.sessionId) { pushError("“" + c.label + "” needs an active session"); return; }
  hideKbPalette();
  dispatchKbCommand(c);
}

// The dispatch table: maps a backend/UI action tag to a side-effecting handler. Adding a new
// action means one branch here + one command in commands.rs — the palette lists it automatically.
function dispatchKbCommand(c) {
  switch (c.action) {
    case "model:provider":
      if (c.arg) onProviderChange(c.arg);
      return;
    case "model:set-id": {
      const id = window.prompt("Executive model id:", $("model-input").value || "");
      if (id && id.trim()) { $("model-input").value = id.trim(); submitModel(); }
      return;
    }
    case "reasoning:set":
      if (c.arg) setReasoningLevel(c.arg);
      return;
    case "graph:fit":
      ensureGraphPanel();
      fitGraph();
      return;
    case "graph:reset":
      ensureGraphPanel();
      resetGraph();
      return;
    case "graph:focus-step":
      focusGraphStep();
      return;
    case "step:rerun":
      rerunOpenStep("");
      return;
    case "step:rerun-feedback": {
      const fb = window.prompt("Feedback for re-run:") || "";
      rerunOpenStep(fb);
      return;
    }
    case "view:panels":
      togglePalette();
      return;
    case "panel:toggle":
      if (c.arg) togglePanel(c.arg);
      return;
  }
}

export {
  bindKbPalette,
  loadKbCommands,
  kbPaletteItems,
  toggleKbPalette,
  hideKbPalette,
  kbMatches,
  renderKbPalette,
  moveKbSelection,
  runKbSelection,
  dispatchKbCommand,
};