/* dotz — workflow graph: SVG DAG render + node-detail drawer + live-editing + run record.
 * Split from app.js (C7). No behavior change — pure mechanical split.
 * Preserves: Q2 render-signature guard + rAF debounce (computeWfRenderSignature / refreshWorkflowGraph),
 * Q3 PANEL_REGISTRY-derived PANEL_COLOR + panelForToolJS, C5 viewport culling (nodeIntersectsViewport
 * inside renderWorkflowDag, driven by applyViewBoxAndCull on pan/zoom/fit/reset).
 */
import { $, el, api, post, esc, truncate } from './api.js';
import { state } from './state.js';
import { openPanel, PANEL_REGISTRY } from './panels.js';
import { pushError, pushInfo } from './chat.js';
import { logBrain } from './panels/brain.js';
import { focusSandboxRun } from './handlers.js';
import { refreshBrowserScreenshot } from './panels/browser.js';

function wireGraphPanel(node) {
  const svg = node.querySelector("#wf-svg");
  let dragging = false; let startX, startY, startView;
  svg.addEventListener("mousedown", (e) => {
    if (e.target.closest(".wf-node")) return;
    dragging = true; startX = e.clientX; startY = e.clientY; startView = { ...state.wfView };
  });
  svg.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    state.wfView.x = startView.x - (e.clientX - startX);
    state.wfView.y = startView.y - (e.clientY - startY);
    applyViewBoxAndCull();
  });
  svg.addEventListener("mouseup", () => { dragging = false; });
  svg.addEventListener("mouseleave", () => { dragging = false; });
  svg.addEventListener("wheel", (e) => {
    e.preventDefault();
    const scale = e.deltaY > 0 ? 1.12 : 0.88;
    state.wfView.w *= scale; state.wfView.h *= scale;
    applyViewBoxAndCull();
  });
  node.querySelector("#wf-fit").onclick = fitGraph;
  node.querySelector("#wf-reset").onclick = resetGraph;
  node.querySelector("#wf-record").onclick = openRunRecord;
  node.querySelector("#wf-replay").onclick = replayActiveRun;
  // run-record-close lives in the global body (the run-record overlay), NOT inside the graph panel
  // template — scope this lookup to the document, else node.querySelector returns null and the
  // ".onclick" throws, aborting the rest of wireGraphPanel (incl. refreshWorkflowGraph) on every open.
  $("run-record-close").onclick = () => $("run-record").classList.add("hidden");
  // Reset the render-signature guard so a freshly (re)mounted graph panel always draws, even if
  // state.workflows hasn't changed since the last render on the previous panel instance.
  lastWfSignature = "";
  if (pendingWfRender) { pendingWfRender = false; } // cancel any stale frame from the prior instance
  refreshWorkflowGraph();
}

function applyViewBox() {
  const svg = document.querySelector('.panel[data-panel="graph"] #wf-svg') || $("wf-svg");
  if (svg) svg.setAttribute("viewBox", `${state.wfView.x} ${state.wfView.y} ${state.wfView.w} ${state.wfView.h}`);
}

// applyViewBox + invalidate the Q2 render signature + schedule a refreshWorkflowGraph frame. Used
// by pan/zoom/fit/reset: the signature guard would otherwise skip the redraw (workflow state didn't
// change, only the viewport did), so newly-visible nodes would never render until the next step
// event. The rAF debounce in refreshWorkflowGraph coalesces rapid pan/zoom into one frame. No-op
// if the graph panel isn't open (applyViewBox is a no-op then too).
function applyViewBoxAndCull() {
  applyViewBox();
  lastWfSignature = ""; // invalidate so refreshWorkflowGraph proceeds
  refreshWorkflowGraph();
}

function fitGraph() {
  const panel = document.querySelector('.panel[data-panel="graph"]');
  const svg = panel ? panel.querySelector("#wf-svg") : $("wf-svg");
  if (!svg) return;
  const bbox = svg.getBBox ? svg.getBBox() : { x: 0, y: 0, width: 800, height: 600 };
  const pad = 40;
  state.wfView.x = bbox.x - pad;
  state.wfView.y = bbox.y - pad;
  state.wfView.w = bbox.width + pad * 2;
  state.wfView.h = bbox.height + pad * 2;
  applyViewBoxAndCull();
}

function resetGraph() {
  state.wfView = { x: 0, y: 0, w: 800, h: 600 };
  applyViewBoxAndCull();
}

// Open the graph panel if it isn't already, then run a callback after the DOM settles. Graph
// commands are no-ops without the panel mounted (the SVG lives inside it).
function ensureGraphPanel() {
  if (!document.querySelector('.panel[data-panel="graph"]')) openPanel("graph");
}

// Focus a step in the active workflow run: opens the graph panel, picks the first step (or the one
// already shown in the node drawer), and surfaces its detail. Falls back gracefully when there's
// no run yet — better a truthful no-op than a fabricated navigation.
function focusGraphStep() {
  ensureGraphPanel();
  const run = state.workflows.get(state.activeWfId) || [...state.workflows.values()][0];
  if (!run || !run.steps || !run.steps.length) { pushError("no workflow steps to focus"); return; }
  let step = null;
  if (state.openNodeDetail) step = run.steps.find((s) => s.id === state.openNodeDetail.stepId);
  if (!step) step = run.steps.find((s) => s.status === "running") || run.steps[0];
  showNodeDetail(run, step);
}

// Re-run the step currently open in the node-detail drawer. Requires the drawer to be open so the
// palette isn't guessing which step the operator means.
function rerunOpenStep(feedback) {
  if (!state.openNodeDetail) { pushError("open a step in the graph first"); return; }
  const run = state.workflows.get(state.openNodeDetail.runId);
  if (!run) { pushError("workflow run not found"); return; }
  const step = run.steps.find((s) => s.id === state.openNodeDetail.stepId);
  if (!step) { pushError("step not found"); return; }
  rerunStep(step, feedback);
}

// Structural signature of the workflow render state — captures run id+status+activeWfId and, per
// step, id+status+toolCall count+live-tool phase/error counts. Any structural change (add/remove
// step, edge count shift) OR status change (pending→running→done) OR tool-chip count or chip-state
// change (a tool fires or completes mid-run, populating/mutating _liveTools before toolCalls is
// authoritative on completion) OR active-tab switch invalidates. Used by refreshWorkflowGraph to
// skip redundant redraws and coalesce burst events into one frame.
let pendingWfRender = false;
let lastWfSignature = "";
function computeWfRenderSignature() {
  const runs = [...state.workflows.values()].map((r) =>
    r.id + ":" + r.status + ":" + (r.summary ? (r.summary.completed + "/" + r.summary.steps + "/" + r.summary.running + "/" + r.summary.failed + "/" + r.summary.skipped) : "")
      + ":" + r.steps.map((s) => {
        const lt = s._liveTools ? Object.values(s._liveTools) : [];
        const running = lt.filter((t) => t.phase !== "end").length;
        const errs = lt.filter((t) => t.isError).length;
        return s.id + s.status + ((s.toolCalls && s.toolCalls.length) || 0) + "/" + lt.length + "/" + running + "/" + errs;
      }).join(",")
  ).join("|");
  return state.activeWfId + "#" + runs;
}

function refreshWorkflowGraph() {
  const panel = document.querySelector('.panel[data-panel="graph"]');
  if (!panel) return;
  // Signature guard + rAF debounce: skip if nothing changed since the last render, and coalesce
  // burst events (a 100-node run can fire dozens of step_state/step_tool events per second) into
  // a single frame. The render body reads live state inside the callback, so the last event's
  // state is always what gets drawn — no lost updates.
  const sig = computeWfRenderSignature();
  if (sig === lastWfSignature) return;
  if (pendingWfRender) return; // a frame is already scheduled; it will render this call's state
  pendingWfRender = true;
  requestAnimationFrame(() => {
    pendingWfRender = false;
    lastWfSignature = computeWfRenderSignature(); // recompute: state may have mutated since call
    _refreshWorkflowGraphInner(panel);
  });
}

function _refreshWorkflowGraphInner(panel) {
  const tabsEl = panel.querySelector("#wf-tabs");
  const emptyEl = panel.querySelector("#wf-empty");
  const nodesG = panel.querySelector("#wf-nodes");
  const edgesG = panel.querySelector("#wf-edges");
  const statusEl = panel.querySelector("#wf-status");
  if (!tabsEl || !nodesG || !edgesG) return;
  tabsEl.innerHTML = "";
  nodesG.innerHTML = "";
  edgesG.innerHTML = "";
  if (state.workflows.size === 0) {
    emptyEl.classList.remove("hidden");
    if (statusEl) statusEl.textContent = "";
    return;
  }
  emptyEl.classList.add("hidden");
  [...state.workflows.values()].forEach((run) => {
    const tab = el("div", "wf-tab " + run.status + (run.id === state.activeWfId ? " active" : ""));
    tab.appendChild(el("span", "wf-tab-dot"));
    tab.appendChild(el("span", null, truncate(run.label, 24)));
    tab.onclick = () => { state.activeWfId = run.id; refreshWorkflowGraph(); };
    tabsEl.appendChild(tab);
  });
  const run = state.workflows.get(state.activeWfId) || [...state.workflows.values()][0];
  if (!run) return;
  if (statusEl) {
    // Prefer the pre-computed `summary` from the server (steps/completed/failed/running)
    // so we render progress counts without walking the DAG client-side. Fall back to the
    // raw step count for runs that predate the summary field (e.g. loaded from old history).
    const s = run.summary;
    if (s) {
      const parts = [`${s.completed}/${s.steps} done`];
      if (s.running) parts.push(`${s.running} running`);
      if (s.failed) parts.push(`${s.failed} failed`);
      if (s.skipped) parts.push(`${s.skipped} skipped`);
      statusEl.textContent = parts.join(" · ") + " · " + run.status;
    } else {
      statusEl.textContent = `${run.steps.length} steps · ${run.status}`;
    }
  }
  renderWorkflowDag(run, panel);
}

// Panel accent colors for tool sub-node chips — derived from PANEL_REGISTRY (only entries that
// declare a `color` appear here; panels without one fall back to var(--surface-2) at call sites).
const PANEL_COLOR = Object.fromEntries(
  PANEL_REGISTRY.filter((e) => e.color).map((e) => [e.name, e.color])
);

// JS mirror of the backend `panel_for_tool` — maps a tool name to its bento panel (for chip color
// + click-to-open). Derived from PANEL_REGISTRY.toolMap. Kept in sync with
// dotz-core/src/agent/session.rs::panel_for_tool. The matcher set is disjoint (no tool name
// matches two panels' toolMaps), so registry-order traversal yields the same result as the
// original if/else chain for every input.
function panelForToolJS(name) {
  if (!name) return null;
  for (const entry of PANEL_REGISTRY) {
    if (!entry.toolMap) continue;
    for (const m of entry.toolMap) {
      if (m.prefix != null) { if (name.startsWith(m.prefix)) return entry.name; }
      else if (m.exact != null) {
        const ex = Array.isArray(m.exact) ? m.exact : [m.exact];
        if (ex.includes(name)) return entry.name;
      }
    }
  }
  return null;
}

// The tools a step touched, as chips. Prefer the durable `toolCalls` (set on completion); while the
// step runs, fall back to the live `_liveTools` map that `step_tool` events populate in real time.
// Each entry now carries capped `args`/`result` so the drawer renders inspectable tool cards, not
// just colored chips.
function stepTools(step) {
  if (step.toolCalls && step.toolCalls.length) {
    return step.toolCalls.map((tc) => ({
      toolName: tc.toolName, panel: tc.panel, isError: tc.isError, running: false,
      toolCallId: tc.toolCallId, args: tc.args, result: tc.result,
    }));
  }
  return Object.values(step._liveTools || {}).map((t) => ({
    toolName: t.toolName, panel: t.panel, isError: t.isError, running: t.phase !== "end",
    toolCallId: t.toolCallId, args: t.args, result: t.result,
  }));
}

// The panel a node opens on click: the most-used panel among its tool chips.
function dominantPanel(step) {
  const counts = {};
  for (const t of stepTools(step)) {
    const p = t.panel || panelForToolJS(t.toolName);
    if (p) counts[p] = (counts[p] || 0) + 1;
  }
  let best = null, n = 0;
  for (const [p, c] of Object.entries(counts)) if (c > n) { best = p; n = c; }
  return best;
}

function renderWorkflowDag(run, panel) {
  const nodesG = panel.querySelector("#wf-nodes");
  const edgesG = panel.querySelector("#wf-edges");
  const layers = computeLayers(run.steps);
  const positions = {};
  const NODE_W = 160, NODE_H = 58, LAYER_GAP = 190, NODE_GAP = 24;
  const DOTZ_Y = 24, DOTZ_DROP = 150;
  // Lay out nodes in a FIXED user-space canvas, not the live (zoom/pan/fit-mutated) viewBox width,
  // so a workflow event mid-run doesn't re-center every node and make them visibly jump. Zoom/pan/fit
  // only move the SVG viewBox; node coordinates stay put.
  const CANVAS_W = 800;
  layers.forEach((layer, i) => {
    const layerWidth = layer.length * (NODE_W + NODE_GAP) - NODE_GAP;
    const startX = (CANVAS_W - layerWidth) / 2;
    layer.forEach((stepId, j) => {
      positions[stepId] = { x: startX + j * (NODE_W + NODE_GAP), y: DOTZ_Y + DOTZ_DROP + i * LAYER_GAP };
    });
  });
  // ponytail: graph virtualization — viewport culling; full virtualization (windowing with stable
  // refs) is the upgrade path if 500 visible nodes still thrash. Only NODES are culled — edges render
  // as today and the SVG clips off-screen portions naturally. Pan/zoom/fit/reset call
  // applyViewBoxAndCull which invalidates the Q2 render signature so refreshWorkflowGraph redraws.
  const CULL_MARGIN = NODE_W * 2; // 2× node width so nodes don't pop in/out at the viewport edge
  const nodeIntersectsViewport = (x, y) =>
    x + NODE_W > state.wfView.x - CULL_MARGIN && x < state.wfView.x + state.wfView.w + CULL_MARGIN &&
    y + NODE_H > state.wfView.y - CULL_MARGIN && y < state.wfView.y + state.wfView.h + CULL_MARGIN;
  // The main dotz agent (lead orchestrator) sits above the whole graph; every ROOT step (a subagent
  // it dispersed) hangs off it, so the fan-out reads as "dotz → reviewers" with real connector lines.
  const dotzPos = { x: CANVAS_W / 2 - NODE_W / 2, y: DOTZ_Y };
  const rootSteps = run.steps.filter((s) => !s.parents || s.parents.length === 0);
  const dotzStatus = run.status === "done" ? "done" : (run.status === "error" || run.status === "aborted") ? "error" : "running";
  // dotz → each root subagent (the dispersal edges)
  rootSteps.forEach((step) => {
    const to = positions[step.id];
    if (!to) return;
    const e = document.createElementNS("http://www.w3.org/2000/svg", "path");
    const fx = dotzPos.x + NODE_W / 2, fy = dotzPos.y + NODE_H;
    const tx = to.x + NODE_W / 2, ty = to.y, midY = (fy + ty) / 2;
    e.setAttribute("d", `M ${fx} ${fy} C ${fx} ${midY} ${tx} ${midY} ${tx} ${ty}`);
    e.setAttribute("class", "wf-edge wf-edge-disperse " + (step.status === "running" ? "active" : step.status === "done" ? "done" : ""));
    e.setAttribute("marker-end", "url(#wf-arrow)");
    edgesG.appendChild(e);
  });

  run.steps.forEach((step) => {
    step.parents.forEach((pid) => {
      const from = positions[pid]; const to = positions[step.id];
      if (!from || !to) return;
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      const fx = from.x + NODE_W / 2; const fy = from.y + NODE_H;
      const tx = to.x + NODE_W / 2; const ty = to.y;
      const midY = (fy + ty) / 2;
      path.setAttribute("d", `M ${fx} ${fy} C ${fx} ${midY} ${tx} ${midY} ${tx} ${ty}`);
      const edgeClass = "wf-edge " + (step.status === "ready" ? "ready" : step.status === "running" ? "active" : step.status === "done" ? "done" : "");
      path.setAttribute("class", edgeClass);
      path.setAttribute("marker-end", "url(#wf-arrow)");
      edgesG.appendChild(path);
    });
  });

  run.steps.forEach((step) => {
    const pos = positions[step.id];
    if (!pos) return;
    // Viewport culling: skip rendering nodes whose user-space bbox is off-screen. The selected
    // node (drawer-bound) is always rendered so the persistent ring stays visible even if the
    // user panned it off-screen — the ring tells the operator which graph node the drawer reflects.
    const isSelected = state.openNodeDetail && state.openNodeDetail.runId === run.id &&
      state.openNodeDetail.stepId === step.id;
    if (!isSelected && !nodeIntersectsViewport(pos.x, pos.y)) return;
    const g = document.createElementNS("http://www.w3.org/2000/svg", "g");
    // The selected class marks the node the drawer is bound to so the operator can see which
    // graph node the detail panel reflects at a glance — a persistent ring, not just a hover.
    g.setAttribute("class", "wf-node " + step.status + (isSelected ? " selected" : ""));
    g.setAttribute("transform", `translate(${pos.x}, ${pos.y})`);
    g.dataset.stepId = step.id;

    const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    rect.setAttribute("width", NODE_W); rect.setAttribute("height", NODE_H); rect.setAttribute("rx", 6);
    g.appendChild(rect);

    const ring = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    ring.setAttribute("x", 3); ring.setAttribute("y", 3);
    ring.setAttribute("width", NODE_W - 6); ring.setAttribute("height", NODE_H - 6); ring.setAttribute("rx", 5);
    ring.setAttribute("class", "wf-node-ring");
    g.appendChild(ring);

    const iconText = step.agent ? step.agent.slice(0, 2).toUpperCase() : "◆";
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "text");
    icon.setAttribute("x", 12); icon.setAttribute("y", 22); icon.setAttribute("class", "wf-node-icon");
    icon.setAttribute("fill", "currentColor");
    icon.textContent = iconText;
    g.appendChild(icon);

    const label = document.createElementNS("http://www.w3.org/2000/svg", "text");
    label.setAttribute("x", 42); label.setAttribute("y", 17); label.setAttribute("class", "wf-node-label");
    label.textContent = truncate(step.agent, 14);
    g.appendChild(label);

    const task = document.createElementNS("http://www.w3.org/2000/svg", "text");
    task.setAttribute("x", 10); task.setAttribute("y", 40); task.setAttribute("class", "wf-node-task");
    task.textContent = truncate(step.task, 26);
    g.appendChild(task);

    const status = document.createElementNS("http://www.w3.org/2000/svg", "text");
    status.setAttribute("x", NODE_W - 8); status.setAttribute("y", 16); status.setAttribute("text-anchor", "end");
    status.setAttribute("class", "wf-node-task");
    status.textContent = step.status;
    g.appendChild(status);

    // Tool sub-nodes: one panel-colored chip per tool call, in a row beneath the node. They stream
    // in live (running → done/error) as the step's tools fire, and clicking one opens its panel.
    const NS = "http://www.w3.org/2000/svg";
    const tools = stepTools(step);
    if (tools.length) {
      const CHIP = 16, GAP = 4, PER_ROW = Math.max(1, Math.floor((NODE_W + GAP) / (CHIP + GAP))), MAX = PER_ROW * 2;
      tools.slice(0, MAX).forEach((t, k) => {
        const row = Math.floor(k / PER_ROW), col = k % PER_ROW;
        const panel = t.panel || panelForToolJS(t.toolName);
        const chip = document.createElementNS(NS, "g");
        chip.setAttribute("class", "wf-chip" + (t.isError ? " err" : "") + (t.running ? " running" : ""));
        chip.setAttribute("transform", `translate(${col * (CHIP + GAP)}, ${NODE_H + 8 + row * (CHIP + GAP)})`);
        const c = document.createElementNS(NS, "rect");
        c.setAttribute("width", CHIP); c.setAttribute("height", CHIP); c.setAttribute("rx", 4);
        c.setAttribute("fill", t.isError ? "var(--red)" : (PANEL_COLOR[panel] || "var(--surface-2)"));
        chip.appendChild(c);
        const gl = document.createElementNS(NS, "text");
        gl.setAttribute("x", CHIP / 2); gl.setAttribute("y", 11); gl.setAttribute("text-anchor", "middle");
        gl.setAttribute("class", "wf-chip-glyph");
        gl.textContent = ((t.toolName || "?")[0] || "?").toUpperCase();
        chip.appendChild(gl);
        const title = document.createElementNS(NS, "title");
        title.textContent = (t.toolName || "?") + (t.isError ? " (error)" : t.running ? " (running)" : "") + (panel ? " → " + panel : "");
        chip.appendChild(title);
        chip.style.cursor = "pointer";
        chip.addEventListener("click", (ev) => { ev.stopPropagation(); if (panel) openPanel(panel); showNodeDetail(run, step); });
        g.appendChild(chip);
      });
      if (tools.length > MAX) {
        const more = document.createElementNS(NS, "text");
        more.setAttribute("x", 1); more.setAttribute("y", NODE_H + 8 + 2 * (CHIP + GAP) + 9);
        more.setAttribute("class", "wf-chip-glyph"); more.setAttribute("fill", "var(--muted)");
        more.textContent = `+${tools.length - MAX}`;
        g.appendChild(more);
      }
    }

    g.onclick = () => { const p = dominantPanel(step); if (p) openPanel(p); showNodeDetail(run, step); };
    nodesG.appendChild(g);
  });

  // The dotz orchestrator node — the lead agent, rendered last so it sits on top, glowing.
  {
    const g = document.createElementNS("http://www.w3.org/2000/svg", "g");
    g.setAttribute("class", "wf-node wf-node-dotz " + dotzStatus);
    g.setAttribute("transform", `translate(${dotzPos.x}, ${dotzPos.y})`);
    const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    rect.setAttribute("width", NODE_W); rect.setAttribute("height", NODE_H); rect.setAttribute("rx", 8);
    g.appendChild(rect);
    const ring = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    ring.setAttribute("x", 3); ring.setAttribute("y", 3); ring.setAttribute("width", NODE_W - 6); ring.setAttribute("height", NODE_H - 6); ring.setAttribute("rx", 7);
    ring.setAttribute("class", "wf-node-ring");
    g.appendChild(ring);
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "text");
    icon.setAttribute("x", 12); icon.setAttribute("y", 23); icon.setAttribute("class", "wf-node-icon"); icon.setAttribute("fill", "currentColor");
    icon.textContent = "◆";
    g.appendChild(icon);
    const label = document.createElementNS("http://www.w3.org/2000/svg", "text");
    label.setAttribute("x", 42); label.setAttribute("y", 17); label.setAttribute("class", "wf-node-label");
    label.textContent = "dotz";
    g.appendChild(label);
    const sub = document.createElementNS("http://www.w3.org/2000/svg", "text");
    sub.setAttribute("x", 10); sub.setAttribute("y", 40); sub.setAttribute("class", "wf-node-task");
    sub.textContent = truncate(run.label || "orchestrator", 26);
    g.appendChild(sub);
    const st = document.createElementNS("http://www.w3.org/2000/svg", "text");
    st.setAttribute("x", NODE_W - 8); st.setAttribute("y", 16); st.setAttribute("text-anchor", "end"); st.setAttribute("class", "wf-node-task");
    st.textContent = dotzStatus;
    g.appendChild(st);
    // The dotz orchestrator node is clickable like every other node: it opens the run-record
    // (the full reproducible capture of the run) so the operator can inspect the lead agent's
    // prompt + model + the whole DAG's provider responses — consistent with "every node reveals
    // what the agent is doing."
    g.style.cursor = "pointer";
    g.onclick = () => openRunRecord();
    nodesG.appendChild(g);
  }
  applyViewBox();
}

function computeLayers(steps) {
  const layers = [];
  const placed = new Set();
  const remaining = [...steps];
  while (remaining.length) {
    const layer = [];
    for (let i = remaining.length - 1; i >= 0; i--) {
      const s = remaining[i];
      if (s.parents.length === 0 || s.parents.every((p) => placed.has(p))) {
        layer.push(s.id); placed.add(s.id); remaining.splice(i, 1);
      }
    }
    if (layer.length === 0) {
      remaining.forEach((s) => { layer.push(s.id); placed.add(s.id); });
      remaining.length = 0;
    }
    layers.push(layer);
  }
  return layers;
}

/* ---------- node detail side drawer ---------- */
function bindNodeDetail() {
  $("node-detail-close").onclick = () => { $("node-detail").classList.add("hidden"); state.openNodeDetail = null; };
}

function showNodeDetail(run, step) {
  const drawer = $("node-detail");
  const body = $("node-detail-body");
  state.openNodeDetail = { runId: run.id, stepId: step.id };
  drawer.classList.remove("hidden");
  body.innerHTML = "";
  body.appendChild(makeNdRow("AGENT", step.agent));
  body.appendChild(makeNdRow("TASK", step.task));
  body.appendChild(makeNdRow("STATUS", step.status));
  const specId = step.specChangeId || step.specId || step.changeId;
  const taskId = step.specTaskId || step.taskId;
  const commitId = step.commitId || step.commitSha;
  const readinessStatus = step.readinessStatus || step.readiness;
  if (specId) body.appendChild(makeNdRow("SPEC", specId));
  if (taskId) body.appendChild(makeNdRow("SPEC TASK", taskId));
  if (commitId) {
    const row = makeNdRow("COMMIT", "");
    const link = el("span", "nd-link", String(commitId).slice(0, 12));
    link.onclick = () => {
      openPanel("vcs");
      setTimeout(() => {
        const input = $("vcs-rollback-target");
        if (input) input.value = commitId;
      }, 0);
    };
    row.querySelector(".val").appendChild(link);
    body.appendChild(row);
  }
  if (readinessStatus) body.appendChild(makeNdRow("READINESS", readinessStatus));
  if (step.rollbackTarget || commitId) {
    const row = makeNdRow("ROLLBACK", "");
    const btn = el("button", "btn-mini btn-stop", "OPEN VCS");
    btn.onclick = () => {
      openPanel("vcs");
      setTimeout(() => {
        const input = $("vcs-rollback-target");
        if (input) input.value = step.rollbackTarget || commitId || "";
      }, 0);
    };
    row.querySelector(".val").appendChild(btn);
    body.appendChild(row);
  }

  // ---- ARTIFACT: the primary, inspectable result of a worker step ----
  // Rendered ABOVE the prose output so the operator sees the concrete
  // change (git diff) first, with inline approve/review controls. For
  // review steps the artifact is the findings markdown.
  if (step.artifact && step.artifact.content) {
    const art = step.artifact;
    const artWrap = el("div", "nd-artifact");
    const artTitle = el("div", "nd-artifact-title", art.title || (art.kind === "git_diff" ? "CHANGES" : art.kind));
    artWrap.appendChild(artTitle);

    if (art.kind === "git_diff") {
      // Render the unified diff with basic syntax coloring.
      const pre = el("pre", "nd-diff");
      pre.textContent = art.content;
      pre.classList.add("nd-diff-colored");
      artWrap.appendChild(pre);

      // Inline approve / request-changes controls for worker steps in
      // terminal state (the operator reviewing the result).
      if (step.status === "done" || step.status === "error") {
        const controls = el("div", "nd-artifact-controls");
        const approveBtn = el("button", "nd-btn nd-btn-approve", "✓ Approve");
        approveBtn.onclick = () => actOnStep(step, "approve");
        const rejectBtn = el("button", "nd-btn nd-btn-reject", "✗ Request changes");
        rejectBtn.onclick = () => actOnStep(step, "reject");
        controls.appendChild(approveBtn);
        controls.appendChild(rejectBtn);
        artWrap.appendChild(controls);
      }
    } else {
      // Generic artifact (review findings markdown, future kinds).
      artWrap.appendChild(el("div", "nd-block", art.content));
    }
    body.appendChild(artWrap);
  }

  if (step.output) {
    body.appendChild(makeNdRow("OUTPUT", ""));
    body.appendChild(el("div", "nd-block", step.output));
  }
  if (step.error) {
    body.appendChild(makeNdRow("ERROR", ""));
    body.appendChild(el("div", "nd-block red", step.error));
  }
  if (step.usage) body.appendChild(makeNdRow("USAGE", JSON.stringify(step.usage)));
  // ---- THINKING (live-streaming) ----
  // While a step runs, the executor bridges subagent reasoning onto the graph channel as
  // `step_thinking` events; `_liveThinking` accumulates them. On completion the step's static
  // `thinking` field is set. Prefer the live buffer while running so the drawer is a live
  // reasoning surface; fall back to the durable field after completion.
  const liveThink = step._liveThinking || {};
  const showThinking = (liveThink.thinking && liveThink.thinking.trim()) || (liveThink.text && liveThink.text.trim());
  if (showThinking || (step.thinking && step.thinking.trim())) {
    body.appendChild(makeNdRow("THINKING", ""));
    const wrap = el("details", "nd-thinking");
    wrap.open = step.status === "running"; // expand while streaming, collapse when done
    const sum = el("summary", "nd-thinking-summary", step.status === "running" ? "reasoning…" : "reasoning");
    wrap.appendChild(sum);
    if (liveThink.thinking && liveThink.thinking.trim()) {
      const t = el("div", "nd-block nd-thinking-thinking", liveThink.thinking);
      wrap.appendChild(t);
    }
    if (liveThink.text && liveThink.text.trim()) {
      const t = el("div", "nd-block nd-thinking-text", liveThink.text);
      wrap.appendChild(t);
    }
    // Durable field (post-completion) when the live buffer is empty.
    if ((!liveThink.thinking && !liveThink.text) && step.thinking) {
      wrap.appendChild(el("div", "nd-block", step.thinking));
    }
    body.appendChild(wrap);
  }
  if (step.sandboxRunId) {
    const row = makeNdRow("SANDBOX", "");
    const link = el("span", "nd-link", step.sandboxRunId.slice(0, 12));
    link.onclick = () => { openPanel("sandbox"); focusSandboxRun(step.sandboxRunId); };
    row.querySelector(".val").appendChild(link);
    body.appendChild(row);
  }
  if (step.browserSessionId) {
    const row = makeNdRow("BROWSER", "");
    const link = el("span", "nd-link", step.browserSessionId.slice(0, 12));
    link.onclick = () => { openPanel("browser"); refreshBrowserScreenshot(); };
    row.querySelector(".val").appendChild(link);
    body.appendChild(row);
  }
  // ---- TOOL CALLS (inspectable) ----
  // The graph is the single source of truth: each tool call renders as a collapsible card with
  // name, capped args, capped result, and an error badge — mirroring the chat toolCard. Clicking
  // a card cross-links to the matching chat toolCard (shared toolCallId) so the operator can drill
  // from the graph into the transcript without losing place.
  const tools = stepTools(step);
  if (tools.length) {
    body.appendChild(makeNdRow("TOOL CALLS", String(tools.length)));
    tools.forEach((t) => {
      const card = el("details", "nd-toolcard" + (t.isError ? " err" : "") + (t.running ? " running" : ""));
      card.dataset.tc = t.toolCallId || "";
      const head = el("summary", "nd-toolcard-head");
      const glyph = el("span", "nd-toolcard-glyph", ((t.toolName || "?")[0] || "?").toUpperCase());
      const nameEl = el("span", "nd-toolcard-name", t.toolName || "tool");
      const badge = el("span", "nd-toolcard-badge", t.isError ? "✕" : t.running ? "●" : "✓");
      head.appendChild(glyph); head.appendChild(nameEl); head.appendChild(badge);
      const body2 = el("div", "nd-toolcard-body");
      if (t.args !== undefined && t.args !== null) {
        const lbl = el("div", "nd-toolcard-label", "ARGS");
        const pre = el("pre", "nd-toolcard-args", t.args);
        body2.appendChild(lbl); body2.appendChild(pre);
      }
      if (t.result !== undefined && t.result !== null) {
        const lbl = el("div", "nd-toolcard-label", "RESULT");
        const pre = el("pre", "nd-toolcard-result", t.result);
        body2.appendChild(lbl); body2.appendChild(pre);
      } else if (t.running) {
        body2.appendChild(el("div", "nd-toolcard-running", "running…"));
      }
      card.appendChild(head); card.appendChild(body2);
      // Click-to-chat cross-link: on card click, find the chat toolCard with the same id and
      // scroll it into view + flash it so the operator sees the graph→transcript bridge.
      card.addEventListener("click", (ev) => {
        if (ev.target.tagName === "SUMMARY") return; // let the <details> toggle
        const id = t.toolCallId;
        if (!id) return;
        const tc = document.querySelector(`.panel[data-panel="chat"] .toolcard[data-tc="${CSS.escape(id)}"]`);
        if (tc) {
          tc.scrollIntoView({ behavior: "smooth", block: "center" });
          tc.classList.add("nd-flash");
          setTimeout(() => tc.classList.remove("nd-flash"), 1200);
        }
      });
      body.appendChild(card);
    });
  }
  if (step.startedAt) body.appendChild(makeNdRow("STARTED", new Date(step.startedAt).toLocaleTimeString()));
  if (step.endedAt) body.appendChild(makeNdRow("ENDED", new Date(step.endedAt).toLocaleTimeString()));

  // ---- live-editing controls (the "steer" surface) ----
  // Shown for non-terminal, non-running steps (failed, pending, ready, done).
  const editable = step.status !== "running" && step.status !== "aborted";
  if (editable) {
    const controls = el("div", "nd-controls");
    controls.appendChild(el("div", "nd-controls-title", "STEER"));

    // Model picker
    const modelRow = el("div", "nd-control-row");
    modelRow.appendChild(el("span", "nd-control-label", "Model"));
    const modelInput = el("input", "nd-model-input");
    modelInput.type = "text";
    modelInput.placeholder = step.model || "inherit (agent default)";
    modelInput.value = step.model || "";
    modelInput.addEventListener("keydown", (e) => { if (e.key === "Enter") applyModel(step, modelInput.value); });
    modelRow.appendChild(modelInput);
    const modelBtn = el("button", "nd-btn", "Set");
    modelBtn.onclick = () => applyModel(step, modelInput.value);
    modelRow.appendChild(modelBtn);
    controls.appendChild(modelRow);

    // Parents editor (comma-separated step indices or ids)
    const parentsRow = el("div", "nd-control-row");
    parentsRow.appendChild(el("span", "nd-control-label", "Parents"));
    const parentsInput = el("input", "nd-parents-input");
    parentsInput.type = "text";
    // Show the agent names of current parent steps for readability.
    const parentLabels = (step.parents || []).map((pid) => {
      const p = run.steps.find((s) => s.id === pid);
      return p ? p.agent : pid.slice(0, 8);
    });
    parentsInput.placeholder = "e.g. scout, planner (or leave empty for root)";
    parentsInput.value = parentLabels.join(", ");
    parentsInput.addEventListener("keydown", (e) => { if (e.key === "Enter") applyParents(step, parentsInput.value, run); });
    parentsRow.appendChild(parentsInput);
    const parentsBtn = el("button", "nd-btn", "Wire");
    parentsBtn.onclick = () => applyParents(step, parentsInput.value, run);
    parentsRow.appendChild(parentsBtn);
    controls.appendChild(parentsRow);

    // Feedback + Rerun (only for failed or done-with-error steps)
    if (step.status === "error" || step.error) {
      const fbRow = el("div", "nd-control-row");
      fbRow.appendChild(el("span", "nd-control-label", "Feedback"));
      const fbInput = el("textarea", "nd-feedback");
      fbInput.placeholder = "What went wrong? What should the agent check?";
      fbInput.value = "";
      fbInput.rows = 3;
      fbRow.appendChild(fbInput);
      const rerunBtn = el("button", "nd-btn primary", "↻ Rerun with feedback");
      rerunBtn.onclick = () => rerunStep(step, fbInput.value);
      fbRow.appendChild(rerunBtn);
      controls.appendChild(fbRow);
    } else if (step.status === "done" || step.status === "ready" || step.status === "pending") {
      // Simple rerun without feedback (for done steps that the operator wants to redo)
      const rerunRow = el("div", "nd-control-row");
      const rerunBtn = el("button", "nd-btn", "↻ Rerun step");
      rerunBtn.onclick = () => rerunStep(step, "");
      rerunRow.appendChild(rerunBtn);
      controls.appendChild(rerunRow);
    }

    body.appendChild(controls);
  }
}

function makeNdRow(label, val) {
  const row = el("div", "nd-row");
  row.appendChild(el("span", "label", label + ": "));
  row.appendChild(el("span", "val", val));
  return row;
}

/* ---------- live-editing helpers ---------- */

function applyModel(step, value) {
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  const model = value.trim();
  state.ws.send(JSON.stringify({
    kind: "workflow.patchModel",
    runId: state.openNodeDetail.runId,
    stepId: step.id,
    model: model || null,
  }));
  // Optimistic update.
  step.model = model || null;
  logBrain(model ? `step ${step.agent}: model → ${model}` : `step ${step.agent}: model cleared`);
}

// Approve / request-changes on a worker step from the artifact controls.
// "approve" is a UI-only acknowledgement (the artifact already exists; nothing
// to tell the server). "reject" asks the server to rerun the step with a
// rejection note fed into the repair cycle (workflow.actOnStep arm).
function actOnStep(step, action) {
  if (action === "approve") {
    logBrain(`step ${step.agent}: artifact approved`);
    return;
  }
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  state.ws.send(JSON.stringify({
    kind: "workflow.actOnStep",
    runId: state.openNodeDetail.runId,
    stepId: step.id,
    action,
  }));
  logBrain(`step ${step.agent}: changes requested — repair rerun dispatched`);
}

function applyParents(step, value, run) {
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  // Parse comma-separated agent names or ids. Resolve names back to step ids.
  const tokens = value.split(",").map((s) => s.trim()).filter(Boolean);
  const parentIds = tokens.map((tok) => {
    // Find by agent name first.
    const byAgent = run.steps.find((s) => s.agent === tok && s.id !== step.id);
    if (byAgent) return byAgent.id;
    // Find by prefix match on id.
    const byId = run.steps.find((s) => s.id.startsWith(tok) && s.id !== step.id);
    if (byId) return byId.id;
    // Return as-is (may be a valid id we can't see).
    return tok;
  });
  state.ws.send(JSON.stringify({
    kind: "workflow.patchParents",
    runId: state.openNodeDetail.runId,
    stepId: step.id,
    parents: parentIds,
  }));
  logBrain(`step ${step.agent}: parents wired to [${tokens.join(", ")}]`);
}

function rerunStep(step, feedback) {
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  state.ws.send(JSON.stringify({
    kind: "workflow.rerun",
    runId: state.openNodeDetail.runId,
    stepId: step.id,
    feedback: feedback || null,
  }));
  logBrain(`rerunning step ${step.agent}${feedback ? " with feedback" : ""}`);
}

/* ---------- run record + replay ---------- */

/// POST /api/workflows/:id/replay — rebuild a fresh run from the captured record
/// (same agent/task/model/parents/thinking/auto_repair/budget) and switch the
/// graph to it. The executor is spawned server-side, so the new run transitions
/// live over WebSocket just like the original. This is the orchestration-
/// regression bisect: replay a recorded run after a code/provider/config change
/// and diff the new record against the old to localize which step drifted.
async function replayActiveRun() {
  const runId = state.activeWfId || [...state.workflows.keys()][0];
  if (!runId) { pushError("no active workflow to replay"); return; }
  try {
    const run = await post(`/api/workflows/${runId}/replay`);
    state.workflows.set(run.id, run);
    state.activeWfId = run.id;
    refreshWorkflowGraph();
    pushInfo(`replaying run as “${run.label}”`);
  } catch (e) {
    pushError("replay failed: " + e.message);
  }
}

/// GET /api/workflows/:id/record — render the full reproducible run record
/// (prompt + model + thinking + skill set + provider response messages per step)
/// in a side drawer so the operator can debug exactly what each agent saw,
/// thought, called, and got back — without re-running the workflow.
async function openRunRecord() {
  const runId = state.activeWfId || [...state.workflows.keys()][0];
  if (!runId) { pushError("no active workflow"); return; }
  const body = $("run-record-body");
  if (!body) return;
  body.innerHTML = `<div class="dim mono" style="padding:8px">loading run record…</div>`;
  $("run-record").classList.remove("hidden");
  try {
    const rec = await api(`/api/workflows/${runId}/record`);
    renderRunRecord(rec, body);
  } catch (e) {
    body.innerHTML = `<div class="nd-error" style="padding:8px">${esc(e.message)}</div>`;
  }
}

function renderRunRecord(rec, body) {
  const parts = [];
  parts.push(`<div class="nd-control-row"><span class="dim mono">${esc(rec.label)} · ${esc(rec.status)} · ${rec.steps.length} steps</span></div>`);
  for (const s of rec.steps) {
    const skill = (s.skillSet || []).join(", ") || "(default active set)";
    const msgs = Array.isArray(s.messages) ? s.messages : [];
    const blocks = [];
    for (const m of msgs) {
      const role = esc(m.role || "");
      const c = Array.isArray(m.content) ? m.content : [];
      const inner = c.map((b) => {
        const t = b.type || "";
        if (t === "thinking") return `<div class="nd-thinking"><span class="dim mono">thinking:</span> ${esc(b.thinking || "")}</div>`;
        if (t === "text") return `<div>${esc(b.text || "")}</div>`;
        if (t === "toolCall") return `<div class="nd-toolcall"><span class="mono">${esc(b.name)}</span>(${esc(JSON.stringify(b.arguments || {}))})</div>`;
        return `<div class="dim mono">[${esc(t)}]</div>`;
      }).join("");
      const meta = [m.provider, m.model, m.stopReason].filter(Boolean).map(esc).join(" · ");
      blocks.push(`<div class="nd-msg"><div class="dim mono">${role}${meta ? " · " + meta : ""}</div>${inner}</div>`);
    }
    parts.push(`
      <details class="nd-step">
        <summary class="nd-step-summary ${esc(s.status)}">${esc(s.agent)} · ${esc(s.status)} · ${esc(s.task.slice(0, 80))}</summary>
        <div class="nd-step-body">
          <div class="nd-kv"><span class="dim mono">task:</span> ${esc(s.task)}</div>
          <div class="nd-kv"><span class="dim mono">model:</span> ${esc(s.model || "—")}${s.modelOverride && s.modelOverride !== s.model ? " (override: " + esc(s.modelOverride) + ")" : ""}</div>
          <div class="nd-kv"><span class="dim mono">thinking:</span> ${esc(s.thinking || "—")}</div>
          <div class="nd-kv"><span class="dim mono">skill set:</span> ${esc(skill)}</div>
          <div class="nd-kv"><span class="dim mono">exit/stop:</span> ${esc(String(s.exitCode))} / ${esc(s.stopReason || "—")}</div>
          ${s.errorMessage ? `<div class="nd-error">${esc(s.errorMessage)}</div>` : ""}
          ${s.output ? `<div class="nd-kv"><span class="dim mono">output:</span> ${esc(s.output.slice(0, 400))}${s.output.length > 400 ? "…" : ""}</div>` : ""}
          <div class="nd-kv"><span class="dim mono">provider responses:</span></div>
          ${blocks.join("") || `<div class="dim mono">(no provider calls — step did not run)</div>`}
        </div>
      </details>`);
  }
  body.innerHTML = parts.join("");
}

export {
  wireGraphPanel,
  applyViewBox,
  applyViewBoxAndCull,
  fitGraph,
  resetGraph,
  ensureGraphPanel,
  focusGraphStep,
  rerunOpenStep,
  computeWfRenderSignature,
  refreshWorkflowGraph,
  PANEL_COLOR,
  panelForToolJS,
  stepTools,
  dominantPanel,
  renderWorkflowDag,
  computeLayers,
  bindNodeDetail,
  showNodeDetail,
  makeNdRow,
  applyModel,
  actOnStep,
  applyParents,
  rerunStep,
  replayActiveRun,
  openRunRecord,
  renderRunRecord,
};