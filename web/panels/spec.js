/* dotz — SPEC panel wirer (OpenSpec-style spec management). Split from app.js (C7). No behavior change. */
import { el, api, esc, truncate, post, projectQs } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

function wireSpecPanel(node) {
  node.querySelector("#spec-refresh").onclick = refreshSpecs;
  node.querySelector("#spec-create").onclick = async () => {
    const input = node.querySelector("#spec-title");
    const title = input.value.trim();
    if (!title) { pushError("spec title is required"); return; }
    try {
      const { change } = await post("/api/specs/changes", {
        projectId: state.activeProjectId || undefined,
        title,
      });
      input.value = "";
      state.activeSpecId = change.id;
      await refreshSpecs();
      showToast(`spec proposed: ${change.id}`);
    } catch (e) { pushError("spec propose: " + e.message); }
  };
  refreshSpecs();
}

async function refreshSpecs() {
  const panel = document.querySelector('.panel[data-panel="spec"]');
  if (!panel) return;
  const status = panel.querySelector("#spec-status");
  status.textContent = "loading...";
  try {
    const data = await api("/api/specs/status" + projectQs());
    state.specStatus = data;
    state.specs = data.changes || [];
    status.textContent = `${data.active || 0} active / ${state.specs.length} changes`;
    renderSpecPanel(panel);
  } catch (e) {
    status.textContent = "load failed";
    pushError("spec status: " + e.message);
  }
}

function renderSpecPanel(panel) {
  const list = panel.querySelector("#spec-list");
  const detail = panel.querySelector("#spec-detail");
  list.innerHTML = "";
  const changes = state.specs || [];
  if (!changes.length) {
    list.appendChild(el("div", "dim mono", "no spec changes"));
    detail.innerHTML = '<div class="dim mono">create a proposal to begin</div>';
    return;
  }
  if (!state.activeSpecId || !changes.some((c) => c.id === state.activeSpecId)) {
    state.activeSpecId = changes[0].id;
  }
  changes.forEach((change) => {
    const item = el("button", "ops-item" + (change.id === state.activeSpecId ? " active" : ""));
    item.type = "button";
    item.innerHTML = `<span class="ops-item-title">${esc(change.id)}</span><span class="ops-pill ${esc(change.status)}">${esc(change.status)}</span><span class="ops-item-sub">${esc(truncate(change.title, 64))}</span>`;
    item.onclick = () => { state.activeSpecId = change.id; renderSpecPanel(panel); };
    list.appendChild(item);
  });
  renderSpecDetail(detail, changes.find((c) => c.id === state.activeSpecId));
}

function renderSpecDetail(detail, change) {
  if (!change) {
    detail.innerHTML = '<div class="dim mono">select a change</div>';
    return;
  }
  detail.innerHTML = "";
  detail.appendChild(el("div", "ops-title", change.title || change.id));
  detail.appendChild(el("div", "ops-sub mono", change.path || ""));
  const actions = el("div", "ops-actions");
  ["apply", "verify", "sync", "archive"].forEach((action) => {
    const btn = el("button", "btn-mini" + (action === "archive" ? " btn-stop" : action === "verify" ? " btn-go" : ""), action.toUpperCase());
    btn.onclick = () => specAction(change.id, action);
    actions.appendChild(btn);
  });
  detail.appendChild(actions);
  detail.appendChild(el("div", "sb-pane-head", "ARTIFACTS"));
  (change.artifacts || []).forEach((a) => {
    detail.appendChild(el("div", "ops-kv-row", `${a.exists ? "ok" : "missing"} ${a.kind}: ${a.path}`));
  });
  detail.appendChild(el("div", "sb-pane-head", "READINESS"));
  const readiness = change.readiness || [];
  if (!readiness.length) detail.appendChild(el("div", "dim mono", "no readiness checklist"));
  readiness.forEach((r) => {
    detail.appendChild(el("div", "ops-kv-row", `${r.status === "complete" ? "done" : "open"} ${r.title}`));
  });
}

async function specAction(id, action) {
  try {
    const result = await post(`/api/specs/changes/${encodeURIComponent(id)}/${action}`, {
      projectId: state.activeProjectId || undefined,
    });
    if (result.change && result.change.id) state.activeSpecId = result.change.id;
    await refreshSpecs();
    showToast(`spec ${action} complete`);
  } catch (e) { pushError(`spec ${action}: ` + e.message); }
}

export { wireSpecPanel, refreshSpecs, renderSpecPanel, renderSpecDetail, specAction };