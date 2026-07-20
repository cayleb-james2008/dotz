/* dotz — MEMORY panel wirer + recall observability. Split from app.js (C7). No behavior change. */
import { $, el, api, post, patch, del, truncate } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

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
      showToast(`memory consolidated — removed ${r.removed}, kept ${r.kept}`);
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
    state.memoryError = false;
    renderMemory();
  } catch { state.memoryError = true; renderMemory(); }
}

function renderMemory() {
  const box = document.querySelector('.panel[data-panel="memory"] #memory-list') || $("memory-list");
  if (!box) return;
  box.innerHTML = "";
  if (!state.memory.length) { box.appendChild(el("span", "dim mono", state.memoryError ? "⚠ couldn't load memory — check the server" : "no memories yet — they're captured automatically")); return; }
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

// Recall observability — which memories were auto-injected for the current task. (handler lives in
// handlers.js::handleMemoryRecall; render here so the panel owns its DOM.)
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

export { wireMemoryPanel, refreshMemory, renderMemory, inlineEditMemory, renderRecalled };