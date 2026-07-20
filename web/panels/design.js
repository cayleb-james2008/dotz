/* dotz — DESIGN panel wirer (native Open Design). Split from app.js (C7). No behavior change. */
import { $, el, api, esc, truncate, DOTZ_TOKEN } from '../api.js';
import { state } from '../state.js';
import { pushError } from '../chat.js';

function wireDesignPanel(node) {
  const search = node.querySelector("#design-search");
  if (search) search.addEventListener("input", () => renderDesignList(search.value.toLowerCase()));
  const eh = node.querySelector("#design-export-html");
  const ep = node.querySelector("#design-export-pdf");
  if (eh) eh.onclick = exportDesignHtml;
  if (ep) ep.onclick = exportDesignPdf;
  refreshDesignSystems();
}

async function refreshDesignSystems() {
  try {
    const { systems } = await api("/api/design/systems");
    state.designSystems = systems || [];
    renderDesignList("");
  } catch (e) { pushError("design systems load: " + e.message); }
}

function renderDesignList(filter) {
  const panel = document.querySelector('.panel[data-panel="design"]');
  const list = panel ? panel.querySelector("#design-list") : null;
  const count = panel ? panel.querySelector("#design-count") : null;
  if (!list || !count) return;
  const all = state.designSystems || [];
  const f = filter
    ? all.filter((s) => (s.name || "").toLowerCase().includes(filter) || (s.category || "").toLowerCase().includes(filter) || (s.id || "").toLowerCase().includes(filter))
    : all;
  count.textContent = `${f.length} of ${all.length} design systems`;
  list.innerHTML = "";
  f.forEach((s) => {
    const item = el("div", "skill-item");
    const head = el("div", "skill-item-head");
    head.appendChild(el("span", "skill-name", s.name || s.id));
    if (s.category) head.appendChild(el("span", "skill-source design", s.category));
    item.appendChild(head);
    if (s.description) item.appendChild(el("div", "skill-desc", truncate(s.description, 100)));
    item.onclick = () => loadDesignPreview(s.id, s.name || s.id);
    list.appendChild(item);
  });
}

async function loadDesignPreview(id, label) {
  const panel = document.querySelector('.panel[data-panel="design"]');
  const iframe = panel ? panel.querySelector("#design-iframe") : null;
  const lbl = panel ? panel.querySelector("#design-preview-label") : null;
  if (!iframe) return;
  if (lbl) lbl.textContent = label || id;
  try {
    // Raw fetch on purpose: this endpoint returns HTML for the iframe srcdoc, and api()
    // JSON-parses the body. Still needs the session token like every other /api call.
    const r = await fetch(`/api/design/systems/${encodeURIComponent(id)}/components`, DOTZ_TOKEN ? { headers: { "x-dotz-token": DOTZ_TOKEN } } : undefined);
    if (!r.ok) throw new Error("preview unavailable");
    iframe.srcdoc = await r.text();
  } catch (e) {
    iframe.srcdoc = `<p style="font-family:monospace;color:#888;padding:1rem">no preview: ${esc(e.message)}</p>`;
  }
}

// Export the current preview. HTML = a Blob download; PDF = native print of the same-origin srcdoc
// iframe (zero deps). PNG export is deferred to Stage 2 (needs rasterization / Chromium).
function exportDesignHtml() {
  const panel = document.querySelector('.panel[data-panel="design"]');
  const iframe = panel ? panel.querySelector("#design-iframe") : null;
  const html = iframe && iframe.srcdoc;
  if (!html) { pushError("open a design preview first"); return; }
  const blob = new Blob([html], { type: "text/html" });
  const a = el("a");
  a.href = URL.createObjectURL(blob);
  a.download = "design-artifact.html";
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
}

function exportDesignPdf() {
  const panel = document.querySelector('.panel[data-panel="design"]');
  const iframe = panel ? panel.querySelector("#design-iframe") : null;
  if (!iframe || !iframe.srcdoc) { pushError("open a design preview first"); return; }
  try { iframe.contentWindow.focus(); iframe.contentWindow.print(); }
  catch (e) { pushError("print failed: " + e.message); }
}

export { wireDesignPanel, refreshDesignSystems, renderDesignList, loadDesignPreview, exportDesignHtml, exportDesignPdf };