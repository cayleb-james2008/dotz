/* dotz — MARKETPLACE panel wirer (C8 + B2). Lists + installs presets from the curated
 * cayleb-james2008/dotz-presets repo. Installed presets land in ~/.dotz/presets/ and are
 * discovered by the existing loaders (skills.rs, templates.rs).
 *
 * C8 shipped the install side. B2 adds the publish flow (author + sign + open a PR) + version
 * pinning (updateAvailable badge). The publish form is built dynamically by JS (no index.html
 * edit needed) — a drawer that overlays the grid when the operator clicks PUBLISH.
 */
import { $, el, api, post, del, truncate } from '../api.js';
import { state } from '../state.js';
import { pushError, showToast } from '../chat.js';

// Kind filter options. The `value` matches the lowercase PresetKind serde tag the backend
// emits; "all" is the no-filter default. Reused for the publish form's kind select.
const KIND_FILTERS = [
  { value: "all", label: "ALL" },
  { value: "profile", label: "PROFILES" },
  { value: "prompt", label: "PROMPTS" },
  { value: "agent", label: "AGENTS" },
  { value: "skill", label: "SKILLS" },
  { value: "design-system", label: "DESIGN SYS" },
  { value: "design-skill", label: "DESIGN SKILL" },
  { value: "plugin", label: "PLUGINS" },
];

// Kind options for the publish form (no "all" — the operator must pick a concrete kind).
const PUBLISH_KINDS = KIND_FILTERS.filter((k) => k.value !== "all");

function wireMarketplacePanel(node) {
  const search = node.querySelector("#marketplace-search");
  if (search) search.addEventListener("input", () => renderMarketplaceGrid(node));

  const filter = node.querySelector("#marketplace-kind-filter");
  if (filter) {
    KIND_FILTERS.forEach((k) => {
      const opt = el("option", null, k.label);
      opt.value = k.value;
      filter.appendChild(opt);
    });
    filter.addEventListener("change", () => renderMarketplaceGrid(node));
  }

  const refresh = node.querySelector("#marketplace-refresh");
  if (refresh) refresh.onclick = () => refreshMarketplace(node);

  // B2: the PUBLISH button opens the publish drawer. Built dynamically so no index.html edit
  // is needed; the drawer is appended to the panel body + removed on close.
  const publishBtn = el("button", "btn-mini btn-go");
  publishBtn.textContent = "PUBLISH";
  publishBtn.title = "author + sign + open a PR for a new preset";
  publishBtn.onclick = () => openPublishDrawer(node);
  // Insert the PUBLISH button right after the refresh button in the toolbar.
  const toolbar = node.querySelector(".templates-toolbar");
  if (toolbar) toolbar.appendChild(publishBtn);

  refreshMarketplace(node);
}

async function refreshMarketplace(node) {
  try {
    const data = await api("/api/presets");
    state.marketplaceCatalog = data.catalog || [];
    state.marketplaceInstalled = data.installed || [];
    renderMarketplaceGrid(node);
  } catch (e) {
    pushError("marketplace load: " + e.message);
    const count = node.querySelector("#marketplace-count");
    if (count) count.textContent = "could not load catalog";
  }
}

function renderMarketplaceGrid(node) {
  const list = node.querySelector("#marketplace-grid");
  const count = node.querySelector("#marketplace-count");
  if (!list || !count) return;
  const search = node.querySelector("#marketplace-search");
  const filter = node.querySelector("#marketplace-kind-filter");
  const q = (search && search.value || "").toLowerCase().trim();
  const kind = (filter && filter.value) || "all";

  const all = state.marketplaceCatalog || [];
  const installed = new Set(state.marketplaceInstalled || []);
  const filtered = all.filter((p) => {
    if (kind !== "all" && p.kind !== kind) return false;
    if (!q) return true;
    return (
      (p.name || "").toLowerCase().includes(q) ||
      (p.description || "").toLowerCase().includes(q) ||
      (p.author || "").toLowerCase().includes(q)
    );
  });

  count.textContent = `${filtered.length} of ${all.length} presets (${installed.size} installed)`;
  list.innerHTML = "";

  if (filtered.length === 0) {
    const empty = el("div", "dim mono");
    empty.style.padding = "1rem";
    empty.textContent = all.length === 0
      ? "no presets available (the marketplace repo may be empty or unreachable)"
      : "no presets match your filter";
    list.appendChild(empty);
    return;
  }

  filtered.forEach((p) => {
    const card = el("div", "marketplace-card");
    if (installed.has(p.name)) card.classList.add("installed");

    const head = el("div", "marketplace-card-head");
    head.appendChild(el("span", "marketplace-card-name", p.name));
    head.appendChild(el("span", "marketplace-card-kind", p.kind || "plugin"));
    if (installed.has(p.name)) {
      head.appendChild(el("span", "marketplace-card-badge", "INSTALLED"));
    }
    // B2: updateAvailable badge — shown when the catalog version is newer than the installed
    // version. The backend stamps this flag at list time.
    if (p.updateAvailable) {
      const upd = el("span", "marketplace-card-badge marketplace-card-badge-update");
      upd.textContent = "UPDATE";
      head.appendChild(upd);
    }
    card.appendChild(head);

    if (p.description) {
      card.appendChild(el("div", "marketplace-card-desc", truncate(p.description, 120)));
    }
    const meta = el("div", "marketplace-card-meta dim mono");
    meta.textContent = [p.author && `by ${p.author}`, p.version && `v${p.version}`]
      .filter(Boolean)
      .join(" · ");
    card.appendChild(meta);

    const actions = el("div", "marketplace-card-actions");
    if (installed.has(p.name)) {
      if (p.updateAvailable) {
        // An update is available → show an UPDATE button (re-install fetches the new version).
        const update = el("button", "btn-mini btn-go");
        update.textContent = "UPDATE";
        update.onclick = (ev) => {
          ev.stopPropagation();
          installPreset(node, p.name, update);
        };
        actions.appendChild(update);
      }
      const uninstall = el("button", "btn-mini btn-stop");
      uninstall.textContent = "UNINSTALL";
      uninstall.onclick = (ev) => {
        ev.stopPropagation();
        uninstallPreset(node, p.name, uninstall);
      };
      actions.appendChild(uninstall);
    } else {
      const install = el("button", "btn-mini btn-go");
      install.textContent = "INSTALL";
      install.onclick = (ev) => {
        ev.stopPropagation();
        installPreset(node, p.name, install);
      };
      actions.appendChild(install);
    }
    card.appendChild(actions);

    list.appendChild(card);
  });
}

// ---- B2: publish drawer ----

/// Open the publish drawer. The drawer is a form overlaid on the panel body with fields for
/// name/kind/description/version + a file list (path + content textarea). The operator adds
/// files one at a time; each file is a {path, content} pair. The "Publish" button POSTs to
/// /api/presets/publish + shows the returned PR URL.
///
/// SECURITY: the publish flow NEVER sends the user's minisign private key to the backend — the
/// signing happens locally (the backend shells out to `minisign` which reads the private key
/// from ~/.dotz/presets-key/). The form only collects preset metadata + file contents.
function openPublishDrawer(node) {
  // Remove any existing drawer (idempotent — a second click replaces the first).
  closePublishDrawer(node);

  const drawer = el("div", "marketplace-publish-drawer");
  drawer.id = "marketplace-publish-drawer";
  drawer.style.cssText = "padding:1rem;border-bottom:1px solid var(--border,#333);";

  const title = el("div", "panel-head dim mono");
  title.textContent = "PUBLISH A PRESET";
  drawer.appendChild(title);

  const hint = el("div", "dim mono");
  hint.style.cssText = "padding:0.25rem 0 0.5rem;font-size:0.8rem;";
  hint.textContent =
    "Authors + signs the preset with your minisign key + opens a PR to cayleb-james2008/dotz-presets. Requires minisign + gh CLIs.";
  drawer.appendChild(hint);

  // Name.
  drawer.appendChild(fieldLabel("name"));
  const nameInput = el("input", "pf-input");
  nameInput.id = "publish-name";
  nameInput.placeholder = "my-preset (lowercase, dashes, max 64)";
  drawer.appendChild(nameInput);

  // Kind.
  drawer.appendChild(fieldLabel("kind"));
  const kindSelect = el("select", "pf-select");
  kindSelect.id = "publish-kind";
  PUBLISH_KINDS.forEach((k) => {
    const opt = el("option", null, k.label);
    opt.value = k.value;
    kindSelect.appendChild(opt);
  });
  drawer.appendChild(kindSelect);

  // Description.
  drawer.appendChild(fieldLabel("description"));
  const descInput = el("input", "pf-input");
  descInput.id = "publish-description";
  descInput.placeholder = "short description";
  drawer.appendChild(descInput);

  // Version.
  drawer.appendChild(fieldLabel("version"));
  const versionInput = el("input", "pf-input");
  versionInput.id = "publish-version";
  versionInput.placeholder = "0.1.0";
  versionInput.value = "0.1.0";
  drawer.appendChild(versionInput);

  // Files list. Each file is a {path, content} pair. Start with one empty file row.
  drawer.appendChild(fieldLabel("files"));
  const filesList = el("div", "marketplace-publish-files");
  filesList.id = "publish-files";
  drawer.appendChild(filesList);
  addPublishFileRow(filesList);

  const addFileBtn = el("button", "btn-mini");
  addFileBtn.textContent = "+ ADD FILE";
  addFileBtn.style.cssText = "margin:0.5rem 0;";
  addFileBtn.onclick = () => addPublishFileRow(filesList);
  drawer.appendChild(addFileBtn);

  // Result area (shows the PR URL or errors).
  const result = el("div", "marketplace-publish-result dim mono");
  result.id = "publish-result";
  result.style.cssText = "padding:0.5rem 0;min-height:1.2rem;";
  drawer.appendChild(result);

  // Buttons: Publish + Cancel.
  const btnRow = el("div", "marketplace-publish-actions");
  btnRow.style.cssText = "display:flex;gap:0.5rem;padding:0.5rem 0;";
  const publishBtn = el("button", "btn-mini btn-go");
  publishBtn.textContent = "PUBLISH";
  publishBtn.onclick = () => doPublish(node, drawer);
  const cancelBtn = el("button", "btn-mini");
  cancelBtn.textContent = "CANCEL";
  cancelBtn.onclick = () => closePublishDrawer(node);
  btnRow.appendChild(publishBtn);
  btnRow.appendChild(cancelBtn);
  drawer.appendChild(btnRow);

  // Insert the drawer at the top of the panel body, before the count + grid.
  const body = node.querySelector(".panel-body");
  if (body) body.insertBefore(drawer, body.querySelector("#marketplace-count"));
}

function fieldLabel(text) {
  const lbl = el("label", "dim mono");
  lbl.style.cssText = "display:block;padding:0.5rem 0 0.25rem;font-size:0.8rem;";
  lbl.textContent = text;
  return lbl;
}

/// Add one file row to the files list: a path input + a content textarea + a remove button.
function addPublishFileRow(filesList) {
  const row = el("div", "marketplace-publish-file-row");
  row.style.cssText = "padding:0.25rem 0;border-bottom:1px solid var(--border,#222);";

  const pathInput = el("input", "pf-input");
  pathInput.placeholder = "path (e.g. SKILL.md)";
  pathInput.style.cssText = "margin-bottom:0.25rem;";
  row.appendChild(pathInput);

  const contentArea = el("textarea", "pf-input");
  contentArea.placeholder = "file content";
  contentArea.style.cssText = "width:100%;min-height:6rem;font-family:monospace;margin-bottom:0.25rem;";
  row.appendChild(contentArea);

  const removeBtn = el("button", "btn-mini btn-stop");
  removeBtn.textContent = "REMOVE FILE";
  removeBtn.style.cssText = "font-size:0.75rem;";
  removeBtn.onclick = () => {
    if (filesList.children.length > 1) {
      filesList.removeChild(row);
    } else {
      showToast("at least one file is required");
    }
  };
  row.appendChild(removeBtn);

  filesList.appendChild(row);
}

/// Collect the form values + POST to /api/presets/publish. Shows the PR URL on success, the
/// error on failure.
async function doPublish(node, drawer) {
  const name = (drawer.querySelector("#publish-name") || {}).value || "";
  const kind = (drawer.querySelector("#publish-kind") || {}).value || "";
  const description = (drawer.querySelector("#publish-description") || {}).value || "";
  const version = (drawer.querySelector("#publish-version") || {}).value || "";
  const filesList = drawer.querySelector("#publish-files");
  const files = [];
  if (filesList) {
    filesList.querySelectorAll(".marketplace-publish-file-row").forEach((row) => {
      const path = (row.querySelector("input") || {}).value || "";
      const content = (row.querySelector("textarea") || {}).value || "";
      if (path.trim()) files.push({ path, content });
    });
  }
  const result = drawer.querySelector("#publish-result");
  if (result) {
    result.textContent = "publishing…";
    result.style.color = "";
  }
  try {
    const res = await post("/api/presets/publish", { name, kind, description, version, files });
    if (result) {
      result.textContent = `PR #${res.prNumber}: ${res.prUrl}`;
      result.style.color = "var(--go,#0f0)";
    }
    showToast(`published ${name} → PR #${res.prNumber}`);
    // Open the PR URL in a new tab so the operator can review it.
    try {
      if (res.prUrl) window.open(res.prUrl, "_blank", "noopener");
    } catch { /* window.open may be blocked — the URL is shown in the result area */ }
  } catch (e) {
    if (result) {
      result.textContent = `error: ${e.message}`;
      result.style.color = "var(--stop,#f00)";
    }
    pushError(`publish: ${e.message}`);
  }
}

function closePublishDrawer(node) {
  const existing = node.querySelector("#marketplace-publish-drawer");
  if (existing) existing.remove();
}

async function installPreset(node, name, btn) {
  if (!name) return;
  const prev = btn.textContent;
  btn.disabled = true;
  btn.textContent = "INSTALLING…";
  try {
    await post("/api/presets/install", { name });
    showToast(`installed ${name}`);
    await refreshMarketplace(node);
    // A newly-installed skill/prompt preset may need a skills index reload so it appears in the
    // SKILLS panel + slash palette without a restart. The backend's skills loader caches the
    // index; fetching /api/skills here would NOT rebuild it (it's a one-shot cache). A future
    // follow-up could expose a "reload skills" endpoint; for now the preset appears after the
    // next skills-panel refresh. # ponytail: skills reload-on-install is the follow-up.
  } catch (e) {
    pushError(`install ${name}: ${e.message}`);
    btn.disabled = false;
    btn.textContent = prev;
  }
}

async function uninstallPreset(node, name, btn) {
  if (!name) return;
  const prev = btn.textContent;
  btn.disabled = true;
  btn.textContent = "REMOVING…";
  try {
    await del(`/api/presets/${encodeURIComponent(name)}`);
    showToast(`removed ${name}`);
    await refreshMarketplace(node);
  } catch (e) {
    pushError(`uninstall ${name}: ${e.message}`);
    btn.disabled = false;
    btn.textContent = prev;
  }
}

export { wireMarketplacePanel, refreshMarketplace, renderMarketplaceGrid, installPreset, uninstallPreset, openPublishDrawer };