/* dotz — AGENT BRAIN panel wirer. Split from app.js (C7). No behavior change. */
import { $, el } from '../api.js';
import { state } from '../state.js';
import { refreshStats } from '../handlers.js';
import { pushError, renderUserMessage } from '../chat.js';

function wireBrainPanel(node) {
  node.querySelector("#brain-selfimprove").onclick = () => {
    if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
    const prompt = "Run a recursive self-improvement cycle on this project. Measure a baseline, research opportunities, pick the single highest-leverage improvement, plan it, implement with TDD + checkpoint commits, review with a 5-reviewer fan-out, simplify, verify against baseline, and report. Stop at the plan stage and ask for my approval before implementing.";
    renderUserMessage({ content: [{ type: "text", text: prompt }] });
    state.ws.send(JSON.stringify({ kind: "prompt", text: prompt }));
  };
  refreshStats();
}

function logBrain(msg) {
  const log = document.querySelector('.panel[data-panel="brain"] #brain-log') || $("brain-log");
  if (!log) return;
  const entry = el("div", "brain-log-entry", new Date().toLocaleTimeString() + " " + msg);
  log.appendChild(entry);
  log.scrollTop = log.scrollHeight;
}

export { wireBrainPanel, logBrain };