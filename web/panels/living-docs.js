/* dotz — LIVING DOCS panel wirer. Split from app.js (C7). No behavior change. */
import { el, api, post, patch, projectQs } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

function wireLivingDocsPanel(node) {
  node.querySelector("#ld-refresh").onclick = refreshLivingDocs;
  node.querySelector("#ld-scope").onchange = refreshLivingDocs;
  node.querySelector("#ld-kind").onchange = () => renderLivingDocsPanel(node);
  node.querySelector("#ld-save").onclick = saveLivingDoc;
  refreshLivingDocs();
}

async function refreshLivingDocs() {
  const panel = document.querySelector('.panel[data-panel="living-docs"]');
  if (!panel) return;
  const status = panel.querySelector("#ld-status");
  const scope = panel.querySelector("#ld-scope").value;
  status.textContent = "loading...";
  try {
    const data = await api("/api/living-docs" + projectQs({ scope }));
    state.livingDocs = { docs: data.docs || [], suggestions: data.suggestions || [] };
    status.textContent = `${state.livingDocs.docs.length} docs / ${state.livingDocs.suggestions.length} suggestions`;
    renderLivingDocsPanel(panel);
  } catch (e) {
    status.textContent = "load failed";
    pushError("living docs: " + e.message);
  }
}

function renderLivingDocsPanel(panel) {
  const kind = panel.querySelector("#ld-kind").value;
  const editor = panel.querySelector("#ld-editor");
  const suggestions = panel.querySelector("#ld-suggestions");
  const doc = (state.livingDocs.docs || []).find((d) => d.kind === kind);
  editor.value = doc ? (doc.content || "") : "";
  suggestions.innerHTML = "";
  const items = state.livingDocs.suggestions || [];
  if (!items.length) {
    suggestions.appendChild(el("div", "dim mono", "no suggestions"));
    return;
  }
  items.forEach((s) => {
    const item = el("div", "ops-suggestion");
    item.appendChild(el("div", "ops-item-title", `${s.kind} / ${Math.round((s.confidence || 0) * 100)}%`));
    item.appendChild(el("div", "ops-item-sub", s.text));
    const actions = el("div", "ops-actions");
    const accept = el("button", "btn-mini btn-go", "ACCEPT");
    accept.onclick = () => livingSuggestionAction(s.id, "accept");
    const reject = el("button", "btn-mini", "REJECT");
    reject.onclick = () => livingSuggestionAction(s.id, "reject");
    actions.appendChild(accept);
    actions.appendChild(reject);
    item.appendChild(actions);
    suggestions.appendChild(item);
  });
}

async function saveLivingDoc() {
  const panel = document.querySelector('.panel[data-panel="living-docs"]');
  if (!panel) return;
  const scope = panel.querySelector("#ld-scope").value;
  const kind = panel.querySelector("#ld-kind").value;
  const content = panel.querySelector("#ld-editor").value;
  try {
    await patch("/api/living-docs" + projectQs({ scope }), { kind, content });
    await refreshLivingDocs();
    showToast("living doc saved");
  } catch (e) { pushError("living doc save: " + e.message); }
}

async function livingSuggestionAction(id, action) {
  const panel = document.querySelector('.panel[data-panel="living-docs"]');
  const scope = panel ? panel.querySelector("#ld-scope").value : "project";
  try {
    await post(`/api/living-docs/suggestions/${encodeURIComponent(id)}/${action}` + projectQs({ scope }));
    await refreshLivingDocs();
    showToast(`suggestion ${action}ed`);
  } catch (e) { pushError(`suggestion ${action}: ` + e.message); }
}

export { wireLivingDocsPanel, refreshLivingDocs, renderLivingDocsPanel, saveLivingDoc, livingSuggestionAction };