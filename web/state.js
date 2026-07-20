/* dotz ultra code mode — shared mutable state singleton + layout/brain-float persistence.
 * Split from app.js (C7 modularization). No behavior change — pure mechanical split.
 * The `state` object is the shared mutable singleton: every module imports the same
 * module-instance reference, so mutations propagate identically to the old single-file scope.
 */
import { $ } from './api.js';
import { PANEL_NAMES } from './panels.js';

const LAYOUT_KEY = "dotz.layout.v1";
const BRAIN_FLOAT_KEY = "dotz.brainFloat.v1";

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
  templates: [],
  specs: [],
  specStatus: null,
  activeSpecId: null,
  livingDocs: { docs: [], suggestions: [] },
  vcsStatus: null,
  commands: [],
  kbCommands: [],
  projectFiles: [],
  sandbox: { languages: [], runs: new Map(), activeRunId: null, mode: "terminal" },
  workflows: new Map(),
  activeWfId: null,
  layout: { open: ["chat"], version: 1 },
  wfView: { x: 0, y: 0, w: 800, h: 600 },
  pendingGate: null,
  activeTemplate: null,
  brainFloat: true,
  updateStatus: null,
  browserSessionId: null,
  browserObservation: null,
  browserPollTimer: null,
  browserBusy: false,
  connectionsPollTimer: null,
  connectionsLoginProvider: null,
  chatAutoScroll: true,
  designSystems: [],
  marketplaceCatalog: [],
  marketplaceInstalled: [],
  // B3: perf dashboard state. perfRecording mirrors the backend
  // perf_recording_enabled flag; perfSummary is the last /api/perf/summary fetch.
  perfRecording: false,
  perfSummary: null,
};

function loadLayout() {
  try {
    const raw = localStorage.getItem(LAYOUT_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed.version === 1 && Array.isArray(parsed.open)) {
        // Drop unknown panel names + collapse duplicates so renderBento can't mount two nodes of the
        // same type (which would be undeletable and break per-panel [data-panel] lookups); keep chat.
        const open = [...new Set(parsed.open.filter((n) => PANEL_NAMES.includes(n)))];
        if (!open.includes("chat")) open.unshift("chat");
        state.layout = { version: 1, open };
      }
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

function updateBrainFloat() {
  const bf = $("brain-float");
  // Only fully hide when there's no session; otherwise toggle the collapsed state so the ◆ button
  // (which lives inside #brain-float) stays visible/clickable to re-expand — hiding the whole
  // container would hide its own toggle, making the collapse irreversible.
  bf.classList.toggle("hidden", !state.sessionId);
  bf.classList.toggle("collapsed", !state.brainFloat);
}

export {
  state,
  LAYOUT_KEY,
  BRAIN_FLOAT_KEY,
  loadLayout,
  saveLayout,
  loadBrainFloat,
  saveBrainFloat,
  updateBrainFloat,
};