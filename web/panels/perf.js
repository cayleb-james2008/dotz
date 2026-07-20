/* dotz — PERFORMANCE panel wirer (B3). Local-only, opt-in perf dashboard.
 * Surfaces per-turn latency, graph render time, embed latency, tool-call latency,
 * and first-turn (cold) latency as p50/p95/p99/max cards + inline-SVG sparklines.
 * Privacy moat: the panel shows a "Local-only · opt-in · never sent remotely"
 * badge and a Recording ON/OFF toggle that calls POST /api/perf/toggle.
 *
 * # ponytail: client-side perf recording via POST; a WS-based `perf_sample` stream
 * is the upgrade path (the graph already posts step_state over WS, so a perf
 * event would slot in naturally — but a single POST endpoint is the shortest
 * working diff for B3).
 */
import { api, post, el } from '../api.js';
import { state } from '../state.js';
import { pushError } from '../chat.js';

// The five metric cards, in render order. The `key` matches the backend
// PerfMetric wire tag + the summary response's metrics map key.
const PERF_METRICS = [
  { key: "turn_latency",       label: "TURN",       unit: "ms" },
  { key: "graph_render_time",  label: "GRAPH",     unit: "ms" },
  { key: "embed_latency",      label: "EMBED",     unit: "ms" },
  { key: "tool_call_latency",  label: "TOOL",      unit: "ms" },
  { key: "first_turn_latency", label: "FIRST TURN", unit: "ms" },
];

let perfRefreshTimer = null;

function wirePerfPanel(node) {
  const grid = node.querySelector("#perf-grid");
  const badge = node.querySelector("#perf-privacy");
  const toggleBtn = node.querySelector("#perf-toggle");
  const clearBtn = node.querySelector("#perf-clear");
  const status = node.querySelector("#perf-status");
  if (!grid || !badge || !toggleBtn || !clearBtn || !status) return;

  toggleBtn.onclick = async () => {
    try {
      const { recording } = await post("/api/perf/toggle");
      state.perfRecording = !!recording;
      updateToggleUI(node, !!recording);
      await refreshPerf(node);
    } catch (e) {
      pushError("perf toggle: " + e.message);
    }
  };

  clearBtn.onclick = async () => {
    try {
      await post("/api/perf/clear");
      await refreshPerf(node);
    } catch (e) {
      pushError("perf clear: " + e.message);
    }
  };

  // Initial render + auto-refresh every 5s. The interval is cleared on unmount
  // (via the mutation observer below) so a closed panel does not leak a timer.
  refreshPerf(node);
  if (perfRefreshTimer) clearInterval(perfRefreshTimer);
  perfRefreshTimer = setInterval(() => refreshPerf(node), 5000);

  // Unmount detection: when the panel node is removed from the DOM, clear the
  // auto-refresh timer so it doesn't keep firing against a detached node.
  const mo = new MutationObserver(() => {
    if (!node.isConnected) {
      if (perfRefreshTimer) { clearInterval(perfRefreshTimer); perfRefreshTimer = null; }
      mo.disconnect();
    }
  });
  mo.observe(node.parentNode || document.body, { childList: true, subtree: false });
}

async function refreshPerf(node) {
  const status = node.querySelector("#perf-status");
  try {
    const summary = await api("/api/perf/summary");
    state.perfSummary = summary;
    renderGrid(node, summary);
    if (status) {
      const on = state.perfRecording;
      status.textContent = on ? "Recording ON" : "Recording OFF";
      status.classList.toggle("dim", !on);
    }
  } catch (e) {
    if (status) status.textContent = "load failed";
    pushError("perf summary: " + e.message);
  }
}

function updateToggleUI(node, recording) {
  state.perfRecording = recording;
  const badge = node.querySelector("#perf-privacy");
  const toggleBtn = node.querySelector("#perf-toggle");
  const status = node.querySelector("#perf-status");
  if (badge) badge.textContent = recording
    ? "Local-only · opt-in · recording ON"
    : "Local-only · opt-in · never sent remotely";
  if (toggleBtn) toggleBtn.textContent = recording ? "STOP" : "RECORD";
  if (status) {
    status.textContent = recording ? "Recording ON" : "Recording OFF";
    status.classList.toggle("dim", !recording);
  }
}

function renderGrid(node, summary) {
  const grid = node.querySelector("#perf-grid");
  if (!grid) return;
  grid.innerHTML = "";
  const metrics = (summary && summary.metrics) || {};
  for (const m of PERF_METRICS) {
    const data = metrics[m.key] || { count: 0 };
    // Inline-styled card (ponytail: no styles.css dependency — keep the panel
    // self-contained; promote to a .perf-card class if more perf widgets land).
    const card = el("div", null);
    card.style.cssText = "border:1px solid var(--border);border-radius:var(--radius-sm);padding:10px;background:var(--surface);";
    // Header: label + count.
    const head = el("div", null);
    head.style.cssText = "display:flex;justify-content:space-between;align-items:baseline;margin-bottom:8px;";
    const lbl = el("span", "mono", m.label);
    lbl.style.cssText = "font-size:11px;letter-spacing:1px;color:var(--lav);";
    head.appendChild(lbl);
    const cnt = el("span", "dim mono", data.count > 0 ? `${data.count} samples` : "no data");
    cnt.style.cssText = "font-size:10px;";
    head.appendChild(cnt);
    card.appendChild(head);
    // Stats: p50 / p95 / p99 / max.
    const stats = el("div", null);
    stats.style.cssText = "display:grid;grid-template-columns:1fr 1fr;gap:4px 8px;margin-bottom:8px;";
    for (const [k, lbl2] of [["p50","p50"],["p95","p95"],["p99","p99"],["max","max"]]) {
      const v = data[k];
      const cell = el("div", null);
      cell.style.cssText = "display:flex;justify-content:space-between;";
      const k1 = el("span", "dim mono", lbl2);
      k1.style.cssText = "font-size:10px;";
      const v1 = el("span", "mono", v == null ? "—" : `${fmt(v)}${m.unit}`);
      v1.style.cssText = "font-size:11px;color:var(--text);";
      cell.appendChild(k1);
      cell.appendChild(v1);
      stats.appendChild(cell);
    }
    card.appendChild(stats);
    // Sparkline (inline SVG, fetched lazily).
    const spark = el("div", null);
    spark.style.cssText = "height:28px;";
    spark.dataset.metric = m.key;
    card.appendChild(spark);
    if (data.count > 0) fetchSparkline(spark, m.key);
    grid.appendChild(card);
  }
}

function fmt(v) {
  if (v == null) return "—";
  return Number(v).toFixed(v < 10 ? 1 : 0);
}

async function fetchSparkline(container, metric) {
  try {
    const samples = await api(`/api/perf/samples?metric=${encodeURIComponent(metric)}&limit=50`);
    const svg = sparklineSvg(Array.isArray(samples) ? samples : [], 120, 28);
    container.innerHTML = svg;
  } catch {
    container.innerHTML = "";
  }
}

// Render a small inline-SVG sparkline from an array of {value_ms} samples.
// Normalizes values to [0,1] over the min..max range and maps to an SVG path.
// No dep — pure stdlib DOM + string concat. # ponytail: a charting lib is the
// upgrade path if we need axes/interaction; for a 120x28 trend this is enough.
function sparklineSvg(samples, w, h) {
  if (!samples || samples.length < 2) return "";
  const vals = samples.map((s) => Number(s.value_ms) || 0);
  const min = Math.min(...vals);
  const max = Math.max(...vals);
  const range = max - min || 1; // avoid /0 for a flat line
  const n = vals.length;
  const stepX = w / (n - 1);
  const points = vals.map((v, i) => {
    const x = i * stepX;
    const y = h - ((v - min) / range) * h;
    return `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
  }).join(" ");
  return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" class="perf-spark-svg" preserveAspectRatio="none">` +
    `<polyline points="${points}" fill="none" stroke="var(--cyan)" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"/>` +
    `</svg>`;
}

export { wirePerfPanel, refreshPerf };