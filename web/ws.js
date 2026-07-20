/* dotz — WebSocket connect + message dispatch by m.kind + auto-reconnect.
 * Split from app.js (C7). No behavior change — pure mechanical split.
 */
import { state } from './state.js';
import { tokenQS } from './api.js';
import {
  setConn, handleEvent, handleSandboxEvent, handleWorkflowEvent,
  showGateCard, handleMemoryRecall, handleProviderHealthEvent,
} from './handlers.js';
import { openPanel } from './panels.js';
import { renderBrowserObservation } from './panels/browser.js';
import { setStreaming, pushError } from './chat.js';

let _wsAttempt = 0, _wsIntentional = false;
function connectWS() {
  if (state.ws) { _wsIntentional = true; try { state.ws.close(); } catch {} }
  _wsIntentional = false;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws?sessionId=${state.sessionId}${tokenQS("&")}`);
  state.ws = ws;
  setConn("off", "ws connecting…");
  ws.onopen = () => {
    const reconnected = _wsAttempt > 0;
    _wsAttempt = 0;
    setConn("on", "ws ✓ " + location.host);
    // A reconnect gets no event replay; if the turn ended while we were offline its agent_end was
    // lost, leaving the UI stuck in 'streaming'. Restore SEND — a still-running turn re-clears on its
    // eventual agent_end.
    if (reconnected && state.streaming) setStreaming(false);
  };
  ws.onclose = () => {
    setConn("off", "ws closed");
    if (!_wsIntentional && state.sessionId && ws === state.ws) {
      const delay = Math.min(15000, 500 * 2 ** _wsAttempt++);
      setConn("off", "reconnecting…");
      setTimeout(connectWS, delay);
    }
  };
  ws.onerror = () => setConn("err", "ws error");
  ws.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return; }
    if (m.kind === "ready") { /* session confirmed */ }
    else if (m.kind === "event") handleEvent(m.event);
    else if (m.kind === "sandbox") handleSandboxEvent(m.event);
    else if (m.kind === "workflow") {
      // The server broadcasts every run's events to every socket. Ignore runs that aren't this
      // client's: workflow_start carries event.run (filter by sessionId/projectId); later events
      // (step_state/end) lack it, so gate on whether this client is already tracking the run.
      const r = m.event && m.event.run;
      // Gate null explicitly: state.sessionId / activeProjectId default to null, and a run created
      // with null sessionId+projectId would otherwise match EVERY client via null===null, bleeding
      // other sessions' runs into this client's graph.
      if (r ? ((r.sessionId != null && r.sessionId === state.sessionId) || (r.projectId != null && r.projectId === state.activeProjectId)) : state.workflows.has(m.runId)) {
        handleWorkflowEvent(m.runId, m.event);
      }
    }
    else if (m.kind === "browser") renderBrowserObservation(m.event);
    else if (m.kind === "gate") showGateCard(m.gateId, m.plan);
    else if (m.kind === "memory_recall") handleMemoryRecall(m);
    else if (m.kind === "provider_health") handleProviderHealthEvent(m);
    else if (m.kind === "panel_open") { if (m.panelName) openPanel(m.panelName); }
    else if (m.kind === "error") {
      // A dead session (the server/exe restarted, so this sessionId no longer exists) will never
      // come back by reconnecting — stop the exponential-reconnect loop and tell the user once,
      // instead of spamming the transcript with "no such session" on every retry forever.
      if (/no such session/i.test(m.error || "")) {
        _wsIntentional = true;   // onclose checks this and skips the reconnect
        state.sessionId = null;  // also fails onclose's state.sessionId guard
        pushError("session ended (server restarted) — reopen the project to continue");
      } else pushError(m.error);
    }
  };
}

export { connectWS };