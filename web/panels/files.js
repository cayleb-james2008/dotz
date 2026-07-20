/* dotz — FILES panel wirer + project-file tree helpers. Split from app.js (C7). No behavior change.
 * loadProjectFiles / flattenFileTree are exported because newSession (main.js) and the composer
 * palette (chat.js) consume the flattened file list — not just this panel.
 */
import { $, el, api } from '../api.js';
import { state } from '../state.js';

async function loadProjectFiles() {
  if (!state.activeProjectId) return;
  try {
    const { tree } = await api(`/api/projects/${state.activeProjectId}/files`);
    state.projectFiles = flattenFileTree(tree);
    // NOTE: do NOT call renderFiles() here — renderFiles() fetches the tree itself, and re-entering
    // it would create a load→render→load loop (double fetch per render). This only feeds the '#' palette.
  } catch (e) { /* fail silently */ }
}

function flattenFileTree(tree, prefix = "") {
  let out = [];
  (tree || []).forEach((n) => {
    const p = prefix ? prefix + "/" + n.path.split(/[\\/]/).pop() : n.path.split(/[\\/]/).pop();
    out.push({ path: p, type: n.type });
    if (n.children) out = out.concat(flattenFileTree(n.children, p));
  });
  return out;
}

function wireFilesPanel(node) {
  renderFiles();
}

function renderFiles() {
  const panel = document.querySelector('.panel[data-panel="files"]');
  const treeEl = panel ? panel.querySelector("#files-tree") : $("files-tree");
  if (!treeEl) return;
  if (!state.activeProjectId) {
    treeEl.innerHTML = "";
    treeEl.appendChild(el("div", "dim mono", "no project open"));
    return;
  }
  const build = (nodes) => {
    const wrap = el("div", "ft-children");
    (nodes || []).forEach((n) => {
      const isDir = n.type === "dir";
      const row = el("div", "ft-row" + (isDir ? " ft-dir" : ""));
      row.appendChild(el("span", "ft-icon", isDir ? "▾" : "▹"));
      const name = n.path.split(/[\\/]/).pop();
      row.appendChild(el("span", "ft-name", name));
      wrap.appendChild(row);
      if (isDir && n.children && n.children.length) {
        const childWrap = build(n.children);
        childWrap.style.display = "";
        row.onclick = () => {
          const hidden = childWrap.style.display === "none";
          childWrap.style.display = hidden ? "" : "none";
          row.querySelector(".ft-icon").textContent = hidden ? "▾" : "▸";
        };
        wrap.appendChild(childWrap);
      }
    });
    return wrap;
  };
  // Single fetch: paints the tree AND refreshes state.projectFiles for the '#' palette (no recursion).
  api(`/api/projects/${state.activeProjectId}/files`).then(({ tree }) => {
    state.projectFiles = flattenFileTree(tree);
    treeEl.innerHTML = "";
    if (!tree.length) { treeEl.appendChild(el("div", "dim mono", "empty directory")); return; }
    const root = build(tree);
    root.className = "files-tree";
    treeEl.appendChild(root);
  }).catch(() => { treeEl.innerHTML = ""; treeEl.appendChild(el("div", "dim mono", "failed to load files")); });
}

export { wireFilesPanel, renderFiles, loadProjectFiles, flattenFileTree };