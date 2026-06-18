/* dotz ultra code mode — bento dashboard UI.
 * Wires the cyberbrutalist front end to the dotz backend (REST + WebSocket).
 * Handles: project launcher, bento panels (chat/graph/brain/browser/memory/files/sandbox/skills),
 * drag-and-drop panel repositioning, on-the-fly SVG workflow graph, multi-provider models,
 * sandbox (terminal + web preview + agent cursor), chat streaming, tool cards, skills pool.
 * Vanilla JS, no framework, no build step. Runs identically in browser and Electron.
 */
"use strict";

const THINK_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh"];
const LAYOUT_KEY = "dotz.layout.v1";
const PANEL_NAMES = ["chat", "graph", "brain", "browser", "memory", "files", "sandbox", "skills"];
const PANEL_META = {
  chat: { icon: "▓", label: "CHAT" },
  graph: { icon: "◐", label: "WORKFLOW GRAPH" },
  brain: { icon: "◆", label: "AGENT BRAIN" },
  browser: { icon: "▣", label: "BROWSER" },
  memory: { icon: "▤", label: "MEMORY" },
  files: { icon: "▥", label: "FILES" },
  sandbox: { icon: "▩", label: "SANDBOX" },
  skills: { icon: "✦", label: "SKILLS" },
};

const state = {
  sessionId: null,
  summary: null,
  ws: null,
  streaming: false,
  cur: null,        // { bubble, partial }
  toolCards: {},    // toolCallId -> card refs
  profiles: [],
  activeProfileId: "workflow",
  projects: [],
  activeProjectId: null,
  providers: [],
  activeProvider: "openrouter",
  modelCatalog: [],
  memory: [],
  skills: [],
  // sandbox
  sandbox: { languages: [], runs: new Map(), activeRunId: null, mode: "terminal" },
  // workflows
  workflows: new Map(),  // runId -> WorkflowRun
  activeWfId: null,
  // bento layout
  layout: { open: ["chat"], version: 1 },
  // graph viewbox pan/zoom
  wfView: { x: 0, y: 0, w: 800, h: 600 },
};

const $ = (id) => document.getElementById(id);
const el = (tag, cls, txt) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (txt != null) e.textContent = txt;
  return e;
};
async function api(path, opts) {
  const r = await fetch(path, opts);
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || r.statusText);
  return body;
}
const post = (p, b) => api(p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(b || {}) });
const patch = (p, b) => api(p, { method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify(b || {}) });
const del = (p) => api(p, { method: "DELETE" });
const esc = (s) => String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const truncate = (s, n) => (s && s.length > n ? s.slice(0, n - 1) + "…" : s || "");

// ---------- bootstrap ----------
init().catch((e) => pushError("init failed: " + e.message));

async function init() {
  bindTopbar();
  bindLauncher();
  bindComposer();
  bindSandbox();
  bindMemory();
  bindPalette();
  bindKeyboard();
  loadLayout();
  await loadProfiles();
  await loadProviders();
  await loadProjects();
  await loadSandboxLanguages();
  // start with launcher visible (no session yet)
  showLauncher();
}

// ---------- layout persistence ----------
function loadLayout() {
  try {
    const raw = localStorage.getItem(LAYOUT_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed.version === 1 && Array.isArray(parsed.open)) state.layout = parsed;
    }
  } catch {}
}
function saveLayout() {
  try { localStorage.setItem(LAYOUT_KEY, JSON.stringify(state.layout)); } catch {}
}

// ---------- launcher (initial state) ----------
function showLauncher() {
  $("launcher").classList.remove("hidden");
  $("bento").classList.add("hidden");
  $("session-name").textContent = "no session";
  renderLauncherProjects();
}

function hideLauncher() {
  $("launcher").classList.add("hidden");
  $("bento").classList.remove("hidden");
}

function bindLauncher() {
  $("launcher-new-btn").onclick = () => {
    const f = $("launcher-form");
    f.classList.toggle("hidden");
    if (!f.classList.contains("hidden")) {
      $("lf-name").value = "";
      $("lf-cwd").value = "";
      $("lf-model").value = "nex-agi/nex-2-pro:free";
      const sel = $("lf-profile");
      sel.innerHTML = "";
      state.profiles.forEach((p) => { const o = el("option", null, p.name); o.value = p.id; sel.appendChild(o); });
      sel.value = state.activeProfileId;
      $("lf-name").focus();
    }
  };
  $("lf-cancel").onclick = () => $("launcher-form").classList.add("hidden");
  $("lf-create").onclick = async () => {
    const name = $("lf-name").value.trim();
    const cwd = $("lf-cwd").value.trim();
    if (!name || !cwd) { pushError("project needs name + cwd"); return; }
    const body = {
      name, cwd,
      profileId: $("lf-profile").value,
      model: { provider: "openrouter", modelId: $("lf-model").value.trim() || "nex-agi/nex-2-pro:free" },
    };
    try {
      const p = await post("/api/projects", body);
      $("launcher-form").classList.add("hidden");
      await loadProjects();
      await openProject(p.id);
    } catch (e) { pushError("create project: " + e.message); }
  };
}

function renderLauncherProjects() {
  const box = $("launcher-projects");
  box.innerHTML = "";
  if (!state.projects.length) {
    box.appendChild(el("div", "dim mono", "no projects yet — create one below"));
    return;
  }
  state.projects.forEach((p) => {
    const card = el("div", "launcher-project");
    card.appendChild(el("span", "lp-glyph", "◆"));
    const info = el("div", "lp-info");
    info.appendChild(el("div", "lp-name", p.name));
    info.appendChild(el("div", "lp-cwd", p.cwd));
    info.appendChild(el("div", "lp-profile", (p.profileId || "workflow").toUpperCase()));
    card.appendChild(info);
    card.onclick = () => openProject(p.id);
    box.appendChild(card);
  });
}

async function openProject(id) {
  state.activeProjectId = id;
  hideLauncher();
  renderBento();
  await newSession();
}

// ---------- bento panels ----------
function renderBento() {
  const bento = $("bento");
  bento.innerHTML = "";
  for (const name of state.layout.open) mountPanel(name);
}

function mountPanel(name) {
  if (!PANEL_NAMES.includes(name)) return;
  const tpl = $("tpl-" + name);
  if (!tpl) return;
  const node = tpl.content.firstElementChild.cloneNode(true);
  $("bento").appendChild(node);
  bindPanel(node, name);
  // wire up panel-specific handlers
  if (name === "chat") wireChatPanel(node);
  else if (name === "graph") wireGraphPanel(node);
  else if (name === "brain") wireBrainPanel(node);
  else if (name === "memory") wireMemoryPanel(node);
  else if (name === "sandbox") wireSandboxPanel(node);
  else if (name === "skills") wireSkillsPanel(node);
}

function unmountPanel(name) {
  const node = document.querySelector(`.panel[data-panel="${name}"]`);
  if (node) node.remove();
  state.layout.open = state.layout.open.filter((n) => n !== name);
  saveLayout();
}

function bindPanel(node, name) {
  const head = node.querySelector(".panel-head");
  const closeBtn = node.querySelector(".panel-close");
  if (closeBtn) closeBtn.onclick = (e) => { e.stopPropagation(); if (name !== "chat") unmountPanel(name); };
  // drag-and-drop
  head.addEventListener("dragstart", (e) => {
    node.classList.add("dragging");
    e.dataTransfer.setData("text/plain", name);
    e.dataTransfer.effectAllowed = "move";
  });
  head.addEventListener("dragend", () => node.classList.remove("dragging"));
  head.addEventListener("dragover", (e) => { e.preventDefault(); head.classList.add("drag-over"); });
  head.addEventListener("dragleave", () => head.classList.remove("drag-over"));
  head.addEventListener("drop", (e) => {
    e.preventDefault();
    head.classList.remove("drag-over");
    const from = e.dataTransfer.getData("text/plain");
    if (from && from !== name) swapPanels(from, name);
  });
}

function swapPanels(from, to) {
  // swap grid spans by swapping data-panel attributes + re-mounting content
  const fromNode = document.querySelector(`.panel[data-panel="${from}"]`);
  const toNode = document.querySelector(`.panel[data-panel="${to}"]`);
  if (!fromNode || !toNode) return;
  // swap the data-panel attrs (CSS grid spans are keyed off data-panel)
  fromNode.dataset.panel = to;
  toNode.dataset.panel = from;
  // swap the inner template content by re-cloning
  const fromTpl = $("tpl-" + from);
  const toTpl = $("tpl-" + to);
  if (fromTpl && toTpl) {
    const newFrom = toTpl.content.firstElementChild.cloneNode(true);
    const newTo = fromTpl.content.firstElementChild.cloneNode(true);
    newFrom.dataset.panel = to;
    newTo.dataset.panel = from;
    fromNode.replaceWith(newFrom);
    toNode.replaceWith(newTo);
    bindPanel(newFrom, to); bindPanel(newTo, from);
    if (to === "chat") wireChatPanel(newFrom);
    else if (to === "graph") wireGraphPanel(newFrom);
    else if (to === "brain") wireBrainPanel(newFrom);
    else if (to === "memory") wireMemoryPanel(newFrom);
    else if (to === "sandbox") wireSandboxPanel(newFrom);
    else if (to === "skills") wireSkillsPanel(newFrom);
    if (from === "chat") wireChatPanel(newTo);
    else if (from === "graph") wireGraphPanel(newTo);
    else if (from === "brain") wireBrainPanel(newTo);
    else if (from === "memory") wireMemoryPanel(newTo);
    else if (from === "sandbox") wireSandboxPanel(newTo);
    else if (from === "skills") wireSkillsPanel(newTo);
  }
  saveLayout();
}

function openPanel(name) {
  if (state.layout.open.includes(name)) return;
  state.layout.open.push(name);
  saveLayout();
  mountPanel(name);
}

// ---------- panel palette ----------
function bindPalette() {
  $("panels-btn").onclick = togglePalette;
  $("palette-close").onclick = () => $("palette").classList.add("hidden");
  $("palette").onclick = (e) => { if (e.target.id === "palette") $("palette").classList.add("hidden"); };
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

function bindKeyboard() {
  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey && e.key.toLowerCase() === "p") { e.preventDefault(); togglePalette(); }
    if (e.key === "Escape") $("palette").classList.add("hidden");
  });
}

// ---------- topbar ----------
function bindTopbar() {
  $("workflows-btn").onclick = () => { openPanel("graph"); refreshWorkflowGraph(); };
}

// ---------- profiles ----------
async function loadProfiles() {
  try {
    const { profiles, default: def } = await api("/api/profiles");
    state.profiles = profiles;
    state.activeProfileId = def || "workflow";
    renderProfilePicker();
  } catch (e) { pushError("profiles: " + e.message); }
}
function renderProfilePicker() {
  // kept for verify-ui compatibility — hidden, but populates the profile picker element
  const box = $("profile-picker");
  box.innerHTML = "";
  state.profiles.forEach((p) => {
    const b = el("button", "prof-btn" + (p.id === state.activeProfileId ? " active" : ""), p.name);
    b.dataset.id = p.id;
    box.appendChild(b);
  });
}

// ---------- providers ----------
async function loadProviders() {
  try { const { providers } = await api("/api/providers"); state.providers = providers || []; }
  catch { state.providers = []; }
}

// ---------- session lifecycle ----------
async function newSession() {
  const body = { profileId: state.activeProfileId };
  if (state.activeProjectId) body.projectId = state.activeProjectId;
  const s = await post("/api/sessions", { profileId: state.activeProfileId, projectId: state.activeProjectId || undefined });
  setSession(s);
  clearTranscript();
  connectWS();
  await Promise.all([loadModels(), loadCommands(), refreshMemory(), refreshSkills(), refreshWorkflows()]);
  await refreshStats();
  refreshSandboxCount();
  refreshWfCount();
}

function setSession(s) {
  state.summary = s;
  state.sessionId = s.sessionId;
  if (s.profileId) state.activeProfileId = s.profileId;
  if (s.projectId) state.activeProjectId = s.projectId;
  $("session-name").textContent = "session " + s.sessionId.slice(0, 8);
  updateModelUI(s.model);
  renderReasoning(s);
}

// ---------- websocket ----------
function connectWS() {
  if (state.ws) { try { state.ws.close(); } catch {} }
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws?sessionId=${state.sessionId}`);
  state.ws = ws;
  setConn("off", "ws connecting…");
  ws.onopen = () => setConn("on", "ws ✓ " + location.host);
  ws.onclose = () => setConn("off", "ws closed");
  ws.onerror = () => setConn("err", "ws error");
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.kind === "event") handleEvent(m.event);
    else if (m.kind === "sandbox") handleSandboxEvent(m.event);
    else if (m.kind === "workflow") handleWorkflowEvent(m.runId, m.event);
    else if (m.kind === "error") pushError(m.error);
  };
}

function setConn(kind, txt) {
  const c = $("conn-chip");
  c.className = "chip chip-" + kind;
  const label = kind === "on" ? "PI ONLINE" : kind === "err" ? "WS ERROR" : "OFFLINE";
  c.innerHTML = '<span class="blk">█</span> ' + label;
}

// ---------- agent event handling ----------
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
    case "agent_end": setStreaming(false); refreshStats(); logBrain("agent_end"); break;
  }
  scrollBottom();
}

function stringifyResult(r) {
  if (r == null) return "";
  if (typeof r === "string") return r;
  if (r.content && Array.isArray(r.content)) return r.content.map((c) => c.text || "").join("\n");
  return JSON.stringify(r);
}

// ---------- chat panel ----------
function wireChatPanel(node) {
  const input = node.querySelector("#composer-input");
  const sendBtn = node.querySelector("#send-btn");
  const stopBtn = node.querySelector("#stop-btn");
  input.addEventListener("input", autoGrow);
  input.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } });
  sendBtn.onclick = send;
  stopBtn.onclick = () => state.ws && state.ws.send(JSON.stringify({ kind: "abort" }));
  // quick chips
  const chips = node.querySelector("#quick-chips");
  chips.innerHTML = "";
  ["implement", "scout-and-plan", "implement-and-review"].forEach((cmd) => {
    const chip = el("span", "qchip", "/" + cmd);
    chip.onclick = () => insertCommand(cmd);
    chips.appendChild(chip);
  });
}
function autoGrow() {
  const input = $("composer-input");
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 200) + "px";
}
function send() {
  const input = $("composer-input");
  const text = input.value.trim();
  if (!text || !state.ws || state.ws.readyState !== WebSocket.OPEN) return;
  renderUserMessage({ content: [{ type: "text", text }] });
  state.ws.send(JSON.stringify({ kind: "prompt", text }));
  input.value = "";
  autoGrow();
}
function setStreaming(b) {
  state.streaming = b;
  const sendBtn = $("send-btn"); const stopBtn = $("stop-btn");
  if (sendBtn) sendBtn.classList.toggle("hidden", b);
  if (stopBtn) stopBtn.classList.toggle("hidden", !b);
  $("st-state").textContent = b ? "streaming…" : "idle";
}
function insertCommand(name) {
  const input = $("composer-input");
  input.value = ("/" + name + " ").replace("//", "/");
  input.focus();
  autoGrow();
}

// ---------- transcript rendering ----------
function clearTranscript() { const t = $("transcript"); if (t) { t.innerHTML = ""; state.cur = null; state.toolCards = {}; } }
function scrollBottom() { const t = $("transcript"); if (t) t.scrollTop = t.scrollHeight; }
function renderUserMessage(message) {
  const text = (message.content || []).map((c) => c.text || "").join("");
  const m = el("div", "msg user");
  m.appendChild(el("div", "msg-role", "you"));
  m.appendChild(el("div", "bubble", text));
  const t = $("transcript"); if (t) { t.appendChild(m); scrollBottom(); }
}
function ensureAssistantBubble() {
  if (state.cur && state.cur.bubble.isConnected) return state.cur;
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "dotz"));
  const bubble = el("div", "bubble");
  m.appendChild(bubble);
  const t = $("transcript"); if (!t) return state.cur; t.appendChild(m);
  state.cur = { bubble, partial: { content: [] } };
  return state.cur;
}
function hasRenderable(partial) {
  return (partial.content || []).some((b) => (b.type === "text" && b.text) || (b.type === "thinking" && b.thinking && b.thinking.trim()));
}
function renderAssistantPartial(partial) {
  if (!hasRenderable(partial)) return;
  const cur = ensureAssistantBubble();
  cur.partial = partial;
  renderAssistant(cur, true);
}
function renderAssistant(cur, streaming) {
  cur.bubble.innerHTML = "";
  (cur.partial.content || []).forEach((block) => {
    if (block.type === "thinking") {
      if (!block.thinking || !block.thinking.trim()) return;
      const d = el("details", "thinking");
      if (streaming) d.open = true;
      d.appendChild(el("summary", null, "▶ THINKING"));
      d.appendChild(el("div", "think-body", block.thinking));
      cur.bubble.appendChild(d);
    } else if (block.type === "text") {
      const t = el("div", "assistant-text");
      t.textContent = block.text || "";
      if (streaming) t.appendChild(el("span", "cursor", "█"));
      cur.bubble.appendChild(t);
    }
  });
}
function finalizeAssistant(message) {
  if (hasRenderable(message) || (message.stopReason === "error" && message.errorMessage)) {
    const cur = ensureAssistantBubble();
    cur.partial = message;
    renderAssistant(cur, false);
    if (message.stopReason === "error" && message.errorMessage) cur.bubble.appendChild(el("div", "msg-error", "⚠ " + message.errorMessage));
  }
  state.cur = null;
}
function toolCard(id, patch) {
  let tc = state.toolCards[id];
  if (!tc) {
    const card = el("div", "toolcard");
    card.dataset.tc = id;
    const head = el("div", "toolcard-head");
    const nameEl = el("span", "toolcard-name", "⚙ tool");
    const badge = el("span", "toolcard-status status-run", "● RUN");
    head.appendChild(nameEl); head.appendChild(badge);
    const body = el("div", "toolcard-body");
    const argsEl = el("div", "toolcard-args");
    const outEl = el("div", "toolcard-out");
    outEl.style.display = "none";
    body.appendChild(argsEl); body.appendChild(outEl);
    card.appendChild(head); card.appendChild(body);
    const t = $("transcript"); if (!t) return; t.appendChild(card);
    tc = state.toolCards[id] = { nameEl, badge, argsEl, outEl, data: {} };
  }
  Object.assign(tc.data, patch);
  const d = tc.data;
  if (d.name) tc.nameEl.textContent = "⚙ " + d.name;
  if (d.args !== undefined) tc.argsEl.textContent = typeof d.args === "string" ? d.args : JSON.stringify(d.args, null, 2);
  if (d.output) { tc.outEl.style.display = ""; tc.outEl.textContent = d.output; }
  const st = d.status || "run";
  tc.badge.className = "toolcard-status " + (st === "done" ? "status-done" : st === "err" ? "status-err" : "status-run");
  tc.badge.textContent = st === "done" ? "✓ DONE" : st === "err" ? "✕ ERROR" : "● RUN";
}
function pushError(msg) {
  const t = $("transcript"); if (!t) { console.error(msg); return; }
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "system"));
  const b = el("div", "bubble");
  b.appendChild(el("div", "msg-error", "⚠ " + msg));
  m.appendChild(b);
  t.appendChild(m);
  scrollBottom();
}

// ---------- model + provider controls ----------
async function loadModels() {
  const m = await api(`/api/sessions/${state.sessionId}/models`);
  state.modelCatalog = m.available || [];
  const provSel = $("provider-select");
  provSel.innerHTML = "";
  (m.providerMeta || state.providers || []).forEach((p) => {
    const o = el("option", null, p.label || p.id);
    o.value = p.id;
    if (p.id === state.activeProvider) o.selected = true;
    provSel.appendChild(o);
  });
  provSel.onchange = () => { state.activeProvider = provSel.value; renderModelInput(); populateModelSuggestions(); };
  updateModelUI(m.current || state.summary.model);
  state.activeProvider = (m.current && m.current.provider) || "openrouter";
  provSel.value = state.activeProvider;
  renderModelInput();
  populateModelSuggestions();
}
function providerIsFreeForm(id) {
  const meta = (state.providers || []).find((p) => p.id === id);
  return !!(meta && meta.freeForm);
}
function renderModelInput() {
  const free = providerIsFreeForm(state.activeProvider);
  $("model-input").classList.toggle("hidden", !free);
  $("model-select").classList.toggle("hidden", free);
  if (!free) {
    const sel = $("model-select");
    sel.innerHTML = "";
    state.modelCatalog.filter((x) => x.provider === state.activeProvider).forEach((x) => {
      const o = el("option", null, (x.name || x.modelId) + "  ·  " + x.modelId);
      o.value = x.modelId; sel.appendChild(o);
    });
    sel.onchange = submitModelSelect;
  } else {
    const input = $("model-input");
    input.onchange = submitModel;
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); submitModel(); } });
  }
}
function populateModelSuggestions() {
  const dl = $("model-suggestions");
  dl.innerHTML = "";
  state.modelCatalog.filter((x) => x.provider === state.activeProvider).slice(0, 400).forEach((x) => { const o = el("option"); o.value = x.modelId; dl.appendChild(o); });
}
function updateModelUI(model) {
  const id = model ? model.modelId : "nex-agi/nex-2-pro:free";
  const prov = model ? model.provider : "openrouter";
  const free = providerIsFreeForm(prov);
  if (free) $("model-input").value = id;
  else $("model-select").value = id;
}
function fmtCtx(n) { return n ? (n >= 1000 ? Math.round(n / 1000) + "K" : "" + n) : "—"; }
async function submitModel() { const id = $("model-input").value.trim(); if (id) await setModel({ provider: state.activeProvider, modelId: id }); }
async function submitModelSelect() { const id = $("model-select").value; if (id) await setModel({ provider: state.activeProvider, modelId: id }); }
async function setModel(ref) {
  try { const s = await post(`/api/sessions/${state.sessionId}/model`, ref); state.summary = s; updateModelUI(s.model); renderReasoning(s); }
  catch (e) { pushError("model: " + e.message); }
}

// ---------- reasoning ----------
function renderReasoning(summary) {
  const seg = $("reasoning-seg");
  seg.innerHTML = "";
  const avail = summary.availableThinkingLevels || [];
  THINK_LEVELS.forEach((lvl) => {
    const b = el("button", null, lvl.toUpperCase().slice(0, 3));
    b.title = lvl;
    b.disabled = !summary.supportsThinking || !avail.includes(lvl);
    if (lvl === summary.thinkingLevel) b.classList.add("active");
    b.onclick = async () => {
      try { const r = await post(`/api/sessions/${state.sessionId}/thinking`, { level: lvl }); state.summary = Object.assign({}, state.summary, r); renderReasoning(state.summary); }
      catch (e) { pushError("thinking: " + e.message); }
    };
    seg.appendChild(b);
  });
}

// ---------- commands ----------
async function loadCommands() {
  // commands are surfaced as quick chips in the chat panel; skills get their own panel
  try {
    const { commands } = await api(`/api/sessions/${state.sessionId}/commands`);
    const chips = $("quick-chips");
    if (!chips) return;
    chips.innerHTML = "";
    commands.slice(0, 6).forEach((c) => {
      const chip = el("span", "qchip", "/" + c.name);
      chip.title = c.description || "";
      chip.onclick = () => insertCommand(c.name);
      chips.appendChild(chip);
    });
  } catch {}
}

// ---------- projects ----------
async function loadProjects() {
  try { const { projects } = await api("/api/projects"); state.projects = projects || []; }
  catch (e) { pushError("projects: " + e.message); }
}

// ---------- memory panel ----------
function wireMemoryPanel(node) {
  const addBtn = node.querySelector("#mem-add");
  const form = node.querySelector("#mem-form");
  addBtn.onclick = () => { form.classList.toggle("hidden"); if (!form.classList.contains("hidden")) { node.querySelector("#mem-key").value = ""; node.querySelector("#mem-value").value = ""; node.querySelector("#mem-key").focus(); } };
  node.querySelector("#mem-cancel").onclick = () => form.classList.add("hidden");
  node.querySelector("#mem-create").onclick = async () => {
    const key = node.querySelector("#mem-key").value.trim();
    const value = node.querySelector("#mem-value").value;
    if (!key) { pushError("memory key required"); return; }
    try {
      await post("/api/memory", { projectId: state.activeProjectId || "", key, value, scope: node.querySelector("#mem-scope").value });
      form.classList.add("hidden");
      await refreshMemory();
    } catch (e) { pushError("memory add: " + e.message); }
  };
  refreshMemory();
}

function bindMemory() { /* memory panel binds itself via wireMemoryPanel */ }

async function refreshMemory() {
  try {
    const { entries } = await api("/api/memory" + (state.activeProjectId ? "?projectId=" + encodeURIComponent(state.activeProjectId) : ""));
    state.memory = entries || [];
    renderMemory();
  } catch {}
}

function renderMemory() {
  const box = $("memory-list");
  if (!box) return;
  box.innerHTML = "";
  if (!state.memory.length) { box.appendChild(el("span", "dim mono", "no entries")); return; }
  state.memory.forEach((m) => {
    const entry = el("div", "mem-entry");
    const head = el("div", "mem-entry-head");
    head.appendChild(el("span", "mem-key", m.key));
    head.appendChild(el("span", "mem-scope" + (m.scope === "global" ? " global" : ""), m.scope));
    entry.appendChild(head);
    entry.appendChild(el("div", "mem-value", truncate(m.value, 80)));
    const actions = el("div", "mem-actions");
    const editBtn = el("button", null, "EDIT");
    editBtn.onclick = () => inlineEditMemory(entry, m);
    const delBtn = el("button", "del", "DEL");
    delBtn.onclick = async (ev) => { ev.stopPropagation(); try { await del("/api/memory/" + m.id); await refreshMemory(); } catch (e) { pushError("memory delete: " + e.message); } };
    actions.appendChild(editBtn); actions.appendChild(delBtn);
    entry.appendChild(actions);
    box.appendChild(entry);
  });
}

function inlineEditMemory(entry, m) {
  entry.innerHTML = "";
  const row = el("div", "mem-edit-row");
  const keyIn = el("input"); keyIn.value = m.key; keyIn.placeholder = "key";
  const valIn = el("input"); valIn.value = m.value; valIn.placeholder = "value";
  const scopeSel = el("select", "pf-select");
  ["project", "global"].forEach((s) => { const o = el("option", null, s); o.value = s; if (s === m.scope) o.selected = true; scopeSel.appendChild(o); });
  const actions = el("div", "pf-actions");
  const save = el("button", "btn-mini btn-go", "SAVE");
  const cancel = el("button", "btn-mini", "CANCEL");
  save.onclick = async () => { try { await patch("/api/memory/" + m.id, { key: keyIn.value.trim(), value: valIn.value, scope: scopeSel.value }); await refreshMemory(); } catch (e) { pushError("memory edit: " + e.message); } };
  cancel.onclick = () => refreshMemory();
  actions.appendChild(save); actions.appendChild(cancel);
  row.appendChild(keyIn); row.appendChild(valIn); row.appendChild(scopeSel); row.appendChild(actions);
  entry.appendChild(row);
}

// ---------- skills panel ----------
function wireSkillsPanel(node) {
  const search = node.querySelector("#skills-search");
  search.addEventListener("input", () => renderSkillsList(search.value.toLowerCase()));
  refreshSkills();
}

async function refreshSkills() {
  try {
    const { skills } = await api("/api/skills");
    state.skills = skills || [];
    renderSkillsList("");
  } catch (e) { pushError("skills load: " + e.message); }
}

function renderSkillsList(filter) {
  const list = $("skills-list");
  const count = $("skills-count");
  if (!list || !count) return;
  const filtered = filter ? state.skills.filter((s) => s.name.toLowerCase().includes(filter) || (s.description || "").toLowerCase().includes(filter)) : state.skills;
  count.textContent = `${filtered.length} of ${state.skills.length} skills`;
  list.innerHTML = "";
  filtered.slice(0, 200).forEach((s) => {
    const item = el("div", "skill-item");
    const head = el("div", "skill-item-head");
    head.appendChild(el("span", "skill-name", s.name + (s.isUmbrella ? " ◫" : "")));
    head.appendChild(el("span", "skill-source " + s.source, s.source));
    item.appendChild(head);
    item.appendChild(el("div", "skill-desc", truncate(s.description, 100)));
    item.onclick = () => showSkillDetail(s);
    list.appendChild(item);
  });
}

async function showSkillDetail(skill) {
  const detail = $("skills-detail");
  if (!detail) return;
  detail.classList.remove("hidden");
  detail.innerHTML = "";
  const head = el("div", "skills-detail-head");
  head.appendChild(el("span", "skill-name", skill.name));
  head.appendChild(el("span", "skill-source " + skill.source, skill.source));
  const close = el("button", "skills-detail-close", "×");
  close.onclick = () => detail.classList.add("hidden");
  head.appendChild(close);
  detail.appendChild(head);
  const body = el("div", "skills-detail-body", "loading…");
  detail.appendChild(body);
  try {
    const { body: full } = await api(`/api/skills/${encodeURIComponent(skill.name)}`);
    body.textContent = full;
  } catch (e) { body.textContent = "error: " + e.message; }
}

// ---------- workflows ----------
async function refreshWorkflows() {
  try {
    const { runs } = await api("/api/workflows/active");
    runs.forEach((r) => state.workflows.set(r.id, r));
    refreshWfCount();
    refreshWorkflowGraph();
  } catch {}
}

function refreshWfCount() {
  const n = state.workflows.size;
  const badge = $("wf-badge");
  badge.textContent = String(n);
  badge.classList.toggle("hidden", n === 0);
  const st = $("st-workflows");
  if (st) st.textContent = String(n);
}

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
      }
      refreshWorkflowGraph();
      break;
    }
    case "step_added": {
      const run = state.workflows.get(runId);
      if (run) { run.steps.push(event.step); refreshWorkflowGraph(); }
      break;
    }
  }
}

function wireGraphPanel(node) {
  const svg = node.querySelector("#wf-svg");
  // pan/zoom
  let dragging = false; let startX, startY, startView;
  svg.addEventListener("mousedown", (e) => {
    if (e.target.closest(".wf-node")) return;
    dragging = true; startX = e.clientX; startY = e.clientY; startView = { ...state.wfView };
  });
  svg.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    state.wfView.x = startView.x - (e.clientX - startX);
    state.wfView.y = startView.y - (e.clientY - startY);
    applyViewBox();
  });
  svg.addEventListener("mouseup", () => { dragging = false; });
  svg.addEventListener("mouseleave", () => { dragging = false; });
  svg.addEventListener("wheel", (e) => {
    e.preventDefault();
    const scale = e.deltaY > 0 ? 1.1 : 0.9;
    state.wfView.w *= scale; state.wfView.h *= scale;
    applyViewBox();
  });
  refreshWorkflowGraph();
}

function applyViewBox() {
  const svg = $("wf-svg");
  if (svg) svg.setAttribute("viewBox", `${state.wfView.x} ${state.wfView.y} ${state.wfView.w} ${state.wfView.h}`);
}

function refreshWorkflowGraph() {
  const tabsEl = $("wf-tabs");
  const emptyEl = $("wf-empty");
  const nodesG = $("wf-nodes");
  const edgesG = $("wf-edges");
  if (!tabsEl || !nodesG || !edgesG) return;
  tabsEl.innerHTML = "";
  nodesG.innerHTML = "";
  edgesG.innerHTML = "";
  if (state.workflows.size === 0) {
    emptyEl.classList.remove("hidden");
    return;
  }
  emptyEl.classList.add("hidden");
  // tabs
  [...state.workflows.values()].forEach((run) => {
    const tab = el("div", "wf-tab " + run.status + (run.id === state.activeWfId ? " active" : ""));
    tab.appendChild(el("span", "wf-tab-dot"));
    tab.appendChild(el("span", null, truncate(run.label, 24)));
    tab.onclick = () => { state.activeWfId = run.id; refreshWorkflowGraph(); };
    tabsEl.appendChild(tab);
  });
  const run = state.workflows.get(state.activeWfId) || [...state.workflows.values()][0];
  if (!run) return;
  renderWorkflowDag(run);
}

function renderWorkflowDag(run) {
  const nodesG = $("wf-nodes");
  const edgesG = $("wf-edges");
  // layered layout (topological layers)
  const layers = computeLayers(run.steps);
  const positions = {};
  const NODE_W = 140, NODE_H = 48, LAYER_GAP = 180, NODE_GAP = 20;
  layers.forEach((layer, i) => {
    const layerWidth = layer.length * (NODE_W + NODE_GAP);
    const startX = (state.wfView.w - layerWidth) / 2;
    layer.forEach((stepId, j) => {
      positions[stepId] = { x: startX + j * (NODE_W + NODE_GAP), y: 40 + i * LAYER_GAP };
    });
  });
  // edges
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
  // nodes
  run.steps.forEach((step) => {
    const pos = positions[step.id];
    if (!pos) return;
    const g = document.createElementNS("http://www.w3.org/2000/svg", "g");
    g.setAttribute("class", "wf-node " + step.status);
    g.setAttribute("transform", `translate(${pos.x}, ${pos.y})`);
    g.dataset.stepId = step.id;
    const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    rect.setAttribute("width", NODE_W); rect.setAttribute("height", NODE_H); rect.setAttribute("rx", 4);
    g.appendChild(rect);
    const dot = document.createElementNS("http://www.w3.org/2000/svg", "circle");
    dot.setAttribute("cx", 10); dot.setAttribute("cy", 10); dot.setAttribute("r", 4);
    dot.setAttribute("class", "wf-node-dot");
    g.appendChild(dot);
    const label = document.createElementNS("http://www.w3.org/2000/svg", "text");
    label.setAttribute("x", 20); label.setAttribute("y", 14); label.setAttribute("class", "wf-node-label");
    label.textContent = truncate(step.agent, 16);
    g.appendChild(label);
    const task = document.createElementNS("http://www.w3.org/2000/svg", "text");
    task.setAttribute("x", 8); task.setAttribute("y", 32); task.setAttribute("class", "wf-node-task");
    task.textContent = truncate(step.task, 22);
    g.appendChild(task);
    g.onclick = () => showStepDetail(run, step);
    nodesG.appendChild(g);
  });
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
    if (layer.length === 0) { // cycle or missing parent — place remaining in one layer
      remaining.forEach((s) => { layer.push(s.id); placed.add(s.id); });
      remaining.length = 0;
    }
    layers.push(layer);
  }
  return layers;
}

function showStepDetail(run, step) {
  const detail = $("wf-detail");
  if (!detail) return;
  detail.classList.remove("hidden");
  detail.innerHTML = "";
  const head = el("div", "wf-detail-head");
  head.appendChild(el("span", "skill-name", step.agent));
  head.appendChild(el("span", "skill-source " + step.status, step.status));
  const close = el("button", "wf-detail-close", "×");
  close.onclick = () => detail.classList.add("hidden");
  head.appendChild(close);
  detail.appendChild(head);
  detail.appendChild(makeRow("TASK", step.task));
  detail.appendChild(makeRow("STATUS", step.status));
  if (step.output) {
    detail.appendChild(el("div", "wf-detail-row", null));
    const out = el("div", "wf-detail-output", step.output);
    detail.appendChild(out);
  }
  if (step.error) detail.appendChild(makeRow("ERROR", step.error));
  if (step.usage) detail.appendChild(makeRow("USAGE", JSON.stringify(step.usage)));
  if (step.startedAt) detail.appendChild(makeRow("STARTED", new Date(step.startedAt).toLocaleTimeString()));
  if (step.endedAt) detail.appendChild(makeRow("ENDED", new Date(step.endedAt).toLocaleTimeString()));
}

function makeRow(label, val) {
  const row = el("div", "wf-detail-row");
  row.appendChild(el("span", "label", label + ": "));
  row.appendChild(el("span", "val", val));
  return row;
}

// ---------- brain panel ----------
function wireBrainPanel(node) {
  node.querySelector("#brain-selfimprove").onclick = () => {
    if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
    const prompt = "Run a recursive self-improvement cycle on this project. Measure a baseline, research opportunities, pick the single highest-leverage improvement, plan it, implement with TDD + checkpoint commits, review with a 5-reviewer fan-out, simplify, verify against baseline, and report. Stop at the plan stage and ask for my approval before implementing.";
    renderUserMessage({ content: [{ type: "text", text: prompt }] });
    state.ws.send(JSON.stringify({ kind: "prompt", text: prompt }));
  };
  refreshStats();
}

function logBrain(msg) {
  const log = $("brain-log");
  if (!log) return;
  const entry = el("div", "brain-log-entry", new Date().toLocaleTimeString() + " " + msg);
  log.appendChild(entry);
  log.scrollTop = log.scrollHeight;
}

// ---------- sandbox ----------
function bindSandbox() { /* sandbox panel binds itself via wireSandboxPanel */ }

function wireSandboxPanel(node) {
  const langSel = node.querySelector("#sb-lang");
  if (state.sandbox.languages.length) {
    langSel.innerHTML = "";
    state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; langSel.appendChild(o); });
  } else {
    loadSandboxLanguages().then(() => {
      langSel.innerHTML = "";
      state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; langSel.appendChild(o); });
    });
  }
  node.querySelector("#sb-run").onclick = () => runSandbox();
  node.querySelector("#sb-kill").onclick = () => killActiveRun();
  node.querySelector("#sb-mode").onchange = (e) => { state.sandbox.mode = e.target.value; toggleWebPreview(e.target.value === "web"); };
}

async function loadSandboxLanguages() {
  try {
    const { languages } = await api("/api/sandbox/languages");
    state.sandbox.languages = languages || [];
    const sel = $("sb-lang");
    if (sel) {
      sel.innerHTML = "";
      state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; sel.appendChild(o); });
    }
  } catch (e) { pushError("sandbox languages: " + e.message); }
}

async function runSandbox() {
  const code = $("sb-code").value;
  if (!code.trim()) { pushError("sandbox: empty code"); return; }
  const language = $("sb-lang").value;
  const mode = $("sb-mode").value;
  state.sandbox.mode = mode;
  $("sb-status").textContent = "starting…";
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

function handleSandboxEvent(e) {
  switch (e.type) {
    case "sandbox_start": {
      const run = e.run;
      const rec = { run, output: run.output || "", port: null, status: run.status };
      state.sandbox.runs.set(run.id, rec);
      state.sandbox.activeRunId = run.id;
      const rid = $("sb-runid"); if (rid) rid.textContent = "#" + run.id.slice(0, 8);
      const st = $("sb-status"); if (st) st.textContent = "running " + run.language;
      const kill = $("sb-kill"); if (kill) kill.classList.remove("hidden");
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
      if (rec) { rec.run = e.run; rec.status = e.run.status; }
      if (e.runId === state.sandbox.activeRunId) {
        const st = $("sb-status"); if (st) st.textContent = "ended: " + e.run.status + (e.run.exitCode != null ? " (exit " + e.run.exitCode + ")" : "");
        const kill = $("sb-kill"); if (kill) kill.classList.add("hidden");
      }
      renderSandboxRuns();
      refreshSandboxCount();
      break;
    }
  }
}

function appendSandboxOutput(line, stream) {
  const out = $("sb-output"); if (!out) return;
  const div = el("div", stream === "stderr" ? "sb-line-stderr" : "");
  div.textContent = line;
  out.appendChild(div);
  out.scrollTop = out.scrollHeight;
}

function toggleWebPreview(web) {
  const wrap = $("sb-web-wrap"); const outWrap = $("sb-output-wrap");
  if (wrap) wrap.classList.toggle("hidden", !web);
  if (outWrap) outWrap.style.flex = web ? "1 1 30%" : "1 1 100%";
  if (!web) { const iframe = $("sb-iframe"); if (iframe) iframe.src = "about:blank"; const cur = $("sb-cursor"); if (cur) cur.classList.add("hidden"); }
}

function loadWebPreview(port) {
  const lbl = $("sb-port-label"); if (lbl) lbl.textContent = "→ http://127.0.0.1:" + port;
  const iframe = $("sb-iframe"); if (iframe) iframe.src = "http://127.0.0.1:" + port + "/";
  const out = $("sb-output");
  if (out) { const div = el("div", "sb-line-port"); div.textContent = "[dotz] web server detected on port " + port; out.appendChild(div); out.scrollTop = out.scrollHeight; }
}

function renderCursor(x, y, action, text) {
  const cur = $("sb-cursor"); if (!cur) return;
  cur.classList.remove("hidden");
  cur.style.left = x + "px"; cur.style.top = y + "px";
  cur.classList.remove("click", "type");
  if (action === "click") cur.classList.add("click");
  if (action === "type") cur.classList.add("type");
  const lbl = $("sb-cursor-label"); if (lbl) lbl.textContent = action + (text ? ": " + truncate(text, 24) : "");
  dispatchCursorIntoIframe(x, y, action, text);
}

function dispatchCursorIntoIframe(x, y, action, text) {
  const iframe = $("sb-iframe");
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
  const box = $("sb-runs"); if (!box) return;
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
  const out = $("sb-output"); if (!out) return;
  out.innerHTML = "";
  (rec.output || "").split("\n").forEach((line) => { if (line) appendSandboxOutput(line, "stdout"); });
  const rid = $("sb-runid"); if (rid) rid.textContent = "#" + rec.run.id.slice(0, 8);
  const st = $("sb-status"); if (st) st.textContent = rec.run.status + (rec.run.exitCode != null ? " (exit " + rec.run.exitCode + ")" : "");
  const kill = $("sb-kill"); if (kill) kill.classList.toggle("hidden", rec.run.status !== "running");
  if (rec.port && state.sandbox.mode === "web") loadWebPreview(rec.port);
  else toggleWebPreview(state.sandbox.mode === "web");
}

function refreshSandboxCount() {
  const n = state.sandbox.runs.size;
  const st = $("st-sandbox"); if (st) st.textContent = n + " run" + (n === 1 ? "" : "s");
}

// ---------- status bar + stats ----------
async function refreshStats() {
  try {
    const s = await api(`/api/sessions/${state.sessionId}`);
    const st = s.stats || {};
    const tok = st.tokens || {};
    const tin = $("st-tok-in"); if (tin) tin.textContent = "↑" + fmtNum(tok.input || 0);
    const tout = $("st-tok-out"); if (tout) tout.textContent = "↓" + fmtNum(tok.output || 0);
    const cost = $("st-cost"); if (cost) cost.textContent = "$" + (st.cost || 0).toFixed(4).replace(/0+$/, "0");
    const cu = st.contextUsage;
    if (cu) {
      const pct = (cu.percent * 100);
      const p = $("st-ctx-pct"); if (p) p.textContent = pct.toFixed(1) + "%";
      const bar = $("st-ctx-bar"); if (bar) bar.style.width = Math.min(100, pct) + "%";
      const win = $("st-ctx-win"); if (win) win.textContent = "of " + fmtCtx(cu.contextWindow);
      // brain panel
      const bt = $("brain-tok"); if (bt) bt.textContent = fmtNum((tok.input || 0) + (tok.output || 0));
      const bc = $("brain-cost"); if (bc) bc.textContent = "$" + (st.cost || 0).toFixed(2);
      const bx = $("brain-ctx"); if (bx) bx.textContent = pct.toFixed(0) + "%";
    }
  } catch {}
}
function fmtNum(n) { return n >= 1000 ? (n / 1000).toFixed(1) + "K" : "" + n; }