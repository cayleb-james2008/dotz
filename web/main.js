/* dotz ultra code mode — bento dashboard UI entry point.
 * Wires the minimal-agentic front end to the dotz backend (REST + WebSocket).
 * Handles: project launcher / command center, bento panels (chat/graph/brain/browser/memory/files/sandbox/skills),
 * drag-and-drop panel repositioning, on-the-fly SVG workflow graph, multi-provider models,
 * sandbox (terminal + web preview + agent cursor), in-app browser, file tree, chat streaming,
 * slash / skill / file command palette, tool cards, skills pool, human gate.
 * Vanilla JS, no framework, no build step. Runs identically in browser and Tauri WebView2.
 *
 * Split from app.js (C7 modularization). No behavior change — pure mechanical split into native
 * ESM modules. This module owns the init/boot sequence + the topbar/command-center/project/model
 * controls + settings/updater/telemetry + keyboard globals + the workflow-refresh glue.
 */
import { $, el, api, post, del, esc } from './api.js';
import { state, loadLayout, saveLayout, loadBrainFloat, saveBrainFloat, updateBrainFloat } from './state.js';
import {
  PANEL_NAMES, renderBento, mountPanel, unmountPanel, openPanel, togglePalette,
} from './panels.js';
import { connectWS } from './ws.js';
import {
  refreshProviderHealth, refreshWfCount, refreshSandboxCount, refreshStats, loadSandboxLanguages,
} from './handlers.js';
import { bindNodeDetail, refreshWorkflowGraph } from './graph.js';
import {
  bindComposer, clearTranscript, insertCommand, pushError, showToast,
  autoGrow, sendFrom, hideCmdPalette,
} from './chat.js';
import { bindKbPalette, loadKbCommands, toggleKbPalette, hideKbPalette } from './kb-palette.js';
import { loadProjectFiles } from './panels/files.js';
import { refreshMemory } from './panels/memory.js';
import { refreshSkills } from './panels/skills.js';
import { refreshTemplates } from './panels/templates.js';
import { bindGateCard, setConn } from './handlers.js';

// Thinking levels for the reasoning picker. Populated from GET /api/models so the picker is
// backend-driven; falls back to the hardcoded list only if the backend hasn't provided one.
const THINK_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh"];
let backendThinkingLevels = null;

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
  bindKbPalette();
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
  await loadGlobalModels();
  primeTopbarFromConfig();
  await refreshProviderHealth();
  await loadProjects();
  await loadSandboxLanguages();
  showCommandCenter();
  // Prime the command-palette catalog (cached in localStorage; refreshed here on each boot so a
  // new provider/thinking level lands without waiting for the first Ctrl+K).
  loadKbCommands();
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
  // Browse… opens the native File Explorer folder picker (Electron only) and fills cwd with the
  // chosen absolute path — so the user selects a real workspace instead of typing one.
  {
    const browseBtn = $("pf-browse");
    if (browseBtn && window.dotz && typeof window.dotz.pickDirectory === "function") {
      browseBtn.onclick = async () => {
        try {
          const dir = await window.dotz.pickDirectory();
          if (dir) $("pf-cwd").value = dir;
        } catch (e) { pushError("folder picker: " + e.message); }
      };
    } else if (browseBtn) {
      browseBtn.style.display = "none";   // no native picker outside the Electron app
    }
  }
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
      appUrl: (($("pf-appurl") && $("pf-appurl").value) || "").trim(),
      gateCommand: (($("pf-gate") && $("pf-gate").value) || "").trim(),
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
    // Distinguish a FAILED load from a genuinely empty list — otherwise a server/fetch error reads
    // as "you have no projects" and the user can't tell the difference.
    const failed = state.projectsError;
    const empty = el("div", "dropdown-empty");
    empty.appendChild(el("div", "de-glyph", failed ? "⚠" : "◆"));
    empty.appendChild(el("div", "de-title", failed ? "Couldn't load projects" : "No projects yet"));
    empty.appendChild(el("div", "de-sub dim", failed ? "Check the server, then reopen" : "Create one below ↓"));
    list.appendChild(empty);
  } else {
    state.projects.forEach((p) => {
      // A <div> row (not <button>) so the delete <button> can nest without invalid markup.
      const item = el("div", "project-item");
      item.setAttribute("role", "button");
      item.tabIndex = 0;
      item.appendChild(el("span", "pi-glyph", "◆"));
      const info = el("div", "pi-info");
      info.appendChild(el("div", "pi-name", p.name));
      info.appendChild(el("div", "pi-cwd", p.cwd));
      info.appendChild(el("div", "pi-profile", (p.profileId || "workflow").toUpperCase()));
      item.appendChild(info);
      const open = () => { $("project-dropdown").classList.add("hidden"); openProject(p.id); };
      item.onclick = open;
      item.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } };
      const delBtn = el("button", "project-delete-btn", "🗑");
      delBtn.type = "button";
      delBtn.title = "Remove from dotz (does not touch the folder)";
      delBtn.setAttribute("aria-label", "Remove project " + p.name + " from dotz");
      delBtn.onclick = (e) => { e.stopPropagation(); confirmDeleteProject(p); };
      item.appendChild(delBtn);
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

/* ---------- topbar ---------- */
function bindTopbar() {
  $("panels-btn").onclick = togglePalette;
  $("leaderboard-btn").onclick = async () => {
    // Opens the self-contained pantheon LLM-leaderboard page in the OS default browser.
    try {
      const r = await post("/api/leaderboard/open");
      if (!r || r.ok === false) pushError("leaderboard: " + ((r && r.error) || "could not open"));
      else showToast("Opened leaderboard");
    } catch (e) { pushError("leaderboard: " + e.message); }
  };
  $("brain-float-toggle").onclick = () => {
    state.brainFloat = !state.brainFloat;
    saveBrainFloat();
    updateBrainFloat();
  };
}

function bindBrainFloat() {
  updateBrainFloat();
}

/* ---------- palette (panel palette close binder; toggle lives in panels.js) ---------- */
function bindPalette() {
  $("palette-close").onclick = () => $("palette").classList.add("hidden");
  $("palette").onclick = (e) => { if (e.target.id === "palette") $("palette").classList.add("hidden"); };
}

function bindKeyboard() {
  document.addEventListener("keydown", (e) => {
    // Global command-palette toggle (Ctrl/Cmd+K). Checked before the panel-palette Ctrl+P so a
    // browser Ctrl+K (often a focus-search hotkey) never leaks while the dashboard is focused.
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      toggleKbPalette();
      return;
    }
    if (e.ctrlKey && e.key.toLowerCase() === "p") { e.preventDefault(); togglePalette(); }
    // Modal-aware Escape routing + Tab focus trap. A visible dialog takes priority.
    const modal = visibleModal();
    if (modal) {
      if (e.key === "Escape") {
        e.preventDefault();
        // The command palette owns its own dismissal; closing it here would double-handle.
        if (modal.id === "kb-palette") { hideKbPalette(); return; }
        // Gate stays modal until an explicit choice; settings/update get a close path.
        if (modal.id === "settings-card") $("settings-close").click();
        else if (modal.id === "update-card") $("update-later").click();
        return;
      }
      // The command palette input owns its own arrow/Tab/Enter navigation; let it handle those so the
      // global Tab-trap below doesn't fight the palette's selection cycling.
      if (modal.id === "kb-palette" && (e.key === "Tab" || e.key === "ArrowUp" || e.key === "ArrowDown" || e.key === "Enter")) return;
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
  // The keyboard command palette sits above the panel palette but below the human gate — a gate
  // must never be dismissable by accident, so it wins when both are up.
  for (const id of ["kb-palette", "update-card", "settings-card", "gate-card"]) {
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

// Apply a reasoning level from the keyboard. Mirrors renderReasoning's button onclick but without
// depending on the seg being rendered/visible — works pre-session (persists config) and live.
function setReasoningLevel(lvl) {
  if (state.sessionId) {
    post(`/api/sessions/${state.sessionId}/thinking`, { level: lvl })
      .then((r) => { state.summary = Object.assign({}, state.summary, r); renderReasoning(state.summary); persistConfig({ thinkingLevel: lvl }); })
      .catch((e) => pushError("thinking: " + e.message));
  } else {
    persistConfig({ thinkingLevel: lvl });
    renderReasoning({ thinkingLevel: lvl, availableThinkingLevels: (backendThinkingLevels || THINK_LEVELS), supportsThinking: true });
  }
}

function togglePanel(name) {
  if (state.layout.open.includes(name)) unmountPanel(name);
  else openPanel(name);
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

// Backend-driven model catalog: GET /api/models returns the full provider list, per-provider
// metadata, the available model catalog, provider-aware defaults, and the current config's
// selected provider + executive model. This populates the model picker before any session
// exists (the command center), so the dropdown/datalist is never empty or hardcoded.
async function loadGlobalModels() {
  try {
    const m = await api("/api/models");
    state.modelCatalog = m.available || [];
    if (m.providerMeta) state.providers = m.providerMeta;
    if (m.providerDefaults) state.providerDefaults = m.providerDefaults;
    if (m.thinkingLevels) backendThinkingLevels = m.thinkingLevels;
    if (m.current) {
      state.activeProvider = m.current.provider || state.activeProvider || "ollama";
      // Surface the backend's current provider/model so primeTopbarFromConfig renders them.
      if (state.config) {
        state.config.provider = m.current.provider || state.config.provider;
        state.config.executiveModel = m.current.modelId || state.config.executiveModel;
        if (m.subagentModel) state.config.subagentModel = m.subagentModel;
        if (m.thinkingLevel) state.config.thinkingLevel = m.thinkingLevel;
      }
    }
  } catch (e) { pushError("models: " + e.message); }
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
  try { const { projects } = await api("/api/projects"); state.projects = projects || []; state.projectsError = false; }
  catch (e) { state.projectsError = true; pushError("projects: " + e.message); }
}

function confirmDeleteProject(p) {
  // Destructive: purges this project's dotz-side memories/cache. The folder and its files are
  // never touched — only dotz's own state under ~/.dotz is removed.
  const ok = window.confirm(
    `Remove "${p.name}" from dotz?\n\nThis deletes its dotz memories, workflow history and cache. ` +
    `Your project folder and files (${p.cwd}) are NOT touched.`
  );
  if (ok) deleteProject(p.id);
}

async function deleteProject(id) {
  try {
    const res = await del("/api/projects/" + encodeURIComponent(id));
    if (!res || res.ok === false) { pushError("project not found"); return; }
    if (state.activeProjectId === id) { state.activeProjectId = null; }
    await loadProjects();
    renderProjectDropdown();
    const pu = res.purged || {};
    showToast(`Project removed · ${pu.memories || 0} memories, ${pu.workflows || 0} runs purged`);
  } catch (e) {
    pushError("delete project: " + e.message);
  }
}

/* ---------- session lifecycle ---------- */
async function newSession() {
  // Reset per-session client collections so a project switch doesn't bleed the previous project's
  // workflow/sandbox runs into the new session (the refresh* calls below repopulate from scratch).
  state.workflows.clear();
  state.activeWfId = null;
  state.sandbox.runs.clear();
  state.sandbox.activeRunId = null;
  state.recalled = [];
  const s = await post("/api/sessions", { profileId: state.activeProfileId, projectId: state.activeProjectId || undefined });
  setSession(s);
  clearTranscript();
  connectWS();
  // allSettled, not all: a single loader failing (e.g. /models) must NOT reject the whole session
  // bootstrap and strand the user in a half-open bento. Each loader surfaces its own error.
  await Promise.allSettled([loadModels(), loadCommands(), refreshMemory(), refreshSkills(), refreshWorkflows(), loadProjectFiles()]);
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
  // Use the backend-provided thinking levels (from GET /api/models) so the picker is backend-driven.
  renderReasoning({ thinkingLevel: state.config.thinkingLevel, availableThinkingLevels: (backendThinkingLevels || THINK_LEVELS), supportsThinking: true });
}

async function loadModels() {
  try {
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
  } catch (e) { pushError("models: " + e.message); }
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
  // Pre-session (command center): no sessionId yet — POSTing would hit /api/sessions/null/model (404).
  // Reflect the choice in the UI; callers persist it to config and the real session inherits it on open.
  if (!state.sessionId) { updateModelUI(ref); return; }
  try { const s = await post(`/api/sessions/${state.sessionId}/model`, ref); state.summary = s; updateModelUI(s.model); renderReasoning(s); }
  catch (e) { pushError("model: " + e.message); }
}

/* ---------- reasoning ---------- */
function renderReasoning(summary) {
  const seg = $("reasoning-seg");
  seg.innerHTML = "";
  const avail = summary.availableThinkingLevels || [];
  // Iterate over the backend-provided thinking-level list (from GET /api/models) so the picker
  // is backend-driven; fall back to the hardcoded constant only if the backend hasn't provided one.
  const allLevels = backendThinkingLevels || THINK_LEVELS;
  allLevels.forEach((lvl) => {
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

/* ---------- workflows (refresh glue; render/event handling in graph.js/handlers.js) ---------- */
async function refreshWorkflows() {
  try {
    const { runs } = await api("/api/workflows/active");
    // /api/workflows/active returns ALL globally-active runs; keep only this project/session's so a
    // foreign project's runs don't bleed into this client's graph (newSession just cleared them).
    runs.filter((r) => (r.projectId != null && r.projectId === state.activeProjectId) || (r.sessionId != null && r.sessionId === state.sessionId))
      .forEach((r) => state.workflows.set(r.id, r));
    refreshWfCount();
    refreshWorkflowGraph();
  } catch {}
}

/* ---------- settings + updater ---------- */
function bindSettings() {
  $("settings-btn").onclick = () => {
    $("settings-card").classList.remove("hidden");
    $("settings-version").textContent = (window.dotz && window.dotz.version) || "browser";
    $("settings-feed").textContent = (window.dotz && window.dotz.electron) ? "configured" : "not configured (browser/dev)";
    $("settings-update-status").textContent = state.updateStatus || "—";
    refreshTelemetry();
    $("settings-close").focus();
  };
  $("settings-close").onclick = () => $("settings-card").classList.add("hidden");
  bindTelemetryToggle();
  $("settings-check-update").onclick = () => {
    if (window.dotz && window.dotz.update && window.dotz.update.check) {
      window.dotz.update.check();
      $("settings-update-status").textContent = "checking…";
    } else {
      $("settings-update-status").textContent = "updates only available in packaged app with feed";
    }
  };
}

/* Opt-in telemetry toggle (settings panel). One click flips PII-free usage telemetry on/off via
   the window.dotz.telemetry bridge; unavailable in a plain browser/dev build, where we disable the
   control instead of pretending it works. */
function renderTelemetry(status) {
  const btn = $("settings-telemetry-toggle");
  const ep = $("settings-telemetry-endpoint");
  if (!btn) return;
  if (!status) {
    // No bridge (browser/dev) or a failed call — telemetry isn't controllable here.
    btn.textContent = "N/A";
    btn.setAttribute("aria-checked", "false");
    btn.disabled = true;
    if (ep) ep.textContent = "packaged app only";
    return;
  }
  const on = !!status.enabled;
  btn.disabled = false;
  btn.textContent = on ? "ON" : "OFF";
  btn.classList.toggle("btn-go", on);
  btn.setAttribute("aria-checked", on ? "true" : "false");
  if (ep) {
    /* reachable === false is a live probe verdict from the backend: the sink endpoint is
       configured but not answering — events are being dropped. Surface it loudly instead of
       the old silent failure; true/undefined/null render as before. */
    const dead = on && status.reachable === false;
    const sink = on ? (status.endpoint || "enabled (no sink)") : "disabled";
    ep.textContent = dead ? sink + " — UNREACHABLE (events are not being collected)" : sink;
    ep.classList.toggle("red", dead);
  }
}

async function refreshTelemetry() {
  if (!window.dotz || !window.dotz.telemetry || typeof window.dotz.telemetry.status !== "function") {
    renderTelemetry(null);
    return;
  }
  renderTelemetry(await window.dotz.telemetry.status());
}

function bindTelemetryToggle() {
  const btn = $("settings-telemetry-toggle");
  if (!btn || btn._dotzBound) return;
  btn._dotzBound = true;
  btn.onclick = async () => {
    if (!window.dotz || !window.dotz.telemetry || typeof window.dotz.telemetry.setEnabled !== "function") {
      renderTelemetry(null);
      return;
    }
    const turningOn = btn.getAttribute("aria-checked") !== "true";
    btn.disabled = true;
    const status = await window.dotz.telemetry.setEnabled(turningOn);
    renderTelemetry(status);
  };
}

function bindUpdateCard() {
  // Tauri signed updater: APPLY downloads + installs the signed release + relaunches in place.
  $("update-apply").onclick = () => {
    if (window.dotz && window.dotz.update && window.dotz.update.apply) {
      window.dotz.update.apply();
      state.updateStatus = "updating…";
      $("settings-update-status").textContent = state.updateStatus;
      $("update-actions").classList.add("hidden");
      $("update-body").textContent = "Downloading and installing the signed update. dotz will relaunch shortly…";
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
        `UPDATE &amp; RESTART downloads and installs the signed update, then relaunches.${dirtyWarn}`;
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
      $("update-body").textContent = "Downloading and installing the signed update. dotz will relaunch shortly…";
      $("settings-update-status").textContent = "updating…";
    } else if (status === "failed") {
      const msg = esc(data.message || data.error || "unknown error");
      $("update-body").textContent = "Update error: " + msg;
      $("update-card").classList.remove("hidden");
      $("update-actions").classList.remove("hidden");
      $("update-progress").classList.add("hidden");
      $("update-apply").classList.add("hidden");
      $("update-later").textContent = "CLOSE";
      $("settings-update-status").textContent = "error: " + msg;
      $("update-later").focus();
    }
  });
}

export {
  init,
  showCommandCenter,
  enterBento,
  updateCommandCenterMeta,
  bindCommandCenter,
  bindProjectSelector,
  renderProjectDropdown,
  openProject,
  bindTopbar,
  bindBrainFloat,
  bindPalette,
  bindKeyboard,
  visibleModal,
  trapFocus,
  setReasoningLevel,
  togglePanel,
  loadProfiles,
  renderProfilePicker,
  loadProviders,
  loadGlobalModels,
  loadDotzConfig,
  persistConfig,
  loadProjects,
  confirmDeleteProject,
  deleteProject,
  newSession,
  setSession,
  populateProviderSelect,
  primeTopbarFromConfig,
  loadModels,
  onProviderChange,
  syncSubagentInput,
  providerIsFreeForm,
  renderModelInput,
  populateModelSuggestions,
  updateModelUI,
  submitModel,
  submitModelSelect,
  setModel,
  renderReasoning,
  loadCommands,
  renderQuickChips,
  refreshWorkflows,
  bindSettings,
  renderTelemetry,
  refreshTelemetry,
  bindTelemetryToggle,
  bindUpdateCard,
  wireUpdaterIpc,
};