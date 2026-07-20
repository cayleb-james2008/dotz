/* dotz — event handlers (chat / workflow / sandbox / memory-recall / provider-health / gate) +
 * sandbox run/output/cursor helpers + status-bar stats + WS connection chip.
 * Split from app.js (C7). No behavior change — pure mechanical split.
 */
import { $, el, api, post, truncate } from './api.js';
import { state } from './state.js';
import { openPanel } from './panels.js';
import { refreshWorkflowGraph, showNodeDetail } from './graph.js';
import {
  setStreaming, renderAssistantPartial, finalizeAssistant, toolCard, scrollBottom, pushError, pushInfo,
} from './chat.js';
import { logBrain } from './panels/brain.js';
import { renderRecalled } from './panels/memory.js';

// state.providerHealth: { providers: { [id]: { status, consecutive_failures, failures, successes, last_error, ... } }, failoverPairs: { [id]: { primary, backup } } }
// Mutated onto the shared state singleton at module load (mirrors the original post-literal
// assignment in app.js — colocated with handleProviderHealthEvent which is its primary mutator).
state.providerHealth = { providers: {}, failoverPairs: {} };

function stringifyResult(r) {
  if (r == null) return "";
  if (typeof r === "string") return r;
  if (r.content && Array.isArray(r.content)) return r.content.map((c) => c.text || "").join("\n");
  return JSON.stringify(r, null, 2);
}

/* ---------- event handling ---------- */
function handleEvent(e) {
  switch (e.type) {
    case "agent_start": setStreaming(true); logBrain("agent_start"); break;
    case "message_start":
      if (e.message && e.message.role === "assistant") state.cur = null;
      break;
    case "message_update":
      if (e.assistantMessageEvent && e.assistantMessageEvent.partial) renderAssistantPartial(e.assistantMessageEvent.partial);
      break;
    case "message_end":
      if (e.message && e.message.role === "assistant") finalizeAssistant(e.message);
      break;
    case "tool_execution_start": toolCard(e.toolCallId, { name: e.toolName, args: e.args, status: "run" }); logBrain("tool: " + e.toolName); break;
    case "tool_execution_update": toolCard(e.toolCallId, { output: stringifyResult(e.partialResult) }); break;
    case "tool_execution_end": toolCard(e.toolCallId, { status: e.isError ? "err" : "done", output: stringifyResult(e.result), name: e.toolName }); break;
    case "subagent_progress":
      // Live subagent reasoning: stream the partial thinking text into the subagent's
      // tool card so the operator can see a drifting scout/planner's reasoning as it
      // happens rather than only after it finishes.
      if (e.toolCallId && e.partial) {
        const thinking = (e.partial.content || []).filter((b) => b.type === "thinking" || b.type === "text").map((b) => b.thinking || b.text || "").join("");
        if (thinking) toolCard(e.toolCallId, { liveThinking: thinking });
      }
      break;
    case "agent_end": setStreaming(false); refreshStats(); logBrain("agent_end"); break;
  }
  scrollBottom();
}

/* ---------- workflows ---------- */
function handleWorkflowEvent(runId, event) {
  switch (event.type) {
    case "workflow_start":
      state.workflows.set(runId, event.run);
      state.activeWfId = runId;
      openPanel("graph");
      refreshWfCount();
      refreshWorkflowGraph();
      break;
    case "workflow_end":
      state.workflows.set(runId, event.run);
      refreshWfCount();
      refreshWorkflowGraph();
      break;
    case "step_state": {
      const run = state.workflows.get(runId);
      if (!run) break;
      const step = run.steps.find((s) => s.id === event.stepId);
      if (step) {
        step.status = event.status;
        if (event.output !== undefined) step.output = event.output;
        if (event.error !== undefined) step.error = event.error;
        if (event.usage !== undefined) step.usage = event.usage;
        if (event.sandboxRunId !== undefined) step.sandboxRunId = event.sandboxRunId;
        if (event.browserSessionId !== undefined) step.browserSessionId = event.browserSessionId;
        if (event.toolCallIds !== undefined) step.toolCallIds = event.toolCallIds;
        if (event.toolCalls !== undefined) step.toolCalls = event.toolCalls; // authoritative on completion
        if (event.thinking !== undefined) step.thinking = event.thinking;
      }
      // Update the run-level progress summary from the server-sent counts so the
      // status line stays live without refetching the run or walking the DAG.
      if (event.summary !== undefined) run.summary = event.summary;
      refreshWorkflowGraph();
      // Repaint the node-detail drawer in place if it's showing the step that just updated, so the
      // primary live-inspection surface doesn't freeze on the snapshot from when it was opened.
      if (step && state.openNodeDetail && state.openNodeDetail.runId === runId &&
          state.openNodeDetail.stepId === event.stepId && !$("node-detail").classList.contains("hidden")) {
        showNodeDetail(run, step);
      }
      break;
    }
    case "step_tool": {
      // Live per-tool chip: upsert into the step's transient live-tool map as each tool fires
      // (phase start → running chip, end → done/error). Reconciled by `toolCalls` on completion.
      // Now carries capped `args` (on start) and `result` (on end) so the graph node drawer is
      // the single source of truth for "what did this tool do?" — not just a colored chip.
      const run = state.workflows.get(runId);
      if (!run) break;
      const step = run.steps.find((s) => s.id === event.stepId);
      if (!step) break;
      if (!step._liveTools) step._liveTools = {};
      const prev = step._liveTools[event.toolCallId] || {};
      step._liveTools[event.toolCallId] = {
        toolName: event.toolName || prev.toolName,
        panel: event.panel !== undefined ? event.panel : prev.panel,
        phase: event.phase || prev.phase,
        isError: event.isError !== undefined ? event.isError : prev.isError,
        args: event.args !== undefined ? event.args : prev.args,
        result: event.result !== undefined ? event.result : prev.result,
      };
      refreshWorkflowGraph();
      // Repaint the node-detail drawer live so the operator sees args/result the instant a
      // tool fires — the graph is the live source of truth, not a snapshot.
      if (state.openNodeDetail && state.openNodeDetail.runId === runId &&
          state.openNodeDetail.stepId === event.stepId && !$("node-detail").classList.contains("hidden")) {
        showNodeDetail(run, step);
      }
      break;
    }
    case "step_thinking": {
      // Live reasoning bridge: the executor path streams subagent thinking/text deltas onto
      // the workflow channel so the graph node is the live reasoning surface — not only the
      // chat. Coalesced into the step's transient `_liveThinking` buffer; rendered in the
      // node-detail drawer as a streaming block while the step runs.
      const run = state.workflows.get(runId);
      if (!run) break;
      const step = run.steps.find((s) => s.id === event.stepId);
      if (!step) break;
      if (!step._liveThinking) step._liveThinking = { thinking: "", text: "" };
      if (event.phase === "thinking") step._liveThinking.thinking += event.text || "";
      else if (event.phase === "text") step._liveThinking.text += event.text || "";
      refreshWorkflowGraph();
      // Repaint the drawer live so reasoning streams in real time (open <details> while running).
      if (state.openNodeDetail && state.openNodeDetail.runId === runId &&
          state.openNodeDetail.stepId === event.stepId && !$("node-detail").classList.contains("hidden")) {
        showNodeDetail(run, step);
      }
      break;
    }
  }
}

function refreshWfCount() {
  const n = state.workflows.size;
  const st = $("st-workflows");
  if (st) st.textContent = String(n);
}

/* ---------- sandbox ---------- */
function handleSandboxEvent(e) {
  switch (e.type) {
    case "sandbox_start": {
      const run = e.run;
      const rec = { run, output: run.output || "", port: null, status: run.status };
      state.sandbox.runs.set(run.id, rec);
      state.sandbox.activeRunId = run.id;
      const panel = document.querySelector('.panel[data-panel="sandbox"]');
      const rid = panel ? panel.querySelector("#sb-runid") : $("sb-runid");
      if (rid) rid.textContent = "#" + run.id.slice(0, 8);
      const st = panel ? panel.querySelector("#sb-status") : $("sb-status");
      if (st) st.textContent = "running " + run.language;
      const kill = panel ? panel.querySelector("#sb-kill") : $("sb-kill");
      if (kill) kill.classList.remove("hidden");
      renderSandboxRuns();
      refreshSandboxCount();
      break;
    }
    case "sandbox_output": {
      const rec = state.sandbox.runs.get(e.runId);
      if (!rec) break;
      rec.output += e.line + "\n";
      if (e.runId === state.sandbox.activeRunId) appendSandboxOutput(e.line, e.stream);
      break;
    }
    case "sandbox_port": {
      const rec = state.sandbox.runs.get(e.runId);
      if (!rec) break;
      rec.port = e.port;
      if (e.runId === state.sandbox.activeRunId) loadWebPreview(e.port);
      break;
    }
    case "sandbox_cursor": {
      if (e.runId === state.sandbox.activeRunId) renderCursor(e.x, e.y, e.action, e.text);
      break;
    }
    case "sandbox_end": {
      const rec = state.sandbox.runs.get(e.runId);
      if (rec) {
        rec.run = e.run;
        rec.status = e.run.status;
        // REST-started runs (panel RUN button) never stream sandbox_output lines — the end
        // event's run record carries the whole captured output, so sync + render it here.
        if ((e.run.output || "") && rec.output !== e.run.output) {
          rec.output = e.run.output;
          if (e.runId === state.sandbox.activeRunId) showRunOutput(rec);
        }
      }
      if (e.runId === state.sandbox.activeRunId) {
        const panel = document.querySelector('.panel[data-panel="sandbox"]');
        const st = panel ? panel.querySelector("#sb-status") : $("sb-status");
        if (st) st.textContent = "ended: " + e.run.status + (e.run.exitCode != null ? " (exit " + e.run.exitCode + ")" : "");
        const kill = panel ? panel.querySelector("#sb-kill") : $("sb-kill");
        if (kill) kill.classList.add("hidden");
      }
      renderSandboxRuns();
      refreshSandboxCount();
      break;
    }
  }
}

function appendSandboxOutput(line, stream) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const out = panel ? panel.querySelector("#sb-output") : $("sb-output");
  if (!out) return;
  const div = el("div", stream === "stderr" ? "sb-line-stderr" : "");
  div.textContent = line;
  out.appendChild(div);
  out.scrollTop = out.scrollHeight;
}

function toggleWebPreview(web) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const wrap = panel ? panel.querySelector("#sb-web-wrap") : $("sb-web-wrap");
  const outWrap = panel ? panel.querySelector("#sb-output-wrap") : $("sb-output-wrap");
  if (wrap) wrap.classList.toggle("hidden", !web);
  if (outWrap) outWrap.style.flex = web ? "1 1 30%" : "1 1 100%";
  if (!web) {
    const iframe = panel ? panel.querySelector("#sb-iframe") : $("sb-iframe");
    if (iframe) iframe.src = "about:blank";
    const cur = panel ? panel.querySelector("#sb-cursor") : $("sb-cursor");
    if (cur) cur.classList.add("hidden");
  }
}

function loadWebPreview(port) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const lbl = panel ? panel.querySelector("#sb-port-label") : $("sb-port-label");
  if (lbl) lbl.textContent = "→ http://127.0.0.1:" + port;
  const iframe = panel ? panel.querySelector("#sb-iframe") : $("sb-iframe");
  if (iframe) iframe.src = "http://127.0.0.1:" + port + "/";
  const out = panel ? panel.querySelector("#sb-output") : $("sb-output");
  if (out) { const div = el("div", "sb-line-port"); div.textContent = "[dotz] web server detected on port " + port; out.appendChild(div); out.scrollTop = out.scrollHeight; }
}

function renderCursor(x, y, action, text) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const cur = panel ? panel.querySelector("#sb-cursor") : $("sb-cursor");
  if (!cur) return;
  cur.classList.remove("hidden");
  cur.style.left = x + "px"; cur.style.top = y + "px";
  cur.classList.remove("click", "type");
  if (action === "click") cur.classList.add("click");
  if (action === "type") cur.classList.add("type");
  const lbl = panel ? panel.querySelector("#sb-cursor-label") : $("sb-cursor-label");
  if (lbl) lbl.textContent = action + (text ? ": " + truncate(text, 24) : "");
  dispatchCursorIntoIframe(x, y, action, text);
}

function dispatchCursorIntoIframe(x, y, action, text) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const iframe = panel ? panel.querySelector("#sb-iframe") : $("sb-iframe");
  let doc;
  try { doc = iframe.contentDocument || iframe.contentWindow.document; } catch { return; }
  if (!doc) return;
  if (action === "click") {
    const target = doc.elementFromPoint(x, y);
    if (target) { try { const ev = new doc.defaultView.MouseEvent("click", { bubbles: true, cancelable: true, clientX: x, clientY: y }); target.dispatchEvent(ev); if (typeof target.focus === "function") target.focus(); } catch {} }
  } else if (action === "type" && text) {
    const target = doc.elementFromPoint(x, y);
    if (target) {
      try {
        if (typeof target.focus === "function") target.focus();
        for (const ch of text) { const kd = new doc.defaultView.KeyboardEvent("keydown", { key: ch, bubbles: true }); target.dispatchEvent(kd); }
        if (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable) {
          if (target.isContentEditable) { target.textContent = (target.textContent || "") + text; }
          else { target.value = (target.value || "") + text; }
          const input = new doc.defaultView.InputEvent("input", { bubbles: true, data: text }); target.dispatchEvent(input);
        }
      } catch {}
    }
  }
}

function renderSandboxRuns() {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const box = panel ? panel.querySelector("#sb-runs") : $("sb-runs");
  if (!box) return;
  box.innerHTML = "";
  if (!state.sandbox.runs.size) return;
  [...state.sandbox.runs.values()].forEach((rec) => {
    const r = rec.run;
    const tab = el("div", "sb-run-tab " + (r.status === "running" ? "run" : r.status === "done" ? "done" : r.status === "error" ? "err" : "killed") + (r.id === state.sandbox.activeRunId ? " active" : ""));
    tab.appendChild(el("span", "sb-run-dot"));
    tab.appendChild(el("span", null, r.id.slice(0, 8) + " · " + r.language));
    tab.onclick = () => { state.sandbox.activeRunId = r.id; showRunOutput(rec); renderSandboxRuns(); };
    box.appendChild(tab);
  });
}

function showRunOutput(rec) {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const out = panel ? panel.querySelector("#sb-output") : $("sb-output");
  if (!out) return;
  out.innerHTML = "";
  (rec.output || "").split("\n").forEach((line) => { if (line) appendSandboxOutput(line, "stdout"); });
  const rid = panel ? panel.querySelector("#sb-runid") : $("sb-runid");
  if (rid) rid.textContent = "#" + rec.run.id.slice(0, 8);
  const st = panel ? panel.querySelector("#sb-status") : $("sb-status");
  if (st) st.textContent = rec.run.status + (rec.run.exitCode != null ? " (exit " + rec.run.exitCode + ")" : "");
  const kill = panel ? panel.querySelector("#sb-kill") : $("sb-kill");
  if (kill) kill.classList.toggle("hidden", rec.run.status !== "running");
  if (rec.port && state.sandbox.mode === "web") loadWebPreview(rec.port);
  else toggleWebPreview(state.sandbox.mode === "web");
}

function refreshSandboxCount() {
  const n = state.sandbox.runs.size;
  const st = $("st-sandbox");
  if (st) st.textContent = n + " run" + (n === 1 ? "" : "s");
}

async function loadSandboxLanguages() {
  try {
    const { languages } = await api("/api/sandbox/languages");
    state.sandbox.languages = languages || [];
    const panel = document.querySelector('.panel[data-panel="sandbox"]');
    const sel = panel ? panel.querySelector("#sb-lang") : $("sb-lang");
    if (sel) {
      sel.innerHTML = "";
      state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; sel.appendChild(o); });
    }
  } catch (e) { pushError("sandbox languages: " + e.message); }
}

async function runSandbox() {
  const panel = document.querySelector('.panel[data-panel="sandbox"]');
  const code = panel ? panel.querySelector("#sb-code").value : $("sb-code").value;
  if (!code.trim()) { pushError("sandbox: empty code"); return; }
  const language = panel ? panel.querySelector("#sb-lang").value : $("sb-lang").value;
  const mode = panel ? panel.querySelector("#sb-mode").value : $("sb-mode").value;
  state.sandbox.mode = mode;
  const st = panel ? panel.querySelector("#sb-status") : $("sb-status");
  if (st) st.textContent = "starting…";
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("sandbox: ws not connected"); return; }
  state.ws.send(JSON.stringify({ kind: "sandbox.start", language, code, mode, projectId: state.activeProjectId || null }));
  openPanel("sandbox");
}

async function killActiveRun() {
  const id = state.sandbox.activeRunId;
  if (!id) return;
  try {
    if (state.ws && state.ws.readyState === WebSocket.OPEN) state.ws.send(JSON.stringify({ kind: "sandbox.kill", runId: id }));
    await post("/api/sandbox/runs/" + id + "/kill");
  } catch (e) { pushError("sandbox kill: " + e.message); }
}

function focusSandboxRun(runId) {
  const rec = state.sandbox.runs.get(runId);
  if (!rec) return;
  state.sandbox.activeRunId = runId;
  showRunOutput(rec);
  renderSandboxRuns();
}

/* ---------- memory recall (handler; render lives in panels/memory.js) ---------- */
function handleMemoryRecall(m) {
  state.recalled = m.items || [];
  renderRecalled();
}

/* ---------- provider health + automatic failover ---------- */
// Update the conn-chip to reflect provider health. A degraded primary shows a "VIA BACKUP" badge
// so the operator knows failover is active without digging into a panel.
function renderProviderHealthChip() {
  const c = $("conn-chip");
  if (!c) return;
  const ph = state.providerHealth && state.providerHealth.providers;
  if (!ph) return;
  // Find the active provider's health (fall back to the configured provider).
  const active = state.activeProvider || (state.config && state.config.provider) || "ollama";
  const h = ph[active];
  if (h && h.status === "degraded") {
    const pair = (state.providerHealth.failoverPairs || {})[active];
    const backup = pair && pair.backup ? pair.backup.provider : "backup";
    c.classList.add("chip-warn");
    c.innerHTML = '<span class="blk">▲</span> ' + active.toUpperCase() + " → " + backup.toUpperCase();
    c.title = active + " degraded (auto-failover active): " + (h.last_error || "consecutive failures");
  } else if (h && h.status === "healthy") {
    // Healthy: clear any degraded styling but keep the normal WS conn state.
    c.classList.remove("chip-warn");
  }
}

// Handle a provider_health WS event: merge the single-provider update into state, re-render the
// chip, and surface a toast on the transition.
function handleProviderHealthEvent(m) {
  const ph = state.providerHealth.providers || (state.providerHealth.providers = {});
  const prev = ph[m.provider] && ph[m.provider].status;
  ph[m.provider] = Object.assign({}, ph[m.provider], { status: m.status });
  if (prev !== m.status) {
    const pair = (state.providerHealth.failoverPairs || {})[m.provider];
    const backup = pair && pair.backup ? pair.backup.provider : "backup";
    if (m.status === "degraded") {
      pushError("provider " + m.provider + " degraded — auto-failing over to " + backup);
    } else if (m.status === "healthy" && prev === "degraded") {
      // Recover: a success cleared the degraded flag.
      const c = $("conn-chip");
      if (c) { c.classList.remove("chip-warn"); }
      if (typeof pushInfo === "function") pushInfo("provider " + m.provider + " recovered");
    }
  }
  renderProviderHealthChip();
}

// Fetch the full health snapshot (REST) and merge into state. Called on load + on demand.
async function refreshProviderHealth() {
  try {
    const h = await api("/api/provider-health");
    if (h && h.providers) state.providerHealth = h;
    renderProviderHealthChip();
  } catch (e) { /* best-effort; chip just won't show health */ }
}

/* ---------- human gate ---------- */
function bindGateCard() {
  $("gate-approve").onclick = () => resolveGate(true);
  $("gate-reject").onclick = () => resolveGate(false);
}

function showGateCard(gateId, plan) {
  // Ignore a replayed event for the gate we're already showing (e.g. after a WS reconnect).
  if (state.pendingGate && state.pendingGate === gateId) return;
  state.gateQueue = state.gateQueue || [];
  // If a DIFFERENT gate is already up (concurrent sessions), queue this one instead of overwriting —
  // otherwise the first gate becomes unanswerable and its agent hangs until timeout.
  if (state.pendingGate && state.pendingGate !== gateId) {
    if (!state.gateQueue.some((g) => g.gateId === gateId)) state.gateQueue.push({ gateId, plan });
    return;
  }
  state.pendingGate = gateId;
  const card = $("gate-card");
  $("gate-plan").textContent = plan || "(no plan provided)";
  $("gate-feedback").value = "";
  card.classList.remove("hidden");
  $("gate-approve").focus();
}

function resolveGate(approved) {
  if (!state.pendingGate) return;
  const gateId = state.pendingGate;
  const feedback = $("gate-feedback").value.trim();
  // Always release the modal so the user is never trapped behind inert buttons (the gate is
  // intentionally non-dismissible via Escape, so a swallowed click would otherwise be a dead end).
  $("gate-card").classList.add("hidden");
  state.pendingGate = null;
  if (state.ws && state.ws.readyState === WebSocket.OPEN) {
    state.ws.send(JSON.stringify({ kind: approved ? "gate.approve" : "gate.reject", gateId, feedback }));
  } else {
    pushError("not connected — gate decision could not be sent (the agent will time out waiting for approval)");
  }
  // Surface the next queued gate, if any (concurrent-session gates aren't dropped).
  const next = (state.gateQueue || []).shift();
  if (next) showGateCard(next.gateId, next.plan);
}

/* ---------- WS connection chip ---------- */
function setConn(kind, txt) {
  const c = $("conn-chip");
  c.className = "chip chip-" + kind;
  const label = kind === "on" ? "PI ONLINE" : kind === "err" ? "WS ERROR" : "OFFLINE";
  c.innerHTML = '<span class="blk">█</span> ' + label;
  // Surface the transient detail (connecting…/reconnecting…/ws closed/✓ host) as a tooltip so the
  // three "off" sub-states aren't all flattened to a static OFFLINE label.
  c.title = txt || "";
}

/* ---------- status + stats ---------- */
async function refreshStats() {
  if (!state.sessionId) return;
  try {
    const s = await api(`/api/sessions/${state.sessionId}`);
    const st = s.stats || {};
    const tok = st.tokens || {};
    const tokenText = formatTokenBreakdown(tok);
    const tin = $("st-tok-in"); if (tin) tin.textContent = "↑" + tokenText.input;
    const tout = $("st-tok-out"); if (tout) tout.textContent = "↓" + tokenText.output;
    const cost = $("st-cost"); if (cost) cost.textContent = "$" + (st.cost || 0).toFixed(4).replace(/0+$/, "0");
    const cu = st.contextUsage;
    const pct = cu?.percent || 0;
    if (cu) {
      // contextUsage.percent is already a percentage (tokens/contextWindow*100), not a 0-1
      // fraction — multiplying by 100 again printed a nonsensical 800%+ context gauge.
      const p = $("st-ctx-pct"); if (p) p.textContent = pct.toFixed(1) + "%";
      const bar = $("st-ctx-bar"); if (bar) bar.style.width = Math.min(100, pct) + "%";
      const win = $("st-ctx-win"); if (win) win.textContent = "of " + fmtCtx(cu.contextWindow);
    }
    updateBrainStats(tokenText.combined, "$" + (st.cost || 0).toFixed(2), pct.toFixed(0) + "%");
  } catch {}
}

function updateBrainStats(tokens, cost, ctx) {
  const chat = document.querySelector('.panel[data-panel="brain"]');
  const set = (id, val) => {
    const el = chat ? chat.querySelector("#" + id) : $(id);
    if (el) el.textContent = val;
  };
  set("brain-tok", tokens);
  set("brain-cost", cost);
  set("brain-ctx", ctx);
  if ($("bf-tok")) $("bf-tok").textContent = tokens;
  if ($("bf-cost")) $("bf-cost").textContent = cost;
  if ($("bf-ctx")) $("bf-ctx").textContent = ctx;
}

function formatTokenBreakdown(tokens) {
  const input = fmtNum(tokens.input || 0);
  const output = fmtNum(tokens.output || 0);
  return { input, output, combined: `↑${input} ↓${output}` };
}

function fmtNum(n) { return n >= 1000 ? (n / 1000).toFixed(1) + "K" : String(n || 0); }
function fmtCtx(n) { return n ? (n >= 1000 ? Math.round(n / 1000) + "K" : "" + n) : "—"; }

export {
  handleEvent,
  handleWorkflowEvent,
  handleSandboxEvent,
  handleMemoryRecall,
  handleProviderHealthEvent,
  refreshProviderHealth,
  renderProviderHealthChip,
  bindGateCard,
  showGateCard,
  resolveGate,
  setConn,
  stringifyResult,
  appendSandboxOutput,
  toggleWebPreview,
  loadWebPreview,
  renderCursor,
  dispatchCursorIntoIframe,
  renderSandboxRuns,
  showRunOutput,
  refreshSandboxCount,
  loadSandboxLanguages,
  runSandbox,
  killActiveRun,
  focusSandboxRun,
  refreshWfCount,
  refreshStats,
  updateBrainStats,
  formatTokenBreakdown,
  fmtNum,
  fmtCtx,
};