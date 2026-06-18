/* dotz chat UI — wires the cyberbrutalist front end to the dotz backend (REST + WebSocket).
 * Handles: profiles, sessions, projects, memory, multi-provider models, sandbox (terminal +
 * web preview with agent cursor overlay), chat streaming, tool cards, thinking, controls. */
"use strict";

const THINK_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh"];
const state = {
  sessionId: null,
  summary: null,
  ws: null,
  streaming: false,
  cur: null, // current assistant render: { bubble, partial }
  toolCards: {}, // toolCallId -> standalone card refs
  profiles: [], // dotz operating profiles
  activeProfileId: "workflow", // profile for the next/new session
  // projects
  projects: [],
  activeProjectId: null,
  // models / providers
  providers: [],
  activeProvider: "openrouter",
  modelCatalog: [], // available models for the active session
  // memory
  memory: [],
  // sandbox
  tab: "chat", // "chat" | "sandbox"
  sandbox: {
    languages: [],
    runs: new Map(), // runId -> { run, output, port, cursor }
    activeRunId: null,
    mode: "terminal",
  },
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
  bindComposer();
  bindTabs();
  bindProjects();
  bindMemory();
  bindSandbox();
  $("new-session").onclick = () => newSession().catch((e) => pushError(e.message));
  await loadProfiles();
  await loadProviders();
  await loadProjects();
  await loadSandboxLanguages();
  await newSession();
}

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
    b.title = p.tagline;
    b.dataset.id = p.id;
    b.onclick = async () => {
      if (p.id === state.activeProfileId) return;
      state.activeProfileId = p.id;
      // Switching profile starts a fresh session with that doctrine.
      await newSession().catch((e) => pushError("profile switch: " + e.message));
    };
    box.appendChild(b);
  });
}

async function loadProviders() {
  try {
    const { providers } = await api("/api/providers");
    state.providers = providers || [];
  } catch { state.providers = []; }
}

async function newSession() {
  const body = { profileId: state.activeProfileId };
  if (state.activeProjectId) body.projectId = state.activeProjectId;
  const s = await post("/api/sessions", { profileId: body.profileId, projectId: body.projectId });
  setSession(s);
  clearTranscript();
  connectWS();
  await Promise.all([loadModels(), loadTools(), loadCommands(), refreshMemory()]);
  await refreshSessions();
  await refreshStats();
  refreshSandboxCount();
}

function setSession(s) {
  state.summary = s;
  state.sessionId = s.sessionId;
  if (s.profileId) {
    state.activeProfileId = s.profileId;
    renderProfilePicker();
  }
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
    else if (m.kind === "error") pushError(m.error);
  };
}

function setConn(kind, txt) {
  const c = $("conn-chip");
  c.className = "chip chip-" + kind;
  const label = kind === "on" ? "PI ONLINE" : kind === "err" ? "WS ERROR" : "OFFLINE";
  c.innerHTML = '<span class="blk">█</span> ' + label;
  $("ws-status").textContent = txt;
}

// ---------- event handling (agent) ----------
function handleEvent(e) {
  switch (e.type) {
    case "agent_start": setStreaming(true); break;
    case "message_start":
      if (e.message && e.message.role === "assistant") state.cur = null;
      break;
    case "message_update":
      if (e.assistantMessageEvent && e.assistantMessageEvent.partial) renderAssistantPartial(e.assistantMessageEvent.partial);
      break;
    case "message_end":
      if (e.message && e.message.role === "assistant") finalizeAssistant(e.message);
      break;
    case "tool_execution_start": toolCard(e.toolCallId, { name: e.toolName, args: e.args, status: "run" }); break;
    case "tool_execution_update": toolCard(e.toolCallId, { output: stringifyResult(e.partialResult) }); break;
    case "tool_execution_end": toolCard(e.toolCallId, { status: e.isError ? "err" : "done", output: stringifyResult(e.result), name: e.toolName }); break;
    case "agent_end": setStreaming(false); refreshStats(); break;
  }
  scrollBottom();
}

function stringifyResult(r) {
  if (r == null) return "";
  if (typeof r === "string") return r;
  if (r.content && Array.isArray(r.content)) return r.content.map((c) => c.text || "").join("\n");
  return JSON.stringify(r);
}

// ---------- transcript rendering ----------
function clearTranscript() { $("transcript").innerHTML = ""; state.cur = null; state.toolCards = {}; }
function scrollBottom() { const t = $("transcript"); t.scrollTop = t.scrollHeight; }

function renderUserMessage(message) {
  const text = (message.content || []).map((c) => c.text || "").join("");
  const m = el("div", "msg user");
  m.appendChild(el("div", "msg-role", "you"));
  m.appendChild(el("div", "bubble", text));
  $("transcript").appendChild(m);
  scrollBottom();
}

function ensureAssistantBubble() {
  if (state.cur && state.cur.bubble.isConnected) return state.cur;
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "dotz"));
  const bubble = el("div", "bubble");
  m.appendChild(bubble);
  $("transcript").appendChild(m);
  state.cur = { bubble, partial: { content: [] } };
  return state.cur;
}

function hasRenderable(partial) {
  return (partial.content || []).some(
    (b) => (b.type === "text" && b.text) || (b.type === "thinking" && b.thinking && b.thinking.trim())
  );
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
    if (message.stopReason === "error" && message.errorMessage) {
      cur.bubble.appendChild(el("div", "msg-error", "⚠ " + message.errorMessage));
    }
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
    head.appendChild(nameEl);
    head.appendChild(badge);
    const body = el("div", "toolcard-body");
    const argsEl = el("div", "toolcard-args");
    const outEl = el("div", "toolcard-out");
    outEl.style.display = "none";
    body.appendChild(argsEl);
    body.appendChild(outEl);
    card.appendChild(head);
    card.appendChild(body);
    $("transcript").appendChild(card);
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
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "system"));
  const b = el("div", "bubble");
  b.appendChild(el("div", "msg-error", "⚠ " + msg));
  m.appendChild(b);
  $("transcript").appendChild(m);
  scrollBottom();
}

// ---------- tabs (CHAT | SANDBOX) ----------
function bindTabs() {
  document.querySelectorAll(".center-tabs .tab").forEach((t) => {
    t.onclick = () => setTab(t.dataset.tab);
  });
}
function setTab(name) {
  state.tab = name;
  $("tab-chat").classList.toggle("active", name === "chat");
  $("tab-sandbox").classList.toggle("active", name === "sandbox");
  $("view-chat").classList.toggle("hidden", name !== "chat");
  $("view-sandbox").classList.toggle("hidden", name !== "sandbox");
  if (name === "sandbox") refreshSandboxView();
}

// ---------- composer ----------
function bindComposer() {
  const input = $("composer-input");
  input.addEventListener("input", autoGrow);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
  });
  $("send-btn").onclick = send;
  $("stop-btn").onclick = () => state.ws && state.ws.send(JSON.stringify({ kind: "abort" }));
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
  $("send-btn").classList.toggle("hidden", b);
  $("stop-btn").classList.toggle("hidden", !b);
  $("st-state").textContent = b ? "streaming…" : "idle";
}

function insertCommand(name) {
  const input = $("composer-input");
  input.value = ("/" + name + " ").replace("//", "/");
  input.focus();
  autoGrow();
}

// ---------- controls: model + providers ----------
async function loadModels() {
  const m = await api(`/api/sessions/${state.sessionId}/models`);
  state.modelCatalog = m.available || [];
  // provider dropdown
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
    state.modelCatalog
      .filter((x) => x.provider === state.activeProvider)
      .forEach((x) => {
        const o = el("option", null, (x.name || x.modelId) + "  ·  " + x.modelId);
        o.value = x.modelId;
        sel.appendChild(o);
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
  state.modelCatalog
    .filter((x) => x.provider === state.activeProvider)
    .slice(0, 400)
    .forEach((x) => { const o = el("option"); o.value = x.modelId; dl.appendChild(o); });
}

function updateModelUI(model) {
  const id = model ? model.modelId : "nex-agi/nex-n2-pro:free";
  const prov = model ? model.provider : "openrouter";
  const free = providerIsFreeForm(prov);
  if (free) $("model-input").value = id;
  else $("model-select").value = id;
  $("model-badge").textContent = prov + " / " + id;
  $("model-ctx").textContent = "ctx " + fmtCtx(model && model.contextWindow);
  $("model-reasoning").textContent = "reasoning " + (model && model.reasoning ? "✓" : "✗");
}
function fmtCtx(n) { return n ? (n >= 1000 ? Math.round(n / 1000) + "K" : "" + n) : "—"; }

async function submitModel() {
  const id = $("model-input").value.trim();
  if (!id) return;
  await setModel({ provider: state.activeProvider, modelId: id });
}
async function submitModelSelect() {
  const id = $("model-select").value;
  if (!id) return;
  await setModel({ provider: state.activeProvider, modelId: id });
}
async function setModel(ref) {
  try {
    const s = await post(`/api/sessions/${state.sessionId}/model`, ref);
    state.summary = s;
    updateModelUI(s.model);
    renderReasoning(s);
  } catch (e) { pushError("model: " + e.message); }
}

// ---------- controls: reasoning ----------
function renderReasoning(summary) {
  const seg = $("reasoning-seg");
  seg.innerHTML = "";
  const avail = summary.availableThinkingLevels || [];
  THINK_LEVELS.forEach((lvl) => {
    const b = el("button", null, lvl.toUpperCase());
    b.disabled = !summary.supportsThinking || !avail.includes(lvl);
    if (lvl === summary.thinkingLevel) b.classList.add("active");
    b.onclick = async () => {
      try {
        const r = await post(`/api/sessions/${state.sessionId}/thinking`, { level: lvl });
        state.summary = Object.assign({}, state.summary, r);
        renderReasoning(state.summary);
      } catch (e) { pushError("thinking: " + e.message); }
    };
    seg.appendChild(b);
  });
}

// ---------- controls: tools ----------
async function loadTools() {
  const t = await api(`/api/sessions/${state.sessionId}/tools`);
  renderTools(t);
}
function renderTools(t) {
  const grid = $("tools-grid");
  grid.innerHTML = "";
  t.all.forEach((name) => {
    const on = t.active.includes(name);
    const chip = el("div", "tool-chip" + (on ? " on" : ""));
    chip.appendChild(el("span", "bx"));
    chip.appendChild(el("span", null, name));
    chip.onclick = async () => {
      const next = on ? t.active.filter((x) => x !== name) : t.active.concat(name);
      try { renderTools(await post(`/api/sessions/${state.sessionId}/tools`, { tools: next })); }
      catch (e) { pushError("tools: " + e.message); }
    };
    grid.appendChild(chip);
  });
}

// ---------- controls: skills + subagents ----------
async function loadCommands() {
  const { commands } = await api(`/api/sessions/${state.sessionId}/commands`);
  const skills = commands.filter((c) => c.source === "skill");
  const agents = commands.filter((c) => c.source === "prompt" || c.source === "extension");
  renderCmdList($("skills-list"), skills);
  renderCmdList($("subagents-list"), agents);
  const chips = $("quick-chips");
  chips.innerHTML = "";
  agents.concat(skills).slice(0, 4).forEach((c) => {
    const chip = el("span", "qchip", "/" + c.name);
    chip.onclick = () => insertCommand(c.name);
    chips.appendChild(chip);
  });
}
function renderCmdList(container, list) {
  container.innerHTML = "";
  if (!list.length) { container.appendChild(el("span", "dim mono", "none loaded")); return; }
  list.forEach((c) => {
    const item = el("div", "cmd-item");
    item.title = c.description || "";
    item.appendChild(el("span", "cmd-run", "▶"));
    item.appendChild(el("span", "cmd-name", "/" + c.name));
    item.appendChild(el("span", "cmd-src", c.source));
    item.onclick = () => insertCommand(c.name);
    container.appendChild(item);
  });
}

// ---------- projects ----------
function bindProjects() {
  $("new-project").onclick = () => {
    const f = $("project-form");
    f.classList.toggle("hidden");
    if (!f.classList.contains("hidden")) {
      $("pf-name").value = "";
      $("pf-cwd").value = "";
      $("pf-model").value = "nex-agi/nex-n2-pro:free";
      const sel = $("pf-profile");
      sel.innerHTML = "";
      state.profiles.forEach((p) => { const o = el("option", null, p.name); o.value = p.id; sel.appendChild(o); });
      sel.value = state.activeProfileId;
      $("pf-name").focus();
    }
  };
  $("pf-cancel").onclick = () => $("project-form").classList.add("hidden");
  $("pf-create").onclick = async () => {
    const name = $("pf-name").value.trim();
    const cwd = $("pf-cwd").value.trim();
    if (!name || !cwd) { pushError("project needs name + cwd"); return; }
    const body = { name, cwd, profileId: $("pf-profile").value, model: { provider: "openrouter", modelId: $("pf-model").value.trim() || "nex-agi/nex-n2-pro:free" } };
    try {
      await post("/api/projects", body);
      $("project-form").classList.add("hidden");
      await loadProjects();
    } catch (e) { pushError("create project: " + e.message); }
  };
}

async function loadProjects() {
  try {
    const { projects } = await api("/api/projects");
    state.projects = projects || [];
    renderProjects();
  } catch (e) { pushError("projects: " + e.message); }
}

function renderProjects() {
  const box = $("project-list");
  box.innerHTML = "";
  if (!state.projects.length) {
    box.appendChild(el("div", "dim mono", "no projects · click + NEW"));
    return;
  }
  state.projects.forEach((p) => {
    const card = el("div", "project-card" + (p.id === state.activeProjectId ? " active" : ""));
    const name = el("div", "pc-name");
    name.appendChild(el("span", "pc-dot"));
    name.appendChild(el("span", null, p.name));
    card.appendChild(name);
    card.appendChild(el("div", "pc-cwd", p.cwd));
    const actions = el("div", "pc-actions");
    const delBtn = el("button", null, "DEL");
    delBtn.onclick = async (ev) => {
      ev.stopPropagation();
      if (!confirm("delete project " + p.name + "?")) return;
      try { await del("/api/projects/" + p.id); if (state.activeProjectId === p.id) state.activeProjectId = null; await loadProjects(); }
      catch (e) { pushError("delete project: " + e.message); }
    };
    actions.appendChild(delBtn);
    card.appendChild(actions);
    card.onclick = () => activateProject(p.id);
    box.appendChild(card);
  });
}

async function activateProject(id) {
  state.activeProjectId = id;
  renderProjects();
  // Starting a session bound to the project applies its cwd/profile/model/memory.
  await newSession().catch((e) => pushError("project session: " + e.message));
}

// ---------- memory ----------
function bindMemory() {
  $("mem-add").onclick = () => {
    const f = $("mem-form");
    f.classList.toggle("hidden");
    if (!f.classList.contains("hidden")) { $("mem-key").value = ""; $("mem-value").value = ""; $("mem-key").focus(); }
  };
  $("mem-cancel").onclick = () => $("mem-form").classList.add("hidden");
  $("mem-create").onclick = async () => {
    const key = $("mem-key").value.trim();
    const value = $("mem-value").value;
    if (!key) { pushError("memory key required"); return; }
    try {
      await post("/api/memory", { projectId: state.activeProjectId || "", key, value, scope: $("mem-scope").value });
      $("mem-form").classList.add("hidden");
      await refreshMemory();
    } catch (e) { pushError("memory add: " + e.message); }
  };
}

async function refreshMemory() {
  try {
    const { entries } = await api("/api/memory" + (state.activeProjectId ? "?projectId=" + encodeURIComponent(state.activeProjectId) : ""));
    state.memory = entries || [];
    renderMemory();
  } catch (e) { pushError("memory load: " + e.message); }
}

function renderMemory() {
  const box = $("memory-list");
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
    delBtn.onclick = async (ev) => {
      ev.stopPropagation();
      try { await del("/api/memory/" + m.id); await refreshMemory(); }
      catch (e) { pushError("memory delete: " + e.message); }
    };
    actions.appendChild(editBtn);
    actions.appendChild(delBtn);
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
  save.onclick = async () => {
    try {
      await patch("/api/memory/" + m.id, { key: keyIn.value.trim(), value: valIn.value, scope: scopeSel.value });
      await refreshMemory();
    } catch (e) { pushError("memory edit: " + e.message); }
  };
  cancel.onclick = () => refreshMemory();
  actions.appendChild(save);
  actions.appendChild(cancel);
  row.appendChild(keyIn);
  row.appendChild(valIn);
  row.appendChild(scopeSel);
  row.appendChild(actions);
  entry.appendChild(row);
}

// ---------- sessions ----------
async function refreshSessions() {
  const list = await api(`/api/sessions`);
  const box = $("session-list");
  box.innerHTML = "";
  const projName = (id) => { const p = state.projects.find((x) => x.id === id); return p ? p.name : null; };
  list.forEach((s) => {
    const card = el("div", "session-card" + (s.sessionId === state.sessionId ? " active" : ""));
    card.appendChild(el("div", "sc-name", "session " + s.sessionId.slice(0, 8)));
    const prof = s.profileId ? s.profileId.toUpperCase() + " · " : "";
    const model = s.model ? s.model.modelId : "—";
    card.appendChild(el("div", "sc-meta", prof + model + " · " + s.thinkingLevel));
    const pn = projName(s.projectId);
    if (pn) card.appendChild(el("div", "sc-proj", "▸ " + pn));
    card.onclick = () => { if (s.sessionId !== state.sessionId) switchSession(s); };
    box.appendChild(card);
  });
}

async function switchSession(s) {
  setSession(s);
  clearTranscript();
  connectWS();
  await Promise.all([loadModels(), loadTools(), loadCommands(), refreshMemory()]);
  await refreshSessions();
  await refreshStats();
}

// ---------- sandbox ----------
function bindSandbox() {
  $("sb-run").onclick = () => runSandbox();
  $("sb-kill").onclick = () => killActiveRun();
  $("sb-mode").onchange = (e) => {
    state.sandbox.mode = e.target.value;
    toggleWebPreview(e.target.value === "web");
  };
}

async function loadSandboxLanguages() {
  try {
    const { languages } = await api("/api/sandbox/languages");
    state.sandbox.languages = languages || [];
    const sel = $("sb-lang");
    sel.innerHTML = "";
    state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; sel.appendChild(o); });
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
  setTab("sandbox");
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
      $("sb-runid").textContent = "#" + run.id.slice(0, 8);
      $("sb-status").textContent = "running " + run.language;
      $("sb-kill").classList.remove("hidden");
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
        $("sb-status").textContent = "ended: " + e.run.status + (e.run.exitCode != null ? " (exit " + e.run.exitCode + ")" : "");
        $("sb-kill").classList.add("hidden");
      }
      renderSandboxRuns();
      refreshSandboxCount();
      break;
    }
  }
}

function appendSandboxOutput(line, stream) {
  const out = $("sb-output");
  const div = el("div", stream === "stderr" ? "sb-line-stderr" : "");
  div.textContent = line;
  out.appendChild(div);
  out.scrollTop = out.scrollHeight;
}

function toggleWebPreview(web) {
  $("sb-web-wrap").classList.toggle("hidden", !web);
  $("sb-output-wrap").style.flex = web ? "1 1 30%" : "1 1 100%";
  if (!web) {
    $("sb-iframe").src = "about:blank";
    $("sb-cursor").classList.add("hidden");
  }
}

function loadWebPreview(port) {
  $("sb-port-label").textContent = "→ http://127.0.0.1:" + port;
  $("sb-iframe").src = "http://127.0.0.1:" + port + "/";
  // surface a port-detected line in the output too
  const out = $("sb-output");
  const div = el("div", "sb-line-port");
  div.textContent = "[dotz] web server detected on port " + port;
  out.appendChild(div);
  out.scrollTop = out.scrollHeight;
}

function renderCursor(x, y, action, text) {
  const cur = $("sb-cursor");
  cur.classList.remove("hidden");
  cur.style.left = x + "px";
  cur.style.top = y + "px";
  cur.classList.remove("click", "type");
  if (action === "click") cur.classList.add("click");
  if (action === "type") cur.classList.add("type");
  $("sb-cursor-label").textContent = action + (text ? ": " + truncate(text, 24) : "");
  dispatchCursorIntoIframe(x, y, action, text);
}

function dispatchCursorIntoIframe(x, y, action, text) {
  const iframe = $("sb-iframe");
  let doc;
  try { doc = iframe.contentDocument || iframe.contentWindow.document; } catch { return; }
  if (!doc) return;
  if (action === "click") {
    const target = doc.elementFromPoint(x, y);
    if (target) {
      try {
        const ev = new doc.defaultView.MouseEvent("click", { bubbles: true, cancelable: true, clientX: x, clientY: y });
        target.dispatchEvent(ev);
        if (typeof target.focus === "function") target.focus();
      } catch { /* cross-origin or detached */ }
    }
  } else if (action === "type" && text) {
    const target = doc.elementFromPoint(x, y);
    if (target) {
      try {
        if (typeof target.focus === "function") target.focus();
        // dispatch keydown events for each char
        for (const ch of text) {
          const kd = new doc.defaultView.KeyboardEvent("keydown", { key: ch, bubbles: true });
          target.dispatchEvent(kd);
        }
        if (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable) {
          if (target.isContentEditable) { target.textContent = (target.textContent || "") + text; }
          else { target.value = (target.value || "") + text; }
          const input = new doc.defaultView.InputEvent("input", { bubbles: true, data: text });
          target.dispatchEvent(input);
        }
      } catch { /* */ }
    }
  }
}

function renderSandboxRuns() {
  const box = $("sb-runs");
  box.innerHTML = "";
  if (!state.sandbox.runs.size) {
    $("sandbox-tab-count").classList.add("hidden");
    return;
  }
  $("sandbox-tab-count").classList.remove("hidden");
  $("sandbox-tab-count").textContent = state.sandbox.runs.size + " runs";
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
  $("sb-output").innerHTML = "";
  (rec.output || "").split("\n").forEach((line) => { if (line) appendSandboxOutput(line, "stdout"); });
  $("sb-runid").textContent = "#" + rec.run.id.slice(0, 8);
  $("sb-status").textContent = rec.run.status + (rec.run.exitCode != null ? " (exit " + rec.run.exitCode + ")" : "");
  $("sb-kill").classList.toggle("hidden", rec.run.status !== "running");
  if (rec.port && state.sandbox.mode === "web") loadWebPreview(rec.port);
  else toggleWebPreview(state.sandbox.mode === "web");
}

function refreshSandboxView() {
  if (state.sandbox.activeRunId) {
    const rec = state.sandbox.runs.get(state.sandbox.activeRunId);
    if (rec) showRunOutput(rec);
  }
  renderSandboxRuns();
  toggleWebPreview(state.sandbox.mode === "web");
}

function refreshSandboxCount() {
  const n = state.sandbox.runs.size;
  $("st-sandbox").textContent = n + " run" + (n === 1 ? "" : "s");
}

// ---------- status bar ----------
async function refreshStats() {
  try {
    const s = await api(`/api/sessions/${state.sessionId}`);
    const st = s.stats || {};
    const tok = st.tokens || {};
    $("st-tok-in").textContent = "↑" + fmtNum(tok.input || 0);
    $("st-tok-out").textContent = "↓" + fmtNum(tok.output || 0);
    $("st-cost").textContent = "$" + (st.cost || 0).toFixed(4).replace(/0+$/, "0");
    const cu = st.contextUsage;
    if (cu) {
      const pct = (cu.percent * 100);
      $("st-ctx-pct").textContent = pct.toFixed(1) + "%";
      $("st-ctx-bar").style.width = Math.min(100, pct) + "%";
      $("st-ctx-win").textContent = "of " + fmtCtx(cu.contextWindow);
    }
  } catch {}
}
function fmtNum(n) { return n >= 1000 ? (n / 1000).toFixed(1) + "K" : "" + n; }