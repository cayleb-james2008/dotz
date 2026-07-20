/* dotz — SANDBOX panel wirer. Split from app.js (C7). No behavior change.
 * The sandbox event handler + run/output/cursor helpers live in handlers.js (handleSandboxEvent
 * is the primary driver); this module imports them. The render/run helpers are exported from
 * handlers.js so wireSandboxPanel, handleSandboxEvent, and the node-detail drawer share one impl.
 */
import { el } from '../api.js';
import { state } from '../state.js';
import { loadSandboxLanguages, runSandbox, killActiveRun, toggleWebPreview } from '../handlers.js';

function wireSandboxPanel(node) {
  const langSel = node.querySelector("#sb-lang");
  if (state.sandbox.languages.length) {
    langSel.innerHTML = "";
    state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; langSel.appendChild(o); });
  } else {
    loadSandboxLanguages().then(() => {
      langSel.innerHTML = "";
      state.sandbox.languages.forEach((l) => { const o = el("option", null, l); o.value = l; langSel.appendChild(o); });
    });
  }
  node.querySelector("#sb-run").onclick = () => runSandbox();
  node.querySelector("#sb-kill").onclick = () => killActiveRun();
  node.querySelector("#sb-mode").onchange = (e) => { state.sandbox.mode = e.target.value; toggleWebPreview(e.target.value === "web"); };
}

export { wireSandboxPanel };