/* dotz — VCS panel wirer (git/vcs helpers). Split from app.js (C7). No behavior change. */
import { el, api, esc, truncate, post, projectQs } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

function wireVcsPanel(node) {
  node.querySelector("#vcs-refresh").onclick = refreshVcs;
  node.querySelector("#vcs-branch").onclick = createVcsBranch;
  node.querySelector("#vcs-commit").onclick = createVcsCommit;
  node.querySelector("#vcs-pr").onclick = createVcsPr;
  node.querySelector("#vcs-rollback").onclick = runVcsRollback;
  refreshVcs();
}

async function refreshVcs() {
  const panel = document.querySelector('.panel[data-panel="vcs"]');
  if (!panel) return;
  const chip = panel.querySelector("#vcs-status-chip");
  chip.textContent = "loading...";
  try {
    const { status } = await api("/api/vcs/status" + projectQs());
    state.vcsStatus = status;
    chip.textContent = status.insideWorktree ? (status.dirty ? "dirty" : "clean") : "not git";
    renderVcsPanel(panel, status);
  } catch (e) {
    chip.textContent = "load failed";
    pushError("vcs status: " + e.message);
  }
}

function renderVcsPanel(panel, status) {
  const box = panel.querySelector("#vcs-status");
  box.innerHTML = "";
  const rows = [
    ["cwd", status.cwd],
    ["branch", status.branch || "-"],
    ["head", truncate(status.headSha || "-", 12)],
    ["dirty", status.dirty ? "yes" : "no"],
    ["staged", status.staged ? "yes" : "no"],
    ["untracked", status.untracked ? "yes" : "no"],
    ["upstream", status.upstream || "-"],
    ["ahead/behind", `${status.ahead || 0}/${status.behind || 0}`],
    ["gh", status.ghInstalled ? (status.ghLoggedIn ? "ready" : "login needed") : "not installed"],
  ];
  rows.forEach(([k, v]) => {
    const row = el("div", "ops-kv-row");
    row.innerHTML = `<span>${esc(k)}</span><b>${esc(v)}</b>`;
    box.appendChild(row);
  });
}

async function createVcsBranch() {
  const panel = document.querySelector('.panel[data-panel="vcs"]');
  const name = panel.querySelector("#vcs-branch-name").value.trim() || (state.activeSpecId || "spec-change");
  try {
    await post("/api/vcs/branch", { projectId: state.activeProjectId || undefined, slug: name });
    await refreshVcs();
    showToast("branch ready");
  } catch (e) { pushError("vcs branch: " + e.message); }
}

async function createVcsCommit() {
  const panel = document.querySelector('.panel[data-panel="vcs"]');
  const message = panel.querySelector("#vcs-commit-msg").value.trim();
  if (!message) { pushError("commit message is required"); return; }
  try {
    await post("/api/vcs/commit", { projectId: state.activeProjectId || undefined, message });
    panel.querySelector("#vcs-commit-msg").value = "";
    await refreshVcs();
    showToast("commit created");
  } catch (e) { pushError("vcs commit: " + e.message); }
}

async function createVcsPr() {
  try {
    const r = await post("/api/vcs/pr", { projectId: state.activeProjectId || undefined, draft: true });
    showToast(r.url ? `PR created: ${r.url}` : "PR created");
  } catch (e) { pushError("vcs pr: " + e.message); }
}

async function runVcsRollback() {
  const panel = document.querySelector('.panel[data-panel="vcs"]');
  const target = panel.querySelector("#vcs-rollback-target").value.trim();
  if (!target) { pushError("rollback target is required"); return; }
  const mode = panel.querySelector("#vcs-rollback-mode").value;
  const confirm = panel.querySelector("#vcs-rollback-confirm").checked;
  try {
    await post("/api/vcs/rollback", { projectId: state.activeProjectId || undefined, target, mode, confirm });
    await refreshVcs();
    showToast("rollback complete");
  } catch (e) { pushError("vcs rollback: " + e.message); }
}

export { wireVcsPanel, refreshVcs, renderVcsPanel, createVcsBranch, createVcsCommit, createVcsPr, runVcsRollback };