/* dotz — first-run wizard (C1). Vanilla ESM, no framework.
 * Appears on launch if `~/.dotz/first-run-done` is absent. Steps: (1) provider + key
 * (POST /api/provider/key), (2) fetch the ONNX embedding model + verify agent-browser
 * (POST /api/models/fetch + poll /api/first-run/state), (3) optional project create
 * (POST /api/projects), (4) mark done (POST /api/first-run/complete → never shows again).
 * Loads after main.js; reuses `api()`/`state`/`pushError`/`showToast`. Self-contained:
 * builds its own overlay DOM + inline styles, so no edits to styles.css.
 */
import { api, post } from './api.js';
import { state } from './state.js';
import { pushError, showToast } from './chat.js';

const FALLBACK_PROVIDERS = [ // mirrors types::providers() — used only if state route omits one
  { id: "ollama", label: "Ollama Cloud" }, { id: "openrouter", label: "OpenRouter" },
  { id: "nvidia-nim", label: "NVIDIA NIM" }, { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" }, { id: "google", label: "Google" },
  { id: "groq", label: "Groq" }, { id: "mistral", label: "Mistral" },
  { id: "xai", label: "xAI" }, { id: "deepseek", label: "DeepSeek" },
  { id: "cohere", label: "Cohere" }, { id: "local", label: "Local (Ollama/LM Studio)" },
];

// el(tag, style, [kids]) — kids are nodes or strings. Inline style keeps the overlay off styles.css.
const el = (tag, style, kids) => {
  const e = document.createElement(tag);
  if (style) e.setAttribute("style", style);
  for (const k of (Array.isArray(kids) ? kids : kids ? [kids] : []))
    e.appendChild(typeof k === "string" ? document.createTextNode(k) : k);
  return e;
};

bootWizard().catch((e) => pushError("wizard: " + e.message));

async function bootWizard() {
  let st;
  try { st = await api("/api/first-run/state"); } catch { return; } // unreachable → don't block
  if (!st || !st.wizardNeeded) return;
  showWizard(st);
}

function showWizard(st) {
  if (document.getElementById("wizard-overlay")) return; // idempotent: never mount two
  const providers = (st.providers && st.providers.length) ? st.providers : FALLBACK_PROVIDERS;
  const overlay = el("div", [
    "position:fixed", "inset:0", "z-index:9999", "background:rgba(0,0,0,0.72)",
    "display:flex", "align-items:center", "justify-content:center",
  ].join(";"));
  overlay.id = "wizard-overlay";
  const card = el("div", [
    "width:min(560px,92vw)", "max-height:88vh", "overflow:auto",
    "background:var(--surface-1,#111)", "border:1px solid var(--mauve,#6e6188)",
    "border-radius:10px", "padding:24px 28px", "box-shadow:0 12px 48px rgba(0,0,0,0.6)",
  ].join(";"), [
    el("div", "font-size:18px;letter-spacing:1px;color:var(--cyan,#3ad);margin-bottom:4px;", "◆ WELCOME TO dotz"),
    el("div", "color:var(--muted,#888);font-size:13px;margin-bottom:18px;", "A quick setup so your first chat works."),
  ]);
  overlay.appendChild(card);
  document.body.appendChild(overlay);

  const steps = [buildStep1(providers), buildStep2(st), buildStep3(), buildStep4(() => overlay.remove())];
  for (const s of steps) card.appendChild(s.root);
  steps.forEach((s, i) => s.wire(() => {
    s.root.style.display = "none";
    if (steps[i + 1]) steps[i + 1].root.style.display = "block";
  }));
}

// Step 1: provider dropdown + key input → POST /api/provider/key.
function buildStep1(providers) {
  const head = (t) => el("div", "font-size:13px;letter-spacing:1px;color:var(--mauve,#a89bd1);margin-bottom:8px;", t);
  const root = el("div", null, [head("STEP 1 — PROVIDER + API KEY")]);
  const sel = el("select", "flex:1;min-width:160px;", providers.map((p) => {
    const o = el("option"); o.value = p.id; o.textContent = p.label || p.id; return o;
  }));
  sel.value = (state.activeProvider && providers.some((p) => p.id === state.activeProvider))
    ? state.activeProvider : "ollama";
  const key = el("input", "flex:2;min-width:200px;");
  key.type = "password"; key.placeholder = "paste provider api key…";
  key.setAttribute("autocomplete", "off"); key.setAttribute("spellcheck", "false");
  root.appendChild(el("div", "display:flex;gap:8px;align-items:center;flex-wrap:wrap;", [sel, key]));
  const err = el("div", "color:var(--peach,#e96);font-size:12px;margin-top:6px;min-height:1em;");
  const save = el("button", null, "TEST & SAVE"); save.className = "btn-go";
  const skip = el("button", "margin-left:auto;", "SKIP"); skip.className = "btn-mini";
  root.appendChild(el("div", "margin-top:12px;display:flex;gap:8px;align-items:center;", [save, skip]));
  root.appendChild(err);

  const wire = (advance) => {
    save.onclick = async () => {
      const provider = sel.value, k = key.value.trim();
      if (!provider) { err.textContent = "select a provider first"; return; }
      if (!k) { err.textContent = "paste a key first (or SKIP)"; return; }
      save.disabled = true; save.textContent = "SAVING…";
      try {
        const r = await api("/api/provider/key", {
          method: "POST", headers: { "content-type": "application/json" },
          body: JSON.stringify({ provider, key: k }),
        });
        if (r && r.error) throw new Error(r.error);
        key.value = ""; key.type = "password"; // never leave the key in the DOM
        showToast(`${provider}: key saved`); advance();
      } catch (e) { err.textContent = "save failed: " + e.message; }
      finally { save.disabled = false; save.textContent = "TEST & SAVE"; }
    };
    skip.onclick = () => advance();
    key.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); save.click(); } });
  };
  return { root, wire };
}

// Step 2: model fetch (POST /api/models/fetch is idempotent; poll state for modelFilesPresent).
function buildStep2(initialState) {
  const head = (t) => el("div", "font-size:13px;letter-spacing:1px;color:var(--mauve,#a89bd1);margin-bottom:8px;", t);
  const root = el("div", "display:none;", [head("STEP 2 — EMBEDDING MODEL + BROWSER")]);
  const status = el("div", "font-size:13px;color:var(--muted,#aaa);margin-bottom:10px;white-space:pre-wrap;");
  const renderStatus = (st) => {
    const m = st && st.modelFilesPresent ? "✓ model files present" : "○ model files absent — download needed";
    const b = st && st.agentBrowserPresent ? "✓ agent-browser binary present" : "✗ agent-browser binary missing (run `npm install`)";
    status.textContent = `${m}\n${b}`;
  };
  renderStatus(initialState);
  root.appendChild(status);
  const fetch = el("button", "margin-right:8px;", "DOWNLOAD EMBEDDING MODEL"); fetch.className = "btn-go";
  const next = el("button", "margin-left:auto;", "NEXT"); next.className = "btn-mini";
  const log = el("div", "margin-top:10px;font-size:12px;color:var(--muted,#888);min-height:1em;white-space:pre-wrap;");
  root.appendChild(el("div", null, [fetch, next]));
  root.appendChild(log);

  let polling = null, advanced = false;
  const stopPoll = () => { if (polling) { clearInterval(polling); polling = null; } };
  // Poll state every 2s for up to 2 min; on success advance, on timeout let the operator RETRY/NEXT.
  const poll = (advance) => {
    stopPoll();
    let ticks = 0;
    polling = setInterval(async () => {
      ticks += 1;
      try {
        const st = await api("/api/first-run/state");
        renderStatus(st);
        if (st && st.modelFilesPresent) {
          stopPoll(); fetch.disabled = false; fetch.textContent = "DONE";
          log.textContent = "embedding model ready — continuing in 1s…";
          if (!advanced) setTimeout(advance, 1000);
        } else if (ticks > 60) {
          stopPoll(); fetch.disabled = false; fetch.textContent = "RETRY";
          log.textContent = "timed out — click RETRY or run `npm run fetch-model`";
        } else { log.textContent = `downloading… (${ticks * 2}s)`; }
      } catch { /* transient — keep polling */ }
    }, 2000);
  };

  const wire = (advance) => {
    fetch.onclick = async () => {
      fetch.disabled = true; fetch.textContent = "STARTING…"; log.textContent = "starting download…";
      try {
        const r = await post("/api/models/fetch");
        if (r && r.error) throw new Error(r.error);
        if (r && r.alreadyPresent) {
          log.textContent = "model already present — nothing to download"; fetch.textContent = "DONE";
          poll(advance); return;
        }
        fetch.textContent = "DOWNLOADING…"; poll(advance);
      } catch (e) {
        log.textContent = "fetch failed: " + e.message + " (or run `npm run fetch-model`)";
        fetch.disabled = false; fetch.textContent = "RETRY";
      }
    };
    next.onclick = () => { advanced = true; stopPoll(); advance(); };
  };
  return { root, wire };
}

// Step 3: optional project create — reuses window.dotz.pickDirectory when available.
function buildStep3() {
  const head = (t) => el("div", "font-size:13px;letter-spacing:1px;color:var(--mauve,#a89bd1);margin-bottom:8px;", t);
  const root = el("div", "display:none;", [head("STEP 3 — CREATE A PROJECT (OPTIONAL)")]);
  const name = el("input", "width:100%;margin-bottom:8px;"); name.placeholder = "project name";
  const cwd = el("input", "flex:1;"); cwd.placeholder = "absolute path to project folder";
  const browse = el("button", null, "BROWSE…"); browse.className = "btn-mini";
  if (window.dotz && typeof window.dotz.pickDirectory === "function") {
    browse.onclick = async () => {
      try { const dir = await window.dotz.pickDirectory(); if (dir) cwd.value = dir; }
      catch (e) { pushError("folder picker: " + e.message); }
    };
  } else { browse.style.display = "none"; }
  root.appendChild(name);
  root.appendChild(el("div", "display:flex;gap:8px;margin-bottom:8px;", [cwd, browse]));
  const create = el("button", null, "CREATE & OPEN"); create.className = "btn-go";
  const skip = el("button", "margin-left:auto;", "SKIP"); skip.className = "btn-mini";
  const err = el("div", "color:var(--peach,#e96);font-size:12px;margin-top:6px;min-height:1em;");
  root.appendChild(el("div", "display:flex;gap:8px;align-items:center;", [create, skip]));
  root.appendChild(err);

  const wire = (advance) => {
    create.onclick = async () => {
      const n = name.value.trim(), c = cwd.value.trim();
      if (!n || !c) { err.textContent = "name and cwd are required (or SKIP)"; return; }
      if (!/^[A-Za-z]:[\\/]/.test(c) && !/^\//.test(c)) {
        err.textContent = "cwd must be an absolute path (e.g. C:\\Users\\me\\proj or /home/me/proj)";
        return;
      }
      create.disabled = true; create.textContent = "CREATING…";
      try {
        const r = await post("/api/projects", { name: n, cwd: c, model: {
          provider: (state.config && state.config.provider) || "ollama",
          modelId: (state.config && state.config.executiveModel) || "glm-5.2",
        }});
        if (r && r.error) throw new Error(r.error);
        showToast("project created"); advance();
      } catch (e) { err.textContent = "create failed: " + e.message; }
      finally { create.disabled = false; create.textContent = "CREATE & OPEN"; }
    };
    skip.onclick = () => advance();
  };
  return { root, wire };
}

// Step 4: done → POST /api/first-run/complete writes the marker; removeOverlay hides it forever.
function buildStep4(removeOverlay) {
  const head = (t) => el("div", "font-size:13px;letter-spacing:1px;color:var(--mauve,#a89bd1);margin-bottom:8px;", t);
  const root = el("div", "display:none;", [
    head("STEP 4 — DONE"),
    el("div", "font-size:13px;color:var(--muted,#aaa);margin-bottom:14px;",
      "dotz is ready. You can change any of these later in the topbar + connections panel."),
  ]);
  const finish = el("button", null, "FINISH ✓"); finish.className = "btn-go";
  root.appendChild(finish);
  const wire = () => {
    finish.onclick = async () => {
      finish.disabled = true; finish.textContent = "…";
      try { await post("/api/first-run/complete"); removeOverlay(); }
      catch (e) { pushError("first-run complete: " + e.message); finish.disabled = false; finish.textContent = "RETRY"; }
    };
  };
  return { root, wire };
}