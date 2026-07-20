/* dotz — DOCTRINE panel wirer (AGENTS.md editor). Split from app.js (C7). No behavior change. */
import { api, patch } from '../api.js';
import { state } from '../state.js';
import { renderMarkdown } from '../chat.js';
import { pushError, showToast } from '../chat.js';

function wireDoctrinePanel(node) {
  const editor = node.querySelector("#doctrine-editor");
  const preview = node.querySelector("#doctrine-preview");
  const status = node.querySelector("#doctrine-status");
  const saveBtn = node.querySelector("#doctrine-save");
  const reloadBtn = node.querySelector("#doctrine-reload");
  if (!editor || !preview || !status || !saveBtn || !reloadBtn) return;

  editor.addEventListener("input", () => {
    preview.innerHTML = renderMarkdown(editor.value);
    status.textContent = "unsaved";
    status.classList.remove("dim");
    status.classList.add("yellow");
  });

  saveBtn.onclick = async () => {
    if (!state.activeProjectId) { pushError("open a project first"); return; }
    status.textContent = "saving…";
    status.classList.remove("yellow");
    try {
      await patch("/api/agents_md?projectId=" + encodeURIComponent(state.activeProjectId), { content: editor.value });
      status.textContent = "saved";
      status.classList.add("dim");
      showToast("AGENTS.md saved");
    } catch (e) {
      status.textContent = "save failed";
      status.classList.add("red");
      pushError("doctrine save: " + e.message);
    }
  };

  reloadBtn.onclick = () => loadDoctrine(editor, preview, status);
  loadDoctrine(editor, preview, status);
}

async function loadDoctrine(editor, preview, status) {
  if (!state.activeProjectId) {
    status.textContent = "no project";
    editor.value = "";
    preview.innerHTML = "";
    return;
  }
  status.textContent = "loading…";
  status.classList.remove("red", "yellow");
  try {
    const { content } = await api("/api/agents_md?projectId=" + encodeURIComponent(state.activeProjectId));
    editor.value = content || "";
    preview.innerHTML = renderMarkdown(content || "");
    status.textContent = "loaded";
    status.classList.add("dim");
  } catch (e) {
    status.textContent = "load failed";
    status.classList.add("red");
    pushError("doctrine load: " + e.message);
  }
}

export { wireDoctrinePanel, loadDoctrine };