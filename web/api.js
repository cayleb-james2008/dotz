/* dotz ultra code mode — REST/DOM helpers + token resolution.
 * Split from app.js (C7 modularization). No behavior change — pure mechanical split.
 */
import { state } from './state.js';

const $ = (id) => document.getElementById(id);
const el = (tag, cls, txt) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (txt != null) e.textContent = txt;
  return e;
};
async function api(path, opts) {
  opts = opts || {};
  // Stamp the session token on every API call (backend requires it on /api/* except /api/health).
  if (DOTZ_TOKEN) opts = Object.assign({}, opts, { headers: Object.assign({ "x-dotz-token": DOTZ_TOKEN }, opts.headers) });
  const r = await fetch(path, opts);
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || r.statusText);
  return body;
}
const post = (p, b) => api(p, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(b || {}) });
const patch = (p, b) => api(p, { method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify(b || {}) });
const del = (p) => api(p, { method: "DELETE" });
const esc = (s) => String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const truncate = (s, n) => (s && s.length > n ? s.slice(0, n - 1) + "…" : s || "");

// Loopback session token (plan-015 follow-up). The Tauri shell injects window.DOTZ_TOKEN via its
// initialization script; the serve-bin flow (DOTZ_TOKEN set) carries ?token= in the opened URL.
// Persisted to sessionStorage so an in-app reload keeps working without re-carrying the query.
// Empty string = token disabled (backend enforces nothing) — every call site degrades to the
// exact pre-token behavior.
const DOTZ_TOKEN = (() => {
  let t = "";
  try {
    t = window.DOTZ_TOKEN || new URLSearchParams(location.search).get("token") || sessionStorage.getItem("dotz.token") || "";
  } catch { t = window.DOTZ_TOKEN || ""; }
  if (t) { try { sessionStorage.setItem("dotz.token", t); } catch { /* storage unavailable */ } }
  return t;
})();
// Token as a query suffix for headerless callers (the WS handshake and <img> src cannot set
// request headers). `sep` is "?" or "&" depending on whether the URL already has a query.
const tokenQS = (sep) => (DOTZ_TOKEN ? `${sep}token=${encodeURIComponent(DOTZ_TOKEN)}` : "");

// Project-scoped query-string builder: ?projectId=<active> + extra params. Used by spec /
// living-docs / vcs panels — colocated with tokenQS since both are URL helpers.
function projectQs(extra) {
  const q = new URLSearchParams();
  if (state.activeProjectId) q.set("projectId", state.activeProjectId);
  Object.entries(extra || {}).forEach(([k, v]) => {
    if (v != null && v !== "") q.set(k, v);
  });
  const s = q.toString();
  return s ? "?" + s : "";
}

export { $, el, api, post, patch, del, esc, truncate, DOTZ_TOKEN, tokenQS, projectQs };