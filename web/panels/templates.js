/* dotz — TEMPLATES panel wirer (user-editable workflow presets). Split from app.js (C7). No behavior change. */
import { $, el, api, post, patch, del, truncate } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

function wireTemplatesPanel(node) {
  const search = node.querySelector("#templates-search");
  if (search) search.addEventListener("input", () => renderTemplatesList(search.value.toLowerCase()));
  const newBtn = node.querySelector("#templates-new");
  if (newBtn) newBtn.onclick = () => showTemplateDetail(null);
  const cancel = node.querySelector("#templates-cancel");
  if (cancel) cancel.onclick = () => {
    const detail = node.querySelector("#templates-detail");
    if (detail) detail.classList.add("hidden");
  };
  const run = node.querySelector("#templates-run");
  if (run) run.onclick = () => {
    const t = state.activeTemplate;
    if (!t) return;
    if (t.hasArgs) {
      const argsBox = node.querySelector("#templates-run-args");
      if (argsBox) argsBox.classList.remove("hidden");
    } else {
      runTemplate(t.id, "");
    }
  };
  const argsRun = node.querySelector("#templates-args-run");
  if (argsRun) argsRun.onclick = () => {
    const input = node.querySelector("#templates-args-input");
    const t = state.activeTemplate;
    if (t && input) runTemplate(t.id, input.value);
  };
  const fork = node.querySelector("#templates-fork");
  if (fork) fork.onclick = async () => {
    const t = state.activeTemplate;
    if (!t) return;
    try {
      const { template } = await post(`/api/templates/${encodeURIComponent(t.id)}/fork`, {});
      await refreshTemplates();
      showTemplateDetail(template);
      showToast(`forked ${t.name} → ${template.name}`);
    } catch (e) { pushError("template fork: " + e.message); }
  };
  const save = node.querySelector("#templates-save");
  if (save) save.onclick = async () => {
    const title = node.querySelector("#templates-detail-title");
    const desc = node.querySelector("#templates-detail-desc");
    const body = node.querySelector("#templates-detail-body");
    const t = state.activeTemplate;
    const payload = {
      name: title.value.trim(),
      description: desc.value.trim(),
      body: body.value.trim(),
    };
    try {
      let result;
      if (t && t.source === "user") {
        result = await patch(`/api/templates/${encodeURIComponent(t.id)}`, payload);
      } else {
        result = await post("/api/templates", payload);
      }
      await refreshTemplates();
      showTemplateDetail(result.template);
      showToast("template saved");
    } catch (e) { pushError("template save: " + e.message); }
  };
  const delBtn = node.querySelector("#templates-delete");
  if (delBtn) delBtn.onclick = async () => {
    const t = state.activeTemplate;
    if (!t || t.source !== "user") return;
    try {
      await del(`/api/templates/${encodeURIComponent(t.id)}`);
      const detail = node.querySelector("#templates-detail");
      if (detail) detail.classList.add("hidden");
      state.activeTemplate = null;
      await refreshTemplates();
      showToast("template deleted");
    } catch (e) { pushError("template delete: " + e.message); }
  };
  refreshTemplates();
}

async function refreshTemplates() {
  try {
    const { templates } = await api("/api/templates");
    state.templates = templates || [];
    renderTemplatesList("");
  } catch (e) { pushError("templates load: " + e.message); }
}

function renderTemplatesList(filter) {
  const panel = document.querySelector('.panel[data-panel="templates"]');
  const list = panel ? panel.querySelector("#templates-list") : $("templates-list");
  const count = panel ? panel.querySelector("#templates-count") : $("templates-count");
  if (!list || !count) return;
  const all = state.templates || [];
  const f = filter ? all.filter((t) => (t.name || "").toLowerCase().includes(filter) || (t.description || "").toLowerCase().includes(filter)) : all;
  count.textContent = `${f.length} of ${all.length} templates`;
  list.innerHTML = "";
  f.forEach((t) => {
    const item = el("div", "skill-item");
    const head = el("div", "skill-item-head");
    head.appendChild(el("span", "skill-name", (t.name || t.id) + (t.hasArgs ? " $@" : "")));
    head.appendChild(el("span", "skill-source " + t.source, t.source));
    item.appendChild(head);
    if (t.description) item.appendChild(el("div", "skill-desc", truncate(t.description, 100)));
    item.onclick = () => showTemplateDetail(t);
    list.appendChild(item);
  });
}

async function showTemplateDetail(t) {
  const panel = document.querySelector('.panel[data-panel="templates"]');
  const detail = panel ? panel.querySelector("#templates-detail") : $("templates-detail");
  if (!detail) return;
  detail.classList.remove("hidden");
  const nameEl = detail.querySelector("#templates-detail-name");
  const sourceEl = detail.querySelector("#templates-detail-source");
  const titleInput = detail.querySelector("#templates-detail-title");
  const descInput = detail.querySelector("#templates-detail-desc");
  const bodyInput = detail.querySelector("#templates-detail-body");
  const argsBox = detail.querySelector("#templates-run-args");
  if (argsBox) argsBox.classList.add("hidden");
  if (!t) {
    state.activeTemplate = null;
    if (nameEl) nameEl.textContent = "New template";
    if (sourceEl) { sourceEl.textContent = "user"; sourceEl.className = "skill-source user"; }
    if (titleInput) titleInput.value = "";
    if (descInput) descInput.value = "";
    if (bodyInput) bodyInput.value = "---\nname: \ndescription: \n---\n\n";
    return;
  }
  state.activeTemplate = t;
  if (nameEl) nameEl.textContent = t.name || t.id;
  if (sourceEl) { sourceEl.textContent = t.source; sourceEl.className = "skill-source " + t.source; }
  if (titleInput) titleInput.value = t.name || "";
  if (descInput) descInput.value = t.description || "";
  try {
    const full = await api(`/api/templates/${encodeURIComponent(t.id)}`);
    if (bodyInput) bodyInput.value = full.body || "";
  } catch (e) {
    if (bodyInput) bodyInput.value = "error: " + e.message;
  }
  const delBtn = detail.querySelector("#templates-delete");
  if (delBtn) delBtn.classList.toggle("hidden", t.source !== "user");
  const forkBtn = detail.querySelector("#templates-fork");
  if (forkBtn) forkBtn.classList.toggle("hidden", t.source !== "bundled");
}

async function runTemplate(id, args) {
  if (!state.sessionId) { pushError("open a session first"); return; }
  try {
    await post(`/api/templates/${encodeURIComponent(id)}/run`, { sessionId: state.sessionId, args });
    showToast("template running");
  } catch (e) { pushError("template run: " + e.message); }
}

export { wireTemplatesPanel, refreshTemplates, renderTemplatesList, showTemplateDetail, runTemplate };