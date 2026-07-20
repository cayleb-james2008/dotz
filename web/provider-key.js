/* dotz — provider API key form (Q4). Split from app.js (C7). No behavior change.
 * Keys travel over the authed api() channel, are NEVER displayed again after save (only a
 * redacted set/not-set indicator), and the Save button disables while the request is in flight.
 * Backend contract:
 *   GET    /api/provider/key            → [{provider:"ollama", set:true}, ...]  (key never included)
 *   POST   /api/provider/key {provider, key} → 204 on success, {error:"..."} on failure
 *   DELETE /api/provider/key?provider=ollama → 204 on success
 */
import { el, api } from '../api.js';
import { state } from '../state.js';
import { showToast } from '../chat.js';

// Fallback provider list for the connections-panel key form, used only before /api/providers
// resolves (or if it fails). Mirrors dotz-core/src/types.rs::providers() — same 12 ids + labels,
// same UI order — so the dropdown is always populated and stays in sync with the backend.
const KEY_PROVIDERS_FALLBACK = [
  { id: "openrouter", label: "OpenRouter" },
  { id: "nvidia-nim", label: "NVIDIA NIM" },
  { id: "ollama", label: "Ollama Cloud" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "google", label: "Google" },
  { id: "groq", label: "Groq" },
  { id: "mistral", label: "Mistral" },
  { id: "xai", label: "xAI" },
  { id: "deepseek", label: "DeepSeek" },
  { id: "cohere", label: "Cohere" },
  { id: "local", label: "Local (Ollama/LM Studio)" },
];

function wireProviderKeyForm(node) {
  const sel = node.querySelector("#conn-key-provider");
  const input = node.querySelector("#conn-key-input");
  const toggle = node.querySelector("#conn-key-toggle");
  const save = node.querySelector("#conn-key-save");
  const clear = node.querySelector("#conn-key-clear");
  const err = node.querySelector("#conn-key-error");
  if (!sel || !input || !toggle || !save || !clear) return;

  // Populate the provider dropdown from state.providers (loaded from /api/providers), falling
  // back to the hardcoded KEY_PROVIDERS_FALLBACK list so the form is usable before that resolves.
  const providers = (state.providers && state.providers.length) ? state.providers : KEY_PROVIDERS_FALLBACK;
  sel.innerHTML = "";
  for (const p of providers) {
    const opt = document.createElement("option");
    opt.value = p.id; opt.textContent = p.label || p.id;
    sel.appendChild(opt);
  }
  // Default to the active provider so the operator most often just pastes + saves.
  if (state.activeProvider && [...sel.options].some((o) => o.value === state.activeProvider)) {
    sel.value = state.activeProvider;
  }

  const setErr = (msg) => { if (!err) return; if (msg) { err.textContent = msg; err.classList.remove("hidden"); } else err.classList.add("hidden"); };
  const busy = (b) => { save.disabled = b; clear.disabled = b; save.textContent = b ? "SAVING…" : "SAVE"; };

  toggle.onclick = () => {
    const show = input.type === "password";
    input.type = show ? "text" : "password";
    toggle.textContent = show ? "hide" : "show";
    toggle.setAttribute("aria-label", show ? "Hide key" : "Show key");
  };

  save.onclick = async () => {
    const provider = sel.value;
    const key = input.value;
    if (!provider) { setErr("select a provider first"); return; }
    if (!key) { setErr("paste a key first"); return; }
    setErr(""); busy(true);
    try {
      const res = await api("/api/provider/key", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ provider, key }) });
      // Defensive: a misbehaving backend could return 2xx with {error:"..."} instead of a 4xx.
      // Treat any body-error as a failure so a key save never silently appears to succeed.
      if (res && res.error) throw new Error(res.error);
      // CRITICAL: never leave the key in the DOM after save — clear the input + reset to password
      // type so a subsequent show-toggle can't reveal a stale value.
      input.value = "";
      input.type = "password";
      toggle.textContent = "show";
      showToast(`${provider}: key saved`);
      refreshProviderKeys(node);
    } catch (e) {
      setErr("save failed: " + e.message);
    } finally {
      busy(false);
    }
  };

  clear.onclick = async () => {
    const provider = sel.value;
    if (!provider) { setErr("select a provider first"); return; }
    setErr(""); busy(true);
    try {
      const res = await api("/api/provider/key?provider=" + encodeURIComponent(provider), { method: "DELETE" });
      if (res && res.error) throw new Error(res.error);
      showToast(`${provider}: key cleared`);
      refreshProviderKeys(node);
    } catch (e) {
      setErr("clear failed: " + e.message);
    } finally {
      busy(false);
    }
  };

  // Enter in the key field submits save; don't submit the (nonexistent) form.
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); save.click(); } });
}

// Fetch the redacted set/not-set status for every provider and render it. The key itself is NEVER
// returned by the backend (contract: GET returns only {provider, set}), so this is the only
// post-save indicator the operator gets.
async function refreshProviderKeys(node) {
  const statusEl = node.querySelector("#conn-keys-status");
  if (!statusEl) return;
  const providers = (state.providers && state.providers.length) ? state.providers : KEY_PROVIDERS_FALLBACK;
  let statusMap = {};
  try {
    const arr = await api("/api/provider/key");
    if (Array.isArray(arr)) for (const e of arr) if (e && e.provider) statusMap[e.provider] = !!e.set;
  } catch {
    // Backend route not yet wired (parallel implementer) — show "not set" for everyone so the
    // form is still usable; the operator's first Save will surface a real error if the route
    // is missing. Don't spam pushError on every poll.
  }
  statusEl.innerHTML = "";
  for (const p of providers) {
    const row = el("div", "conn-key-row");
    const set = !!statusMap[p.id];
    row.appendChild(el("span", "conn-dot " + (set ? "ok" : "off")));
    row.appendChild(el("span", "conn-key-name", p.label || p.id));
    row.appendChild(el("span", "conn-key-state dim mono", set ? "set (redacted) ••••••••" : "not set"));
    statusEl.appendChild(row);
  }
}

export { wireProviderKeyForm, refreshProviderKeys };