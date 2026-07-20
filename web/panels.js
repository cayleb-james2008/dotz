/* dotz — panel registry + mounting. Split from app.js (C7). No behavior change.
 * The PANEL_REGISTRY (Q3) is the single source of truth for panel metadata — adding a panel =
 * one entry here + a <template id="tpl-<name>"> in index.html. PANEL_NAMES / PANEL_META /
 * PANEL_COLOR / panelForToolJS / the mountPanel wirer are all derived from this array.
 */
import { $, el } from './api.js';
import { state, saveLayout } from './state.js';
import { wireChatPanel } from './panels/chat.js';
import { wireGraphPanel } from './panels/graph.js';
import { wireBrainPanel } from './panels/brain.js';
import { wireBrowserPanel } from './panels/browser.js';
import { wireMemoryPanel } from './panels/memory.js';
import { wireFilesPanel } from './panels/files.js';
import { wireSandboxPanel } from './panels/sandbox.js';
import { wireSkillsPanel } from './panels/skills.js';
import { wireTemplatesPanel } from './panels/templates.js';
import { wireDesignPanel } from './panels/design.js';
import { wireSpecPanel } from './panels/spec.js';
import { wireLivingDocsPanel } from './panels/living-docs.js';
import { wireVcsPanel } from './panels/vcs.js';
import { wireConnectionsPanel } from './panels/connections.js';
import { wireDoctrinePanel } from './panels/doctrine.js';
import { wireMarketplacePanel } from './panels/marketplace.js';
import { wirePerfPanel } from './panels/perf.js';

// Each entry: { name, icon, label, color?, wire?, toolMap? }
//   - color: optional CSS-var accent for tool chips (omitted = falls back to var(--surface-2))
//   - wire:  optional panel-wirer function (hoisted function declaration; called by mountPanel)
//   - toolMap: optional array of matchers used to derive panelForToolJS. Each matcher is either
//     {prefix:"memory_"} (startsWith) or {exact:["skill","create_skill",...]} (=== any).
//     Order matters: the first matching entry wins, mirroring the original if/else chain.
const PANEL_REGISTRY = [
  { name: "chat",        icon: "▓", label: "CHAT",            wire: wireChatPanel },
  { name: "graph",       icon: "◐", label: "WORKFLOW GRAPH", color: "var(--cyan)",   wire: wireGraphPanel, toolMap: [{ exact: "subagent" }] },
  { name: "brain",       icon: "◆", label: "AGENT BRAIN",    color: "var(--mauve)",  wire: wireBrainPanel, toolMap: [{ exact: ["rsi_baseline", "rsi_compare"] }] },
  { name: "browser",     icon: "▣", label: "BROWSER",        color: "var(--cyan)",   wire: wireBrowserPanel, toolMap: [{ prefix: "browser_" }] },
  { name: "memory",      icon: "▤", label: "MEMORY",         color: "var(--mauve)",  wire: wireMemoryPanel, toolMap: [{ prefix: "memory_" }] },
  { name: "files",       icon: "▥", label: "FILES",          color: "var(--muted)",  wire: wireFilesPanel, toolMap: [{ exact: ["edit", "write"] }] },
  { name: "sandbox",     icon: "▩", label: "SANDBOX",        color: "var(--peach)",  wire: wireSandboxPanel, toolMap: [{ prefix: "sandbox_" }] },
  { name: "skills",      icon: "✦", label: "SKILLS",         color: "var(--yellow)", wire: wireSkillsPanel, toolMap: [{ exact: ["skill", "create_skill", "list_skills", "create_agent", "list_agents"] }] },
  { name: "templates",   icon: "⬡", label: "TEMPLATES",      wire: wireTemplatesPanel },
  { name: "design",      icon: "❖", label: "DESIGN",         color: "var(--pink)",   wire: wireDesignPanel, toolMap: [{ prefix: "design_" }] },
  { name: "spec",        icon: "◇", label: "SPEC",           color: "var(--peach)",  wire: wireSpecPanel, toolMap: [{ prefix: "openspec_" }] },
  { name: "living-docs", icon: "◧", label: "LIVING DOCS",    color: "var(--pink)",   wire: wireLivingDocsPanel, toolMap: [{ prefix: "living_docs_" }] },
  { name: "vcs",         icon: "⌁", label: "VCS",            color: "var(--green)",  wire: wireVcsPanel, toolMap: [{ prefix: "vcs_" }] },
  { name: "connections", icon: "⊕", label: "CONNECTIONS",    wire: wireConnectionsPanel },
  { name: "doctrine",    icon: "◈", label: "DOCTRINE",       color: "var(--lav)",    wire: wireDoctrinePanel, toolMap: [{ exact: "agents_md" }] },
  { name: "marketplace", icon: "⚑", label: "MARKETPLACE",    wire: wireMarketplacePanel },
  { name: "perf",         icon: "⚡", label: "PERFORMANCE",    wire: wirePerfPanel },
];
// Derived (kept as const so all existing PANEL_NAMES / PANEL_META call sites work unchanged).
const PANEL_NAMES = PANEL_REGISTRY.map((e) => e.name);
const PANEL_META = Object.fromEntries(PANEL_REGISTRY.map((e) => [e.name, { icon: e.icon, label: e.label }]));

function renderBento() {
  const bento = $("bento");
  bento.innerHTML = "";
  for (const name of state.layout.open) mountPanel(name);
}

function mountPanel(name) {
  if (!PANEL_NAMES.includes(name)) return;
  // Never mount a second node of the same type — duplicates are undeletable and shadow every
  // per-panel [data-panel] lookup (only the first node ever resolves).
  if (document.querySelector(`.panel[data-panel="${name}"]`)) return;
  const tpl = $("tpl-" + name);
  if (!tpl) return;
  const node = tpl.content.firstElementChild.cloneNode(true);
  $("bento").appendChild(node);
  bindPanel(node, name);
  // Registry-driven wiring: one place to add a panel (PANEL_REGISTRY), no if/else chain to edit.
  const entry = PANEL_REGISTRY.find((e) => e.name === name);
  if (entry && entry.wire) entry.wire(node);
}

function unmountPanel(name) {
  if (name === "browser" && state.browserPollTimer) {
    clearInterval(state.browserPollTimer);
    state.browserPollTimer = null;
  }
  if (name === "connections" && state.connectionsPollTimer) {
    clearInterval(state.connectionsPollTimer);
    state.connectionsPollTimer = null;
    state.connectionsLoginProvider = null;
  }
  const node = document.querySelector(`.panel[data-panel="${name}"]`);
  if (node) node.remove();
  state.layout.open = state.layout.open.filter((n) => n !== name);
  saveLayout();
}

function bindPanel(node, name) {
  const head = node.querySelector(".panel-head");
  const closeBtn = node.querySelector(".panel-close");
  if (closeBtn) {
    closeBtn.setAttribute("aria-label", "Close panel");
    closeBtn.onclick = (e) => { e.stopPropagation(); if (name !== "chat") unmountPanel(name); };
  }
  head.addEventListener("dragstart", (e) => {
    node.classList.add("dragging");
    e.dataTransfer.setData("text/plain", name);
    e.dataTransfer.effectAllowed = "move";
  });
  head.addEventListener("dragend", () => node.classList.remove("dragging"));
  head.addEventListener("dragover", (e) => { e.preventDefault(); head.classList.add("drag-over"); });
  head.addEventListener("dragleave", (e) => {
    // Ignore dragleave fired when the cursor crosses onto a child of the header (icon/label/close
    // button) — only clear the highlight when focus truly leaves the header, to avoid flicker.
    if (e.relatedTarget && head.contains(e.relatedTarget)) return;
    head.classList.remove("drag-over");
  });
  head.addEventListener("drop", (e) => {
    e.preventDefault();
    head.classList.remove("drag-over");
    const from = e.dataTransfer.getData("text/plain");
    if (from && from !== name) swapPanels(from, name);
  });
}

// Swap two panels' positions by reordering their DOM nodes (the CSS grid auto-flows in source
// order), keeping each panel's data-panel + content intact. Swapping data-panel instead would
// relocate the grid footprint but de-sync it from the node's content, since data-panel keys every
// per-panel content lookup — that silently kills chat/graph/etc. (see styles.css [data-panel=…]).
function swapPanels(from, to) {
  const fromNode = document.querySelector(`.panel[data-panel="${from}"]`);
  const toNode = document.querySelector(`.panel[data-panel="${to}"]`);
  if (!fromNode || !toNode || fromNode === toNode) return;
  const fromNext = fromNode.nextSibling === toNode ? fromNode : fromNode.nextSibling;
  toNode.parentNode.insertBefore(fromNode, toNode);
  fromNode.parentNode.insertBefore(toNode, fromNext);
  // Persist the new order so renderBento reproduces the swap on reload.
  const open = state.layout.open;
  const i = open.indexOf(from), j = open.indexOf(to);
  if (i >= 0 && j >= 0) { open[i] = to; open[j] = from; }
  saveLayout();
}

function openPanel(name) {
  if (state.layout.open.includes(name)) return;
  state.layout.open.push(name);
  saveLayout();
  mountPanel(name);
}

function togglePalette() {
  const p = $("palette");
  if (!p.classList.contains("hidden")) { p.classList.add("hidden"); return; }
  p.classList.remove("hidden");
  const grid = $("palette-grid");
  grid.innerHTML = "";
  PANEL_NAMES.forEach((name) => {
    const meta = PANEL_META[name];
    const item = el("div", "palette-item" + (state.layout.open.includes(name) ? " open" : ""));
    item.appendChild(el("span", "pi-glyph", meta.icon));
    item.appendChild(el("span", "pi-label", meta.label));
    if (!state.layout.open.includes(name)) {
      item.onclick = () => { openPanel(name); p.classList.add("hidden"); };
    }
    grid.appendChild(item);
  });
}

export {
  PANEL_REGISTRY,
  PANEL_NAMES,
  PANEL_META,
  renderBento,
  mountPanel,
  unmountPanel,
  bindPanel,
  swapPanels,
  openPanel,
  togglePalette,
};