/* dotz ultra code mode — bento dashboard UI.
 * Wires the minimal-agentic front end to the dotz backend (REST + WebSocket).
 * Handles: project launcher / command center, bento panels (chat/graph/brain/browser/memory/files/sandbox/skills),
 * drag-and-drop panel repositioning, on-the-fly SVG workflow graph, multi-provider models,
 * sandbox (terminal + web preview + agent cursor), in-app browser, file tree, chat streaming,
 * slash / skill / file command palette, tool cards, skills pool, human gate.
 * Vanilla JS, no framework, no build step. Runs identically in browser and Electron.
 */
"use strict";

const THINK_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh"];
const LAYOUT_KEY = "dotz.layout.v1";
const BRAIN_FLOAT_KEY = "dotz.brainFloat.v1";
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
  cur: null,
  toolCards: {},
  profiles: [],
  activeProfileId: "workflow",
  projects: [],
  activeProjectId: null,
  providers: [],
  activeProvider: "ollama",
  config: null,
  providerDefaults: {},
  modelCatalog: [],
  memory: [],
  skills: [],
  commands: [],
  projectFiles: [],
  sandbox: { languages: [], runs: new Map(), activeRunId: null, mode: "terminal" },
  workflows: new Map(),
  activeWfId: null,
  layout: { open: ["chat"], version: 1 },
  wfView: { x: 0, y: 0, w: 800, h: 600 },
  pendingGate: null,
  brainFloat: true,
  updateStatus: null,
  browserSessionId: null,
  browserObservation: null,
  browserPollTimer: null,
  browserBusy: false,
  chatAutoScroll: true,
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

init().catch((e) => pushError("init failed: " + e.message));

async function init() {
  window.addEventListener('unhandledrejection', (e) => {
    const r = e.reason;
    pushError('unhandled: ' + ((r && r.message) || r));
  });
  window.addEventListener('error', (e) => pushError('error: ' + e.message));
  bindTopbar();
  bindCommandCenter();
  bindProjectSelector();
  bindComposer();
  bindPalette();
  bindKeyboard();
  bindBrainFloat();
  bindGateCard();
  bindNodeDetail();
  bindSettings();
  bindUpdateCard();
  wireUpdaterIpc();
  loadLayout();
  loadBrainFloat();
  await loadProfiles();
  await loadProviders();
  await loadDotzConfig();
  primeTopbarFromConfig();
  await loadProjects();
  await loadSandboxLanguages();
  showCommandCenter();
}

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
function loadBrainFloat() {
  try {
    const raw = localStorage.getItem(BRAIN_FLOAT_KEY);
    if (raw != null) state.brainFloat = raw === "1";
  } catch {}
  updateBrainFloat();
}
function saveBrainFloat() {
  try { localStorage.setItem(BRAIN_FLOAT_KEY, state.brainFloat ? "1" : "0"); } catch {}
}

/* ---------- command center / launcher ---------- */
function showCommandCenter() {
  $("command-center").classList.remove("hidden");
  $("bento").classList.add("hidden");
  $("project-name").textContent = "NO PROJECT";
  renderProjectDropdown();
  updateCommandCenterMeta();
}

function enterBento() {
  $("command-center").classList.add("hidden");
  $("bento").classList.remove("hidden");
  renderBento();
}

function updateCommandCenterMeta() {
  const p = state.projects.find((x) => x.id === state.activeProjectId);
  $("cc-project").textContent = p ? p.name : "no project selected";
  $("cc-project").classList.toggle("dim", !p);
  $("cc-profile").textContent = (p && p.profileId) || state.activeProfileId || "workflow";
  const cc = $("cc-model");
  const hasModel = !!(p && p.model);
  cc.textContent = hasModel ? `${p.model.provider}/${p.model.modelId}` : "no model";
  cc.classList.toggle("dim", !hasModel);
}

function bindCommandCenter() {
  const input = $("composer-input");
  const sendBtn = $("send-btn");
  const stopBtn = $("stop-btn");
  input.addEventListener("input", autoGrow);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendFrom(input); }
    else if (e.key === "Escape") { hideCmdPalette(); }
  });
  sendBtn.onclick = () => sendFrom(input);
  stopBtn.onclick = () => state.ws && state.ws.send(JSON.stringify({ kind: "abort" }));
  const chips = $("quick-chips");
  chips.innerHTML = "";
  ["implement", "scout-and-plan", "implement-and-review"].forEach((cmd) => {
    const chip = el("span", "qchip", "/" + cmd);
    chip.onclick = () => { input.value = "/" + cmd + " "; input.focus(); autoGrow(); };
    chips.appendChild(chip);
  });
}

/* ---------- project selector ---------- */
function bindProjectSelector() {
  const btn = $("project-select");
  const dropdown = $("project-dropdown");
  btn.onclick = (e) => {
    e.stopPropagation();
    dropdown.classList.toggle("hidden");
    if (!dropdown.classList.contains("hidden")) renderProjectDropdown();
  };
  document.addEventListener("click", (e) => {
    if (!dropdown.contains(e.target) && e.target !== btn) dropdown.classList.add("hidden");
  });
  $("project-new-btn").onclick = () => $("project-form").classList.remove("hidden");
  $("pf-cancel").onclick = () => $("project-form").classList.add("hidden");
  $("pf-create").onclick = async () => {
    const name = $("pf-name").value.trim();
    const cwd = $("pf-cwd").value.trim();
    if (!name || !cwd) { pushError("project needs name + cwd"); return; }
    if (!/^[A-Za-z]:[\\/]/.test(cwd) && !/^\//.test(cwd)) {
      pushError("cwd must be an absolute path (e.g. C:\\Users\\me\\proj or /home/me/proj)");
      return;
    }
    const body = {
      name, cwd,
      profileId: $("pf-profile").value,
      model: { provider: (state.config && state.config.provider) || "ollama", modelId: $("pf-model").value.trim() || (state.config && state.config.executiveModel) || "glm-5.2" },
    };
    try {
      const p = await post("/api/projects", body);
      $("project-form").classList.add("hidden");
      await loadProjects();
      await openProject(p.id);
    } catch (e) { pushError("create project: " + e.message); }
  };
}

function renderProjectDropdown() {
  const list = $("project-list");
  list.innerHTML = "";
  if (!state.projects.length) {
    const empty = el("div", "dropdown-empty");
    empty.appendChild(el("div", "de-glyph", "◆"));
    empty.appendChild(el("div", "de-title", "No projects yet"));
    empty.appendChild(el("div", "de-sub dim", "Create one below ↓"));
    list.appendChild(empty);
  } else {
    state.projects.forEach((p) => {
      const item = el("button", "project-item");
      item.appendChild(el("span", "pi-glyph", "◆"));
      const info = el("div", "pi-info");
      info.appendChild(el("div", "pi-name", p.name));
      info.appendChild(el("div", "pi-cwd", p.cwd));
      info.appendChild(el("div", "pi-profile", (p.profileId || "workflow").toUpperCase()));
      item.appendChild(info);
      item.onclick = () => { $("project-dropdown").classList.add("hidden"); openProject(p.id); };
      list.appendChild(item);
    });
  }
  const sel = $("pf-profile");
  sel.innerHTML = "";
  state.profiles.forEach((p) => { const o = el("option", null, p.name); o.value = p.id; sel.appendChild(o); });
  sel.value = state.activeProfileId;
}

async function openProject(id) {
  state.activeProjectId = id;
  const p = state.projects.find((x) => x.id === id);
  if (p) {
    state.activeProfileId = p.profileId || state.activeProfileId;
    $("project-name").textContent = p.name;
  }
  enterBento();
  await newSession();
}

/* ---------- bento panels ---------- */
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
  if (name === "chat") wireChatPanel(node);
  else if (name === "graph") wireGraphPanel(node);
  else if (name === "brain") wireBrainPanel(node);
  else if (name === "memory") wireMemoryPanel(node);
  else if (name === "sandbox") wireSandboxPanel(node);
  else if (name === "skills") wireSkillsPanel(node);
  else if (name === "browser") wireBrowserPanel(node);
  else if (name === "files") wireFilesPanel(node);
}

function unmountPanel(name) {
  if (name === "browser" && state.browserPollTimer) {
    clearInterval(state.browserPollTimer);
    state.browserPollTimer = null;
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
  head.addEventListener("dragleave", () => head.classList.remove("drag-over"));
  head.addEventListener("drop", (e) => {
    e.preventDefault();
    head.classList.remove("drag-over");
    const from = e.dataTransfer.getData("text/plain");
    if (from && from !== name) swapPanels(from, name);
  });
}

function swapPanels(from, to) {
  const fromNode = document.querySelector(`.panel[data-panel="${from}"]`);
  const toNode = document.querySelector(`.panel[data-panel="${to}"]`);
  if (!fromNode || !toNode) return;
  const fromClass = fromNode.className;
  const toClass = toNode.className;
  fromNode.className = toClass;
  toNode.className = fromClass;
  const tmp = fromNode.dataset.panel;
  fromNode.dataset.panel = toNode.dataset.panel;
  toNode.dataset.panel = tmp;
  saveLayout();
}

function openPanel(name) {
  if (state.layout.open.includes(name)) return;
  state.layout.open.push(name);
  saveLayout();
  mountPanel(name);
}

function bindTopbar() {
  $("panels-btn").onclick = togglePalette;
  $("brain-float-toggle").onclick = () => {
    state.brainFloat = !state.brainFloat;
    saveBrainFloat();
    updateBrainFloat();
  };
}

function updateBrainFloat() {
  const bf = $("brain-float");
  bf.classList.toggle("hidden", !state.brainFloat || !state.sessionId);
}

function bindBrainFloat() {
  updateBrainFloat();
}

/* ---------- palette ---------- */
function bindPalette() {
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
    // Modal-aware Escape routing + Tab focus trap. A visible dialog takes priority.
    const modal = visibleModal();
    if (modal) {
      if (e.key === "Escape") {
        e.preventDefault();
        // Gate stays modal until an explicit choice; settings/update get a close path.
        if (modal.id === "settings-card") $("settings-close").click();
        else if (modal.id === "update-card") $("update-later").click();
        return;
      }
      if (e.key === "Tab") trapFocus(e, modal);
      return;
    }
    if (e.key === "Escape") {
      $("palette").classList.add("hidden");
      $("project-dropdown").classList.add("hidden");
      hideCmdPalette();
    }
  });
}

// Returns the topmost visible modal dialog element, or null.
function visibleModal() {
  for (const id of ["update-card", "settings-card", "gate-card"]) {
    const card = $(id);
    if (card && !card.classList.contains("hidden")) return card;
  }
  return null;
}

// Cycle Tab focus within the visible .*-card-inner so it can't escape the dialog.
function trapFocus(e, modal) {
  const inner = modal.querySelector(".gate-card-inner, .settings-card-inner, .update-card-inner") || modal;
  const focusables = inner.querySelectorAll('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])');
  // offsetParent is null inside position:fixed modals, so use the getClientRects visibility idiom instead.
  const visible = Array.from(focusables).filter((n) => !n.classList.contains("hidden") && (n.offsetWidth || n.offsetHeight || n.getClientRects().length));
  if (!visible.length) return;
  const first = visible[0];
  const last = visible[visible.length - 1];
  if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
}

/* ---------- profiles ---------- */
async function loadProfiles() {
  try {
    const { profiles, default: def } = await api("/api/profiles");
    state.profiles = profiles;
    state.activeProfileId = def || "workflow";
    renderProfilePicker();
  } catch (e) { pushError("profiles: " + e.message); }
}
function renderProfilePicker() {
  const box = $("profile-picker");
  box.innerHTML = "";
  state.profiles.forEach((p) => {
    const b = el("button", "prof-btn" + (p.id === state.activeProfileId ? " active" : ""), p.name);
    b.dataset.id = p.id;
    box.appendChild(b);
  });
}

async function loadProviders() {
  try { const { providers } = await api("/api/providers"); state.providers = providers || []; }
  catch (e) { state.providers = []; pushError("providers: " + e.message); }
}

async function loadDotzConfig() {
  try {
    const r = await api("/api/config");
    state.config = r.config || null;
    state.providerDefaults = r.providerDefaults || {};
  } catch (e) { state.config = null; state.providerDefaults = {}; pushError("config: " + e.message); }
}

async function persistConfig(patch) {
  try { const r = await post("/api/config", patch); state.config = r.config || state.config; }
  catch (e) { pushError("config: " + e.message); }
}

async function loadProjects() {
  try { const { projects } = await api("/api/projects"); state.projects = projects || []; }
  catch (e) { pushError("projects: " + e.message); }
}

/* ---------- session lifecycle ---------- */
async function newSession() {
  const s = await post("/api/sessions", { profileId: state.activeProfileId, projectId: state.activeProjectId || undefined });
  setSession(s);
  clearTranscript();
  connectWS();
  await Promise.all([loadModels(), loadCommands(), refreshMemory(), refreshSkills(), refreshWorkflows(), loadProjectFiles()]);
  await refreshStats();
  refreshSandboxCount();
  refreshWfCount();
  updateBrainFloat();
}

function setSession(s) {
  state.summary = s;
  state.sessionId = s.sessionId;
  if (s.profileId) state.activeProfileId = s.profileId;
  if (s.projectId) state.activeProjectId = s.projectId;
  updateModelUI(s.model);
  renderReasoning(s);
}

/* ---------- websocket ---------- */
let _wsAttempt = 0, _wsIntentional = false;
function connectWS() {
  if (state.ws) { _wsIntentional = true; try { state.ws.close(); } catch {} }
  _wsIntentional = false;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws?sessionId=${state.sessionId}`);
  state.ws = ws;
  setConn("off", "ws connecting…");
  ws.onopen = () => { _wsAttempt = 0; setConn("on", "ws ✓ " + location.host); };
  ws.onclose = () => {
    setConn("off", "ws closed");
    if (!_wsIntentional && state.sessionId && ws === state.ws) {
      const delay = Math.min(15000, 500 * 2 ** _wsAttempt++);
      setConn("off", "reconnecting…");
      setTimeout(connectWS, delay);
    }
  };
  ws.onerror = () => setConn("err", "ws error");
  ws.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return; }
    if (m.kind === "ready") { /* session confirmed */ }
    else if (m.kind === "event") handleEvent(m.event);
    else if (m.kind === "sandbox") handleSandboxEvent(m.event);
    else if (m.kind === "workflow") handleWorkflowEvent(m.runId, m.event);
    else if (m.kind === "browser") renderBrowserObservation(m.event);
    else if (m.kind === "gate") showGateCard(m.gateId, m.plan);
    else if (m.kind === "memory_recall") handleMemoryRecall(m);
    else if (m.kind === "error") pushError(m.error);
  };
}

function setConn(kind, txt) {
  const c = $("conn-chip");
  c.className = "chip chip-" + kind;
  const label = kind === "on" ? "PI ONLINE" : kind === "err" ? "WS ERROR" : "OFFLINE";
  c.innerHTML = '<span class="blk">█</span> ' + label;
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
    case "agent_end": setStreaming(false); refreshStats(); logBrain("agent_end"); break;
  }
  scrollBottom();
}

function stringifyResult(r) {
  if (r == null) return "";
  if (typeof r === "string") return r;
  if (r.content && Array.isArray(r.content)) return r.content.map((c) => c.text || "").join("\n");
  return JSON.stringify(r, null, 2);
}

/* ---------- chat / composer ---------- */
function wireChatPanel(node) {
  const input = node.querySelector('[data-role="composer-input"]');
  const sendBtn = node.querySelector('[data-role="send-btn"]');
  const stopBtn = node.querySelector('[data-role="stop-btn"]');
  const transcript = node.querySelector('[data-role="transcript"]');
  state.chatAutoScroll = true;
  const updateFollowTail = () => {
    const distanceFromTail = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight;
    state.chatAutoScroll = distanceFromTail <= 48;
  };
  // Scroll events also fire for our own follow-tail writes. Listen to user intent instead so a
  // fast-growing message cannot accidentally classify a programmatic scroll as "user scrolled up".
  transcript.addEventListener("wheel", (event) => {
    if (event.deltaY < 0) state.chatAutoScroll = false;
    else requestAnimationFrame(updateFollowTail);
  }, { passive: true });
  transcript.addEventListener("pointerdown", () => { state.chatAutoScroll = false; });
  transcript.addEventListener("pointerup", () => requestAnimationFrame(updateFollowTail));
  transcript.addEventListener("keydown", (event) => {
    if (["PageUp", "Home", "ArrowUp"].includes(event.key)) state.chatAutoScroll = false;
    else requestAnimationFrame(updateFollowTail);
  });
  attachComposerPalette(input);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendFrom(input); }
    else if (e.key === "Escape") hideCmdPalette();
    else if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Tab") {
      const palette = $("cmd-palette");
      if (!palette.classList.contains("hidden")) { e.preventDefault(); navigatePalette(e.key === "ArrowDown" || e.key === "Tab" ? 1 : -1); }
    } else if (e.key === "Enter" && isPaletteOpen()) {
      e.preventDefault();
      selectPaletteItem(input);
    }
  });
  sendBtn.onclick = () => sendFrom(input);
  stopBtn.onclick = () => state.ws && state.ws.send(JSON.stringify({ kind: "abort" }));
  const chips = node.querySelector('[data-role="quick-chips"]');
  chips.innerHTML = "";
  ["implement", "scout-and-plan", "implement-and-review"].forEach((cmd) => {
    const chip = el("span", "qchip", "/" + cmd);
    chip.onclick = () => { input.value = "/" + cmd + " "; input.focus(); autoGrow(); };
    chips.appendChild(chip);
  });
}

function autoGrow() {
  const input = activeComposerInput();
  if (!input) return;
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 260) + "px";
}

function sendFrom(input) {
  const text = input.value.trim();
  if (!text) return;
  if (!state.sessionId) {
    pushError("open a project first");
    return;
  }
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  renderUserMessage({ content: [{ type: "text", text }] });
  state.ws.send(JSON.stringify({ kind: "prompt", text }));
  input.value = "";
  autoGrow();
  hideCmdPalette();
}

function setStreaming(b) {
  state.streaming = b;
  const chat = document.querySelector('.panel[data-panel="chat"]');
  if (chat) {
    const sendBtn = chat.querySelector('[data-role="send-btn"]');
    const stopBtn = chat.querySelector('[data-role="stop-btn"]');
    if (sendBtn) sendBtn.classList.toggle("hidden", b);
    if (stopBtn) stopBtn.classList.toggle("hidden", !b);
  }
  const ccSend = $("send-btn");
  const ccStop = $("stop-btn");
  if (ccSend && ccStop && !ccSend.closest('.panel[data-panel="chat"]')) {
    ccSend.classList.toggle("hidden", b);
    ccStop.classList.toggle("hidden", !b);
  }
  $("st-state").textContent = b ? "streaming…" : "idle";
}

function insertCommand(name) {
  const input = activeComposerInput();
  if (!input) return;
  input.value = ("/" + name + " ").replace("//", "/");
  input.focus();
  autoGrow();
}

function activeComposerInput() {
  const chat = document.querySelector('.panel[data-panel="chat"]');
  if (chat) return chat.querySelector('[data-role="composer-input"]');
  return $("composer-input");
}

/* ---------- composer palette ---------- */
// Reusable per-textarea palette trigger: closes over the passed input so it works for both
// the command-center textarea and each class-scoped chat-panel clone.
function attachComposerPalette(input) {
  if (!input) return;
  input.addEventListener("input", () => {
    autoGrow();
    const before = input.value.slice(0, input.selectionStart);
    const m = before.match(/(^|\s)([/@#])(\w*)$/);
    if (m) {
      const items = paletteItemsFor(m[2], m[3]);
      if (items.length) showCmdPalette(items, m[2], m[3]); else hideCmdPalette();
    } else hideCmdPalette();
  });
  input.addEventListener("blur", () => { setTimeout(hideCmdPalette, 180); });
}

function bindComposer() {
  const input = $("composer-input");
  if (!input) return;
  attachComposerPalette(input);
  const pinput = $("cmd-palette-input");
  pinput.addEventListener("input", () => filterPalette(pinput.value));
  pinput.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); navigatePalette(e.key === "ArrowDown" ? 1 : -1); }
    if (e.key === "Enter") { e.preventDefault(); selectPaletteItem(activeComposerInput()); }
    if (e.key === "Escape") hideCmdPalette();
  });
}

function paletteItemsFor(type, prefix) {
  const p = prefix.toLowerCase();
  if (type === "/") {
    const defaults = ["implement", "scout-and-plan", "implement-and-review", "ultra-code-review", "self-improve", "e2e-test"].map((name) => ({ name, description: "workflow preset", source: "prompt" }));
    const cmds = state.commands.length ? state.commands : defaults;
    return cmds
      .filter((c) => c.name.toLowerCase().startsWith(p))
      .map((c) => ({ icon: "/", name: c.name, desc: c.description || "", source: c.source || "cmd", insert: "/" + c.name + " " }));
  }
  if (type === "@") {
    return state.skills
      .filter((s) => s.name.toLowerCase().startsWith(p))
      .slice(0, 40)
      .map((s) => ({ icon: "@", name: s.name, desc: s.description || "", source: s.source || "skill", insert: "@" + s.name + " " }));
  }
  if (type === "#") {
    return state.projectFiles
      .filter((f) => f.path.toLowerCase().includes(p))
      .slice(0, 40)
      .map((f) => ({ icon: f.type === "dir" ? "▤" : "▥", name: f.path, desc: f.type, source: "file", insert: "#" + f.path + " " }));
  }
  return [];
}

let _paletteItems = [];
let _paletteType = null;
function showCmdPalette(items, type, prefix) {
  _paletteItems = items;
  _paletteType = type;
  const palette = $("cmd-palette");
  const list = $("cmd-palette-list");
  const pinput = $("cmd-palette-input");
  palette.classList.remove("hidden");
  pinput.value = prefix;
  pinput.focus();
  renderPaletteList(items, 0);
}

function filterPalette(prefix) {
  const p = prefix.toLowerCase();
  const filtered = _paletteItems.filter((i) => i.name.toLowerCase().includes(p));
  renderPaletteList(filtered, 0);
}

function renderPaletteList(items, activeIdx) {
  const list = $("cmd-palette-list");
  list.innerHTML = "";
  items.forEach((it, idx) => {
    const row = el("button", "cmd-item" + (idx === activeIdx ? " active" : ""));
    row.appendChild(el("span", "cmd-glyph", it.icon));
    row.appendChild(el("span", "cmd-name", it.name));
    row.appendChild(el("span", "cmd-desc", it.desc));
    row.appendChild(el("span", "cmd-source", it.source));
    row.onclick = () => {
      _paletteItems = [it];
      selectPaletteItem(activeComposerInput());
    };
    list.appendChild(row);
  });
  list.dataset.active = String(activeIdx);
}

function navigatePalette(delta) {
  const list = $("cmd-palette-list");
  const rows = list.querySelectorAll(".cmd-item");
  let active = parseInt(list.dataset.active || "0", 10) || 0;
  active = Math.max(0, Math.min(rows.length - 1, active + delta));
  renderPaletteList(Array.from(rows).map((r) => ({
    icon: r.querySelector(".cmd-glyph").textContent,
    name: r.querySelector(".cmd-name").textContent,
    desc: r.querySelector(".cmd-desc").textContent,
    source: r.querySelector(".cmd-source").textContent,
  })), active);
}

function selectPaletteItem(input) {
  if (!input) return;
  const list = $("cmd-palette-list");
  const rows = list.querySelectorAll(".cmd-item");
  const active = parseInt(list.dataset.active || "0", 10) || 0;
  const selected = rows[active];
  if (!selected) return;
  const name = selected.querySelector(".cmd-name").textContent;
  const insert = (_paletteType === "/" ? "/" : _paletteType) + name + " ";
  const text = input.value;
  const sel = input.selectionStart;
  const before = text.slice(0, sel);
  const m = before.match(/(^|\s)([/@#])(\w*)$/);
  if (!m) return;
  const newBefore = before.slice(0, m.index + m[1].length) + insert;
  input.value = newBefore + text.slice(sel);
  input.selectionStart = input.selectionEnd = newBefore.length;
  input.focus();
  autoGrow();
  hideCmdPalette();
}

function isPaletteOpen() {
  return !$('cmd-palette').classList.contains('hidden');
}

function hideCmdPalette() {
  $("cmd-palette").classList.add("hidden");
  _paletteItems = [];
}

/* ---------- transcript ---------- */
function clearTranscript() {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (t) { t.innerHTML = ""; state.cur = null; state.toolCards = {}; state.chatAutoScroll = true; }
}
function scrollBottom(force = false) {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t || (!force && !state.chatAutoScroll)) return;
  state.chatAutoScroll = true;
  requestAnimationFrame(() => {
    // Re-check inside the frame: a user scroll can occur after this callback was queued by a
    // streaming token. In that case the stale callback must not yank them back to the tail.
    if (force || state.chatAutoScroll) t.scrollTop = t.scrollHeight;
  });
}
function renderUserMessage(message) {
  const text = (message.content || []).map((c) => c.text || "").join("");
  const m = el("div", "msg user");
  m.appendChild(el("div", "msg-role", "you"));
  m.appendChild(el("div", "bubble", text));
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (t) { t.appendChild(m); scrollBottom(true); }
}
function ensureAssistantBubble() {
  if (state.cur && state.cur.bubble.isConnected) return state.cur;
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "dotz"));
  const bubble = el("div", "bubble");
  m.appendChild(bubble);
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t) return state.cur;
  t.appendChild(m);
  state.cur = { bubble, partial: { content: [] } };
  return state.cur;
}
function hasRenderable(partial) {
  return (partial.content || []).some((b) => (b.type === "text" && b.text) || (b.type === "thinking" && b.thinking && b.thinking.trim()));
}
/* Minimal, dependency-free Markdown -> safe HTML for assistant output. Escapes ALL text first,
   then layers block + inline formatting so LLM replies render as real prose instead of a raw blob. */
function renderMarkdown(src) {
  if (src == null) return "";
  const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const fences = [];
  const s = String(src).replace(/```(\w*)\r?\n?([\s\S]*?)```/g, (_, _lang, code) => {
    fences.push(`<pre class="md-pre"><code>${esc(code.replace(/\n+$/, ""))}</code></pre>`);
    return ` F${fences.length - 1} `;
  });
  const inline = (line) => esc(line)
    .replace(/`([^`]+)`/g, (_, c) => `<code class="md-code">${c}</code>`)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  const out = [];
  let list = null;
  let para = [];
  const flushPara = () => { if (para.length) { out.push(`<p>${para.map(inline).join("<br>")}</p>`); para = []; } };
  const flushList = () => { if (list) { out.push(`<${list.tag}>${list.items.join("")}</${list.tag}>`); list = null; } };
  const flush = () => { flushPara(); flushList(); };
  for (const raw of s.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    let m;
    if ((m = line.match(/^ F(\d+) $/))) { flush(); out.push(fences[+m[1]]); continue; }
    if (!line.trim()) { flush(); continue; }
    if ((m = line.match(/^(#{1,4})\s+(.*)$/))) { flush(); out.push(`<h${m[1].length} class="md-h">${inline(m[2])}</h${m[1].length}>`); continue; }
    if ((m = line.match(/^\s*[-*+]\s+(.*)$/))) { flushPara(); if (!list || list.tag !== "ul") { flushList(); list = { tag: "ul", items: [] }; } list.items.push(`<li>${inline(m[1])}</li>`); continue; }
    if ((m = line.match(/^\s*\d+[.)]\s+(.*)$/))) { flushPara(); if (!list || list.tag !== "ol") { flushList(); list = { tag: "ol", items: [] }; } list.items.push(`<li>${inline(m[1])}</li>`); continue; }
    if ((m = line.match(/^>\s?(.*)$/))) { flush(); out.push(`<blockquote>${inline(m[1])}</blockquote>`); continue; }
    if (/^(---+|\*\*\*+|___+)$/.test(line.trim())) { flush(); out.push("<hr>"); continue; }
    flushList(); para.push(line);
  }
  flush();
  return out.join("");
}

/* One-line summary of a tool call for the collapsed card header, so the transcript reads as a
   compact list of actions instead of a wall of raw JSON args + output. */
function toolPreview(name, args) {
  if (!args || typeof args !== "object") return typeof args === "string" ? args : "";
  const a = args;
  switch (name) {
    case "bash": return a.command || "";
    case "read": case "write": case "edit": return a.file_path || a.path || "";
    case "grep": return (a.pattern ? "/" + a.pattern + "/" : "") + (a.path ? " in " + a.path : "");
    case "find": return (a.pattern || "*") + (a.path ? " in " + a.path : "");
    case "ls": return a.path || ".";
    case "subagent": return a.tasks ? `${a.tasks.length} parallel` : a.chain ? `${a.chain.length}-step chain` : (a.agent || "");
    case "skill": return a.name || a.command || "";
    default: { try { const s = JSON.stringify(a); return s.length > 90 ? s.slice(0, 90) + "…" : s; } catch { return ""; } }
  }
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
      const t = el("div", "assistant-text md");
      t.innerHTML = renderMarkdown(block.text || "");
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
    // A <details> so each tool call is a compact, collapsed one-liner (icon · name · preview · status)
    // that the user can expand for full args/output — instead of a wall of raw JSON in the transcript.
    const card = el("details", "toolcard");
    card.dataset.tc = id;
    const head = el("summary", "toolcard-head");
    const glyph = el("span", "toolcard-glyph", "⚙");
    const nameEl = el("span", "toolcard-name", "tool");
    const previewEl = el("span", "toolcard-preview");
    const badge = el("span", "toolcard-status status-run", "● RUN");
    head.appendChild(glyph); head.appendChild(nameEl); head.appendChild(previewEl); head.appendChild(badge);
    const body = el("div", "toolcard-body");
    const argsEl = el("pre", "toolcard-args");
    const outEl = el("pre", "toolcard-out");
    outEl.style.display = "none";
    body.appendChild(argsEl); body.appendChild(outEl);
    card.appendChild(head); card.appendChild(body);
    const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
    if (!t) return;
    t.appendChild(card);
    tc = state.toolCards[id] = { card, nameEl, previewEl, badge, argsEl, outEl, data: {} };
  }
  Object.assign(tc.data, patch);
  const d = tc.data;
  if (d.name) { tc.nameEl.textContent = d.name; tc.previewEl.textContent = toolPreview(d.name, d.args); }
  if (d.args !== undefined) tc.argsEl.textContent = typeof d.args === "string" ? d.args : JSON.stringify(d.args, null, 2);
  if (d.output) { tc.outEl.style.display = ""; tc.outEl.textContent = d.output; }
  const st = d.status || "run";
  tc.card.classList.toggle("is-running", st === "run");
  tc.badge.className = "toolcard-status " + (st === "done" ? "status-done" : st === "err" ? "status-err" : "status-run");
  tc.badge.textContent = st === "done" ? "✓ DONE" : st === "err" ? "✕ ERR" : "● RUN";
}
function showToast(msg) {
  const host = $("toast"); if (!host) { console.error(msg); return; }
  const n = el("div", "toast-item", msg); host.appendChild(n);
  setTimeout(() => n.remove(), 6000);
}
function pushError(msg) {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t) { showToast("⚠ " + msg); return; }
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "system"));
  const b = el("div", "bubble");
  b.appendChild(el("div", "msg-error", "⚠ " + msg));
  m.appendChild(b);
  t.appendChild(m);
  scrollBottom(true);
}

/* ---------- model + provider controls ---------- */
// Populate #provider-select from a provider-meta list (shared by loadModels + primeTopbarFromConfig).
function populateProviderSelect(providerMeta) {
  const provSel = $("provider-select");
  if (!provSel) return;
  provSel.innerHTML = "";
  (providerMeta || state.providers || []).forEach((p) => {
    const o = el("option", null, p.label || p.id);
    o.value = p.id;
    provSel.appendChild(o);
  });
}

// First-run priming: before any session exists, populate the four topbar knobs from state.config
// so a brand-new user sees the real defaults (ollama/glm-5.2) instead of empty controls.
function primeTopbarFromConfig() {
  if (!state.config) return;
  state.activeProvider = state.config.provider || state.activeProvider || "ollama";
  populateProviderSelect(state.providers);
  const provSel = $("provider-select");
  if (provSel) provSel.value = state.activeProvider;
  renderModelInput();
  populateModelSuggestions();
  if (state.config.executiveModel) { const mi = $("model-input"); if (mi) mi.value = state.config.executiveModel; }
  if (state.config.subagentModel) { const si = $("subagent-input"); if (si) si.value = state.config.subagentModel; }
  // Prime REASONING with a synthesized stub; renderReasoning early-returns the POST when no session exists.
  renderReasoning({ thinkingLevel: state.config.thinkingLevel, availableThinkingLevels: THINK_LEVELS, supportsThinking: true });
}

async function loadModels() {
  const m = await api(`/api/sessions/${state.sessionId}/models`);
  state.modelCatalog = m.available || [];
  populateProviderSelect(m.providerMeta);
  const provSel = $("provider-select");
  updateModelUI(m.current || state.summary.model);
  state.activeProvider = (m.current && m.current.provider) || (state.config && state.config.provider) || "ollama";
  provSel.value = state.activeProvider;
  provSel.onchange = () => onProviderChange(provSel.value);
  renderModelInput();
  populateModelSuggestions();
  syncSubagentInput();
}

// Switching provider prefills both the executive + subagent model ids with that provider's
// defaults (the user can then type any id), persists them, and applies the executive model live.
function onProviderChange(prov) {
  state.activeProvider = prov;
  const d = (state.providerDefaults && state.providerDefaults[prov]) || null;
  if (d) { $("model-input").value = d.executive; const si = $("subagent-input"); if (si) si.value = d.subagent; }
  renderModelInput();
  populateModelSuggestions();
  syncSubagentInput();
  const patch = { provider: prov };
  if (d) { patch.executiveModel = d.executive; patch.subagentModel = d.subagent; }
  persistConfig(patch);
  if (d) setModel({ provider: prov, modelId: d.executive });
}

// The SUBAGENT knob: the model every dispersed subagent runs on. Persisted to dotz config,
// which sets DOTZ_SUBAGENT_MODEL server-side for future subagent spawns.
function syncSubagentInput() {
  const input = $("subagent-input");
  if (!input) return;
  if (state.config && state.config.subagentModel && !input.value) input.value = state.config.subagentModel;
  const dl = $("subagent-suggestions");
  if (dl) { dl.innerHTML = ""; (state.modelCatalog || []).filter((x) => x.provider === state.activeProvider).slice(0, 400).forEach((x) => { const o = el("option"); o.value = x.modelId; dl.appendChild(o); }); }
  input.onchange = () => { const v = input.value.trim(); if (v) persistConfig({ subagentModel: v, provider: state.activeProvider }); };
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
    input.onkeydown = (e) => { if (e.key === "Enter") { e.preventDefault(); submitModel(); } };
  }
}
function populateModelSuggestions() {
  const dl = $("model-suggestions");
  dl.innerHTML = "";
  state.modelCatalog.filter((x) => x.provider === state.activeProvider).slice(0, 400).forEach((x) => { const o = el("option"); o.value = x.modelId; dl.appendChild(o); });
}
function updateModelUI(model) {
  const id = model ? model.modelId : "glm-5.2";
  const prov = model ? model.provider : "ollama";
  const free = providerIsFreeForm(prov);
  if (free) $("model-input").value = id;
  else $("model-select").value = id;
  const cc = $("cc-model");
  if (cc) { cc.textContent = prov + "/" + id; cc.classList.remove("dim"); }
}
async function submitModel() { const id = $("model-input").value.trim(); if (id) { await setModel({ provider: state.activeProvider, modelId: id }); persistConfig({ provider: state.activeProvider, executiveModel: id }); } }
async function submitModelSelect() { const id = $("model-select").value; if (id) await setModel({ provider: state.activeProvider, modelId: id }); }
async function setModel(ref) {
  try { const s = await post(`/api/sessions/${state.sessionId}/model`, ref); state.summary = s; updateModelUI(s.model); renderReasoning(s); }
  catch (e) { pushError("model: " + e.message); }
}

/* ---------- reasoning ---------- */
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
      // Pre-session (command center): no sessionId yet — POSTing would hit /api/sessions/null/thinking (404).
      if (!state.sessionId) return;
      try { const r = await post(`/api/sessions/${state.sessionId}/thinking`, { level: lvl }); state.summary = Object.assign({}, state.summary, r); renderReasoning(state.summary); persistConfig({ thinkingLevel: lvl }); }
      catch (e) { pushError("thinking: " + e.message); }
    };
    seg.appendChild(b);
  });
}

/* ---------- commands ---------- */
async function loadCommands() {
  try {
    const { commands } = await api(`/api/sessions/${state.sessionId}/commands`);
    state.commands = commands || [];
    renderQuickChips();
  } catch {}
}

function renderQuickChips() {
  const chat = document.querySelector('.panel[data-panel="chat"]');
  const chips = chat ? chat.querySelector('[data-role="quick-chips"]') : $("quick-chips");
  if (!chips) return;
  chips.innerHTML = "";
  const defaults = ["implement", "scout-and-plan", "implement-and-review"];
  const candidates = defaults.map((name) => ({ name, description: "" })).concat(state.commands);
  const seen = new Set();
  for (const command of candidates) {
    if (!command.name || seen.has(command.name)) continue;
    seen.add(command.name);
    const chip = el("span", "qchip", "/" + command.name);
    chip.title = command.description || "";
    chip.onclick = () => insertCommand(command.name);
    chips.appendChild(chip);
    if (seen.size >= 6) break;
  }
}

/* ---------- memory (mem0-backed, autonomous) ---------- */
function wireMemoryPanel(node) {
  const form = node.querySelector("#mem-form");
  node.querySelector("#mem-add").onclick = () => {
    form.classList.toggle("hidden");
    if (!form.classList.contains("hidden")) {
      node.querySelector("#mem-text").value = "";
      node.querySelector("#mem-category").value = "";
      node.querySelector("#mem-text").focus();
    }
  };
  node.querySelector("#mem-cancel").onclick = () => form.classList.add("hidden");
  node.querySelector("#mem-create").onclick = async () => {
    const text = node.querySelector("#mem-text").value.trim();
    if (!text) { pushError("memory text required"); return; }
    try {
      await post("/api/memory", {
        projectId: state.activeProjectId || "",
        text,
        category: node.querySelector("#mem-category").value.trim() || undefined,
        scope: node.querySelector("#mem-scope").value,
      });
      form.classList.add("hidden");
      await refreshMemory();
    } catch (e) { pushError("memory add: " + e.message); }
  };
  node.querySelector("#mem-consolidate").onclick = async () => {
    try {
      const r = await post("/api/memory/consolidate", { projectId: state.activeProjectId || "" });
      pushError(`memory consolidated — removed ${r.removed}, kept ${r.kept}`);
      await refreshMemory();
    } catch (e) { pushError("consolidate: " + e.message); }
  };
  renderRecalled();
  refreshMemory();
}

async function refreshMemory() {
  try {
    const { entries } = await api("/api/memory" + (state.activeProjectId ? "?projectId=" + encodeURIComponent(state.activeProjectId) : ""));
    state.memory = entries || [];
    renderMemory();
  } catch {}
}

function renderMemory() {
  const box = document.querySelector('.panel[data-panel="memory"] #memory-list') || $("memory-list");
  if (!box) return;
  box.innerHTML = "";
  if (!state.memory.length) { box.appendChild(el("span", "dim mono", "no memories yet — they're captured automatically")); return; }
  state.memory.forEach((m) => {
    const entry = el("div", "mem-entry");
    const head = el("div", "mem-entry-head");
    if (m.category) head.appendChild(el("span", "mem-cat", m.category));
    head.appendChild(el("span", "mem-scope" + (m.scope === "global" ? " global" : ""), m.scope));
    entry.appendChild(head);
    entry.appendChild(el("div", "mem-value", truncate(m.memory || "", 200)));
    const actions = el("div", "mem-actions");
    const editBtn = el("button", null, "EDIT");
    editBtn.onclick = () => inlineEditMemory(entry, m);
    const delBtn = el("button", "del", "DEL");
    delBtn.onclick = async (ev) => {
      ev.stopPropagation();
      try { await del("/api/memory/" + m.id + (state.activeProjectId ? "?projectId=" + encodeURIComponent(state.activeProjectId) : "")); await refreshMemory(); }
      catch (e) { pushError("memory delete: " + e.message); }
    };
    actions.appendChild(editBtn); actions.appendChild(delBtn);
    entry.appendChild(actions);
    box.appendChild(entry);
  });
}

function inlineEditMemory(entry, m) {
  entry.innerHTML = "";
  const row = el("div", "mem-edit-row");
  const txt = el("textarea"); txt.value = m.memory || ""; txt.placeholder = "fact";
  const actions = el("div", "pf-actions");
  const save = el("button", "btn-mini btn-go", "SAVE");
  const cancel = el("button", "btn-mini", "CANCEL");
  save.onclick = async () => {
    try { await patch("/api/memory/" + m.id, { text: txt.value.trim(), projectId: state.activeProjectId || "" }); await refreshMemory(); }
    catch (e) { pushError("memory edit: " + e.message); }
  };
  cancel.onclick = () => refreshMemory();
  actions.appendChild(save); actions.appendChild(cancel);
  row.appendChild(txt); row.appendChild(actions);
  entry.appendChild(row);
}

/* Recall observability — which memories were auto-injected for the current task. */
function handleMemoryRecall(m) {
  state.recalled = m.items || [];
  renderRecalled();
}

function renderRecalled() {
  const box = document.querySelector('.panel[data-panel="memory"] #mem-recalled') || $("mem-recalled");
  if (!box) return;
  const items = state.recalled || [];
  if (!items.length) { box.classList.add("hidden"); box.innerHTML = ""; return; }
  box.classList.remove("hidden");
  box.innerHTML = "";
  box.appendChild(el("div", "mem-recalled-title", "↳ recalled for this task"));
  items.forEach((it) => {
    const r = el("div", "mem-recalled-item");
    r.appendChild(el("span", "mem-recalled-score", (it.score ?? 0).toFixed(2)));
    r.appendChild(el("span", "mem-recalled-text", truncate(it.memory || "", 120)));
    box.appendChild(r);
  });
}

/* ---------- skills ---------- */
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
  const panel = document.querySelector('.panel[data-panel="skills"]');
  const list = panel ? panel.querySelector("#skills-list") : $("skills-list");
  const count = panel ? panel.querySelector("#skills-count") : $("skills-count");
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
  const panel = document.querySelector('.panel[data-panel="skills"]');
  const detail = panel ? panel.querySelector("#skills-detail") : $("skills-detail");
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

/* ---------- files ---------- */
async function loadProjectFiles() {
  if (!state.activeProjectId) return;
  try {
    const { tree } = await api(`/api/projects/${state.activeProjectId}/files`);
    state.projectFiles = flattenFileTree(tree);
    renderFiles();
  } catch (e) { /* fail silently */ }
}

function flattenFileTree(tree, prefix = "") {
  let out = [];
  (tree || []).forEach((n) => {
    const p = prefix ? prefix + "/" + n.path.split(/[\\/]/).pop() : n.path.split(/[\\/]/).pop();
    out.push({ path: p, type: n.type });
    if (n.children) out = out.concat(flattenFileTree(n.children, p));
  });
  return out;
}

function wireFilesPanel(node) {
  renderFiles();
}

function renderFiles() {
  const panel = document.querySelector('.panel[data-panel="files"]');
  const treeEl = panel ? panel.querySelector("#files-tree") : $("files-tree");
  if (!treeEl) return;
  if (!state.activeProjectId) {
    treeEl.innerHTML = "";
    treeEl.appendChild(el("div", "dim mono", "no project open"));
    return;
  }
  loadProjectFiles().then(() => {
    const build = (nodes) => {
      const wrap = el("div", "ft-children");
      (nodes || []).forEach((n) => {
        const isDir = n.type === "dir";
        const row = el("div", "ft-row" + (isDir ? " ft-dir" : ""));
        row.appendChild(el("span", "ft-icon", isDir ? "▾" : "▹"));
        const name = n.path.split(/[\\/]/).pop();
        row.appendChild(el("span", "ft-name", name));
        wrap.appendChild(row);
        if (isDir && n.children && n.children.length) {
          const childWrap = build(n.children);
          childWrap.style.display = "";
          row.onclick = () => {
            const hidden = childWrap.style.display === "none";
            childWrap.style.display = hidden ? "" : "none";
            row.querySelector(".ft-icon").textContent = hidden ? "▾" : "▸";
          };
          wrap.appendChild(childWrap);
        }
      });
      return wrap;
    };
    api(`/api/projects/${state.activeProjectId}/files`).then(({ tree }) => {
      treeEl.innerHTML = "";
      if (!tree.length) { treeEl.appendChild(el("div", "dim mono", "empty directory")); return; }
      const root = build(tree);
      root.className = "files-tree";
      treeEl.appendChild(root);
    }).catch(() => { treeEl.innerHTML = ""; treeEl.appendChild(el("div", "dim mono", "failed to load files")); });
  });
}

/* ---------- workflows ---------- */
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
        if (event.sandboxRunId !== undefined) step.sandboxRunId = event.sandboxRunId;
        if (event.browserSessionId !== undefined) step.browserSessionId = event.browserSessionId;
        if (event.toolCallIds !== undefined) step.toolCallIds = event.toolCallIds;
        if (event.thinking !== undefined) step.thinking = event.thinking;
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
    const scale = e.deltaY > 0 ? 1.12 : 0.88;
    state.wfView.w *= scale; state.wfView.h *= scale;
    applyViewBox();
  });
  node.querySelector("#wf-fit").onclick = fitGraph;
  node.querySelector("#wf-reset").onclick = resetGraph;
  refreshWorkflowGraph();
}

function applyViewBox() {
  const svg = document.querySelector('.panel[data-panel="graph"] #wf-svg') || $("wf-svg");
  if (svg) svg.setAttribute("viewBox", `${state.wfView.x} ${state.wfView.y} ${state.wfView.w} ${state.wfView.h}`);
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
  applyViewBox();
}

function resetGraph() {
  state.wfView = { x: 0, y: 0, w: 800, h: 600 };
  applyViewBox();
}

function refreshWorkflowGraph() {
  const panel = document.querySelector('.panel[data-panel="graph"]');
  if (!panel) return;
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
  if (statusEl) statusEl.textContent = `${run.steps.length} steps · ${run.status}`;
  renderWorkflowDag(run, panel);
}

function renderWorkflowDag(run, panel) {
  const nodesG = panel.querySelector("#wf-nodes");
  const edgesG = panel.querySelector("#wf-edges");
  const layers = computeLayers(run.steps);
  const positions = {};
  const NODE_W = 160, NODE_H = 58, LAYER_GAP = 190, NODE_GAP = 24;
  const DOTZ_Y = 24, DOTZ_DROP = 150;
  layers.forEach((layer, i) => {
    const layerWidth = layer.length * (NODE_W + NODE_GAP) - NODE_GAP;
    const startX = (state.wfView.w - layerWidth) / 2;
    layer.forEach((stepId, j) => {
      positions[stepId] = { x: startX + j * (NODE_W + NODE_GAP), y: DOTZ_Y + DOTZ_DROP + i * LAYER_GAP };
    });
  });
  // The main dotz agent (lead orchestrator) sits above the whole graph; every ROOT step (a subagent
  // it dispersed) hangs off it, so the fan-out reads as "dotz → reviewers" with real connector lines.
  const dotzPos = { x: state.wfView.w / 2 - NODE_W / 2, y: DOTZ_Y };
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
    const g = document.createElementNS("http://www.w3.org/2000/svg", "g");
    g.setAttribute("class", "wf-node " + step.status);
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

    g.onclick = () => showNodeDetail(run, step);
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
  $("node-detail-close").onclick = () => $("node-detail").classList.add("hidden");
}

function showNodeDetail(run, step) {
  const drawer = $("node-detail");
  const body = $("node-detail-body");
  drawer.classList.remove("hidden");
  body.innerHTML = "";
  body.appendChild(makeNdRow("AGENT", step.agent));
  body.appendChild(makeNdRow("TASK", step.task));
  body.appendChild(makeNdRow("STATUS", step.status));
  if (step.output) {
    body.appendChild(makeNdRow("OUTPUT", ""));
    body.appendChild(el("div", "nd-block", step.output));
  }
  if (step.error) {
    body.appendChild(makeNdRow("ERROR", ""));
    body.appendChild(el("div", "nd-block red", step.error));
  }
  if (step.usage) body.appendChild(makeNdRow("USAGE", JSON.stringify(step.usage)));
  if (step.thinking) {
    body.appendChild(makeNdRow("THINKING", ""));
    body.appendChild(el("div", "nd-block", step.thinking));
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
  if (step.toolCallIds && step.toolCallIds.length) {
    body.appendChild(makeNdRow("TOOL CALLS", ""));
    step.toolCallIds.forEach((id) => body.appendChild(el("div", "nd-block", id)));
  }
  if (step.startedAt) body.appendChild(makeNdRow("STARTED", new Date(step.startedAt).toLocaleTimeString()));
  if (step.endedAt) body.appendChild(makeNdRow("ENDED", new Date(step.endedAt).toLocaleTimeString()));
}

function makeNdRow(label, val) {
  const row = el("div", "nd-row");
  row.appendChild(el("span", "label", label + ": "));
  row.appendChild(el("span", "val", val));
  return row;
}

/* ---------- human gate ---------- */
function bindGateCard() {
  $("gate-approve").onclick = () => resolveGate(true);
  $("gate-reject").onclick = () => resolveGate(false);
}

function showGateCard(gateId, plan) {
  state.pendingGate = gateId;
  const card = $("gate-card");
  $("gate-plan").textContent = plan || "(no plan provided)";
  $("gate-feedback").value = "";
  card.classList.remove("hidden");
  $("gate-approve").focus();
}

function resolveGate(approved) {
  if (!state.pendingGate || !state.ws || state.ws.readyState !== WebSocket.OPEN) return;
  const feedback = $("gate-feedback").value.trim();
  state.ws.send(JSON.stringify({ kind: approved ? "gate.approve" : "gate.reject", gateId: state.pendingGate, feedback }));
  $("gate-card").classList.add("hidden");
  state.pendingGate = null;
}

/* ---------- settings + updater ---------- */
function bindSettings() {
  $("settings-btn").onclick = () => {
    $("settings-card").classList.remove("hidden");
    $("settings-version").textContent = (window.dotz && window.dotz.version) || "browser";
    $("settings-feed").textContent = (window.dotz && window.dotz.electron) ? "configured" : "not configured (browser/dev)";
    $("settings-update-status").textContent = state.updateStatus || "—";
    $("settings-close").focus();
  };
  $("settings-close").onclick = () => $("settings-card").classList.add("hidden");
  $("settings-check-update").onclick = () => {
    if (window.dotz && window.dotz.update && window.dotz.update.check) {
      window.dotz.update.check();
      $("settings-update-status").textContent = "checking…";
    } else {
      $("settings-update-status").textContent = "updates only available in packaged app with feed";
    }
  };
}

function bindUpdateCard() {
  // Source-rebuild updater: APPLY = git pull + portable rebuild + relaunch (out of process).
  $("update-apply").onclick = () => {
    if (window.dotz && window.dotz.update && window.dotz.update.apply) {
      window.dotz.update.apply();
      state.updateStatus = "updating…";
      $("settings-update-status").textContent = state.updateStatus;
      $("update-actions").classList.add("hidden");
      $("update-body").textContent = "Pulling, rebuilding the portable exe, and relaunching. dotz will close shortly…";
    }
  };
  $("update-later").onclick = () => {
    $("update-card").classList.add("hidden");
    state.updateStatus = "update deferred";
    $("settings-update-status").textContent = state.updateStatus;
  };
  $("update-settings").onclick = () => {
    $("update-card").classList.add("hidden");
    $("settings-card").classList.remove("hidden");
  };
}

function wireUpdaterIpc() {
  if (!window.dotz || !window.dotz.update || !window.dotz.update.onStatus) return;
  window.dotz.update.onStatus((status, data) => {
    state.updateStatus = status;
    if (status === "available") {
      const n = data.behind || 0;
      const plural = n === 1 ? "commit" : "commits";
      const dirtyWarn = data.dirty
        ? ` <strong>Note:</strong> the local source tree has uncommitted changes — commit or stash them first or the update will be blocked.`
        : "";
      $("update-body").innerHTML =
        `dotz is <strong>${n}</strong> ${plural} behind (${esc(data.localSha)} → ${esc(data.remoteSha)}). ` +
        `UPDATE & RESTART pulls, rebuilds the portable exe, and relaunches.${dirtyWarn}`;
      $("update-card").classList.remove("hidden");
      $("update-actions").classList.remove("hidden");
      $("update-progress").classList.add("hidden");
      $("update-apply").classList.toggle("hidden", !!data.dirty);
      $("update-later").textContent = "LATER";
      $("settings-update-status").textContent = `update available: ${n} ${plural} behind`;
      $("update-later").focus();
    } else if (status === "not-available") {
      $("settings-update-status").textContent = `up to date (${esc(data.localSha || "")})`;
    } else if (status === "applying") {
      $("update-actions").classList.add("hidden");
      $("update-body").textContent = "Pulling, rebuilding the portable exe, and relaunching. dotz will close shortly…";
      $("settings-update-status").textContent = "updating…";
    } else if (status === "failed") {
      $("update-body").textContent = "Update error: " + esc(data.message);
      $("update-card").classList.remove("hidden");
      $("update-actions").classList.remove("hidden");
      $("update-progress").classList.add("hidden");
      $("update-apply").classList.add("hidden");
      $("update-later").textContent = "CLOSE";
      $("settings-update-status").textContent = "error: " + esc(data.message);
      $("update-later").focus();
    }
  });
}

/* ---------- brain panel ---------- */
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
  const log = document.querySelector('.panel[data-panel="brain"] #brain-log') || $("brain-log");
  if (!log) return;
  const entry = el("div", "brain-log-entry", new Date().toLocaleTimeString() + " " + msg);
  log.appendChild(entry);
  log.scrollTop = log.scrollHeight;
}

/* ---------- browser panel ---------- */
function wireBrowserPanel(node) {
  const url = node.querySelector("#br-url");
  const shot = node.querySelector("#br-shot");
  const typeText = node.querySelector("#br-type-text");
  const runAct = async (input) => {
    if (!state.browserSessionId || state.browserBusy) return;
    state.browserBusy = true;
    try {
      const observation = await browserAction("act", { sessionId: state.browserSessionId, ...input });
      renderBrowserObservation(observation);
      return observation;
    } catch (e) { pushError("browser action: " + e.message); }
    finally { state.browserBusy = false; }
  };
  const startOrNavigate = async () => {
    const target = url.value.trim();
    if (!target || !state.activeProjectId) return pushError("browser: open a project and enter an http(s) URL");
    try {
      if (state.browserSessionId) {
        await runAct({ action: "navigate", url: target });
      } else {
        const origin = new URL(target).origin;
        state.browserBusy = true;
        const observation = await browserAction("start", { projectId: state.activeProjectId, url: target, allowedOrigins: [origin] });
        renderBrowserObservation(observation);
      }
    } catch (e) { pushError("browser start: " + e.message); }
    finally { state.browserBusy = false; }
  };
  node.querySelector("#br-navigate").onclick = startOrNavigate;
  node.querySelector("#br-back").onclick = () => runAct({ action: "back" });
  node.querySelector("#br-forward").onclick = () => runAct({ action: "forward" });
  node.querySelector("#br-reload").onclick = () => runAct({ action: "reload" });
  node.querySelector("#br-shot-btn").onclick = () => runAct({ action: "observe" });
  node.querySelector("#br-stop").onclick = async () => {
    if (!state.browserSessionId) return;
    try {
      state.browserBusy = true;
      renderBrowserObservation(await browserAction("stop", { sessionId: state.browserSessionId }));
    }
    catch (e) { pushError("browser stop: " + e.message); }
    finally { state.browserBusy = false; }
  };

  const sendType = () => {
    const text = typeText.value;
    if (!text || !state.browserObservation) return;
    typeText.value = "";
    runAct({ action: "type", text, expectedSeq: state.browserObservation.seq });
  };
  node.querySelector("#br-type-send").onclick = sendType;
  typeText.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); sendType(); } });
  node.querySelector("#br-scroll-up").onclick = () => runAct({ action: "scroll", direction: "up", pixels: 500 });
  node.querySelector("#br-scroll-down").onclick = () => runAct({ action: "scroll", direction: "down", pixels: 500 });

  shot.addEventListener("click", (e) => {
    if (!state.browserObservation || state.browserObservation.status !== "ready") return;
    const rect = shot.getBoundingClientRect();
    const viewport = state.browserObservation.page?.viewport;
    if (!rect.width || !rect.height || !viewport?.width || !viewport?.height) return;
    const x = Math.round((e.clientX - rect.left) / rect.width * viewport.width);
    const y = Math.round((e.clientY - rect.top) / rect.height * viewport.height);
    shot.focus();
    runAct({ action: "clickAt", x, y, expectedSeq: state.browserObservation.seq });
  });
  shot.addEventListener("keydown", (e) => {
    if (!state.browserObservation || !["Enter", "Tab", "Escape", "Backspace", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"].includes(e.key)) return;
    e.preventDefault();
    runAct({ action: "key", key: e.key });
  });
  url.addEventListener("keydown", (e) => { if (e.key === "Enter") startOrNavigate(); });
  if (state.browserPollTimer) clearInterval(state.browserPollTimer);
  state.browserPollTimer = setInterval(refreshBrowserScreenshot, 1200);
  refreshBrowserScreenshot();
}

async function browserAction(action, body) {
  const path = "/api/browser/" + action;
  const opts = { method: body ? "POST" : "GET", headers: {} };
  if (body) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
  return api(path, opts);
}

async function refreshBrowserScreenshot() {
  const panel = document.querySelector('.panel[data-panel="browser"]');
  if (!panel || state.browserBusy) return;
  try {
    const query = state.browserSessionId ? `?sessionId=${encodeURIComponent(state.browserSessionId)}` : "";
    const result = await browserAction("state" + query);
    renderBrowserObservation(result.observation);
  } catch (e) { pushError("browser: " + e.message); }
}

function renderBrowserObservation(observation) {
  const panel = document.querySelector('.panel[data-panel="browser"]');
  if (!panel) return;
  const shot = panel.querySelector("#br-shot");
  const ph = panel.querySelector("#br-placeholder");
  if (!observation) {
    state.browserSessionId = null;
    state.browserObservation = null;
    shot.classList.add("hidden"); ph.classList.remove("hidden");
    ph.textContent = "waiting for a Pi browser session";
    return;
  }
  state.browserSessionId = observation.status === "stopped" ? null : observation.sessionId;
  state.browserObservation = observation.status === "stopped" ? null : observation;
  panel.querySelector("#br-url").value = observation.page?.url || "";
  panel.querySelector("#br-navigate").textContent = state.browserSessionId ? "GO" : "START";
  for (const id of ["br-back", "br-forward", "br-reload", "br-shot-btn", "br-stop", "br-scroll-up", "br-scroll-down", "br-type-text", "br-type-send"]) {
    const control = panel.querySelector("#" + id);
    if (control) control.disabled = !state.browserSessionId;
  }
  const action = observation.currentAction;
  const owner = observation.owner || {};
  ph.textContent = `${observation.status} | ${owner.projectId || "unowned"} | seq ${observation.seq}`;
  if (observation.frame?.available && observation.status !== "stopped") {
    shot.src = `/api/browser/frame?sessionId=${encodeURIComponent(observation.sessionId)}&afterSeq=${Math.max(-1, observation.frame.seq - 1)}&t=${observation.frame.seq}`;
    shot.classList.remove("hidden"); ph.classList.add("hidden");
  } else { shot.classList.add("hidden"); ph.classList.remove("hidden"); }
  let details = panel.querySelector(".browser-observation");
  if (!details) { details = el("div", "browser-observation mono"); panel.querySelector(".browser-body").appendChild(details); }
  const errors = [...(observation.consoleErrors || []), ...(observation.networkErrors || [])];
  const refs = (observation.elements || []).slice(0, 10).map((item) => `@${item.ref} ${item.role} ${item.name}`).join("\n");
  details.textContent = [
    `owner ${owner.projectId || "-"} / workflow ${owner.workflowId || "-"}`,
    `viewport ${observation.page?.viewport?.width || 0}x${observation.page?.viewport?.height || 0} | recording off`,
    `active ${action ? `${action.name} ${action.targetRef || ""}` : "observe"}`,
    `console ${observation.counters?.consoleErrors || 0} | network ${observation.counters?.networkErrors || 0}`,
    errors.slice(-3).join(" | "), refs,
  ].filter(Boolean).join("\n");
  let cursor = panel.querySelector(".browser-agent-cursor");
  const viewport = observation.page?.viewport;
  if (observation.cursor && shot.clientWidth && viewport?.width && viewport?.height) {
    if (!cursor) { cursor = el("span", "browser-agent-cursor"); panel.querySelector(".browser-shot-host").appendChild(cursor); }
    cursor.style.left = `${Math.max(0, Math.min(100, observation.cursor.x / viewport.width * 100))}%`;
    cursor.style.top = `${Math.max(0, Math.min(100, observation.cursor.y / viewport.height * 100))}%`;
  } else if (cursor) cursor.remove();
}

/* ---------- sandbox ---------- */
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
      if (rec) { rec.run = e.run; rec.status = e.run.status; }
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
