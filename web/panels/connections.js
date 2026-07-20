/* dotz — CONNECTIONS panel wirer (local CLI login status). Split from app.js (C7). No behavior change. */
import { el, api } from '../api.js';
import { state } from '../state.js';
import { wireProviderKeyForm, refreshProviderKeys } from '../provider-key.js';

function wireConnectionsPanel(node) {
  refreshConnections();
  if (state.connectionsPollTimer) clearInterval(state.connectionsPollTimer);
  state.connectionsPollTimer = setInterval(refreshConnections, 4000);
  wireProviderKeyForm(node);
  refreshProviderKeys(node);
}

async function refreshConnections() {
  const panel = document.querySelector('.panel[data-panel="connections"]');
  if (!panel) return;
  try {
    const { connections } = await api("/api/connections");
    state.connectionsLoaded = true;
    renderConnections(panel, connections || []);
  } catch (e) {
    // Keep the last good render on a transient poll failure — but if the FIRST load fails, replace the
    // "checking local logins…" seed with an error state instead of leaving it stuck forever.
    if (!state.connectionsLoaded) {
      const list = panel.querySelector("#conn-list");
      if (list) { list.innerHTML = ""; list.appendChild(el("div", "dim mono", "⚠ couldn't reach the server — retrying…")); }
    }
  }
}

function renderConnections(panel, connections) {
  const list = panel.querySelector("#conn-list");
  if (!list) return;
  list.innerHTML = "";
  if (!connections.length) { list.appendChild(el("div", "dim mono", "no providers")); return; }
  for (const c of connections) {
    const row = el("div", "conn-row");
    row.appendChild(el("span", "conn-dot " + (c.loggedIn ? "ok" : c.installed ? "off" : "missing")));
    const meta = el("div", "conn-meta");
    meta.appendChild(el("span", "conn-name", c.label));
    const sub = c.loggedIn
      ? "connected" + (c.account ? " · " + c.account : "")
      : (c.hint || (c.installed ? "not logged in" : "CLI not installed"));
    meta.appendChild(el("span", "conn-sub dim mono", sub));
    row.appendChild(meta);
    if (!c.loggedIn) {
      // In-app login/logout is disabled server-side (the routes 501 by design — a spawned
      // provider login has logged the operator out before). Point at the terminal instead
      // of rendering buttons that can only fail.
      const actions = el("div", "conn-actions");
      actions.appendChild(el("span", "conn-sub dim mono", "connect from a terminal — dotz reads the CLI's auth"));
      row.appendChild(actions);
    }
    list.appendChild(row);
  }
}

export { wireConnectionsPanel, refreshConnections, renderConnections };