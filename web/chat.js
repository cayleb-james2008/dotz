/* dotz — chat / composer / transcript / tool cards / composer palette / pushError + pushInfo.
 * Split from app.js (C7). No behavior change — pure mechanical split.
 */
import { $, el, esc, truncate } from './api.js';
import { state } from './state.js';
import { openPanel } from './panels.js';
import { refreshWorkflowGraph, showNodeDetail } from './graph.js';

function wireChatPanel(node) {
  const input = node.querySelector('[data-role="composer-input"]');
  const sendBtn = node.querySelector('[data-role="send-btn"]');
  const stopBtn = node.querySelector('[data-role="stop-btn"]');
  const transcript = node.querySelector('[data-role="transcript"]');
  state.chatAutoScroll = true;
  const updateFollowTail = () => {
    const distanceFromTail = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight;
    state.chatAutoScroll = distanceFromTail <= 48;
  };
  // Scroll events also fire for our own follow-tail writes. Listen to user intent instead so a
  // fast-growing message cannot accidentally classify a programmatic scroll as "user scrolled up".
  transcript.addEventListener("wheel", (event) => {
    if (event.deltaY < 0) state.chatAutoScroll = false;
    else requestAnimationFrame(updateFollowTail);
  }, { passive: true });
  transcript.addEventListener("pointerdown", () => { state.chatAutoScroll = false; });
  transcript.addEventListener("pointerup", () => requestAnimationFrame(updateFollowTail));
  transcript.addEventListener("keydown", (event) => {
    if (["PageUp", "Home", "ArrowUp"].includes(event.key)) state.chatAutoScroll = false;
    else requestAnimationFrame(updateFollowTail);
  });
  attachComposerPalette(input);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      // When the palette is open, Enter inserts the highlighted item (matching the palette input's
      // own handler) instead of submitting the half-typed trigger text.
      if (isPaletteOpen()) { selectPaletteItem(input); return; }
      sendFrom(input);
    }
    else if (e.key === "Escape") hideCmdPalette();
    else if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Tab") {
      const palette = $("cmd-palette");
      if (!palette.classList.contains("hidden")) { e.preventDefault(); navigatePalette(e.key === "ArrowDown" || e.key === "Tab" ? 1 : -1); }
    }
  });
  sendBtn.onclick = () => sendFrom(input);
  stopBtn.onclick = () => state.ws && state.ws.send(JSON.stringify({ kind: "abort" }));
  const chips = node.querySelector('[data-role="quick-chips"]');
  chips.innerHTML = "";
  ["implement", "scout-and-plan", "implement-and-review"].forEach((cmd) => {
    const chip = el("span", "qchip", "/" + cmd);
    chip.onclick = () => { input.value = "/" + cmd + " "; input.focus(); autoGrow(); };
    chips.appendChild(chip);
  });
}

function autoGrow() {
  const input = activeComposerInput();
  if (!input) return;
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 260) + "px";
}

function sendFrom(input) {
  const text = input.value.trim();
  if (!text) return;
  if (!state.sessionId) {
    pushError("open a project first");
    return;
  }
  if (!state.ws || state.ws.readyState !== WebSocket.OPEN) { pushError("not connected"); return; }
  renderUserMessage({ content: [{ type: "text", text }] });
  state.ws.send(JSON.stringify({ kind: "prompt", text }));
  input.value = "";
  autoGrow();
  hideCmdPalette();
}

function setStreaming(b) {
  state.streaming = b;
  const chat = document.querySelector('.panel[data-panel="chat"]');
  if (chat) {
    const sendBtn = chat.querySelector('[data-role="send-btn"]');
    const stopBtn = chat.querySelector('[data-role="stop-btn"]');
    if (sendBtn) sendBtn.classList.toggle("hidden", b);
    if (stopBtn) stopBtn.classList.toggle("hidden", !b);
  }
  const ccSend = $("send-btn");
  const ccStop = $("stop-btn");
  if (ccSend && ccStop && !ccSend.closest('.panel[data-panel="chat"]')) {
    ccSend.classList.toggle("hidden", b);
    ccStop.classList.toggle("hidden", !b);
  }
  $("st-state").textContent = b ? "streaming…" : "idle";
}

function insertCommand(name) {
  const input = activeComposerInput();
  if (!input) return;
  input.value = ("/" + name + " ").replace("//", "/");
  input.focus();
  autoGrow();
}

function activeComposerInput() {
  const chat = document.querySelector('.panel[data-panel="chat"]');
  if (chat) return chat.querySelector('[data-role="composer-input"]');
  return $("composer-input");
}

/* ---------- composer palette ---------- */
// Reusable per-textarea palette trigger: closes over the passed input so it works for both
// the command-center textarea and each class-scoped chat-panel clone.
function attachComposerPalette(input) {
  if (!input) return;
  input.addEventListener("input", () => {
    autoGrow();
    const before = input.value.slice(0, input.selectionStart);
    const m = before.match(/(^|\s)([/@#])(\w*)$/);
    if (m) {
      const items = paletteItemsFor(m[2], m[3]);
      if (items.length) showCmdPalette(items, m[2], m[3]); else hideCmdPalette();
    } else hideCmdPalette();
  });
  // Only dismiss when focus truly left BOTH the composer and the palette. showCmdPalette() moves
  // focus into #cmd-palette-input, which blurs this textarea — without this guard the palette
  // would force-close itself ~180ms after every open.
  input.addEventListener("blur", (e) => {
    if (e.relatedTarget && e.relatedTarget.closest && e.relatedTarget.closest("#cmd-palette")) return;
    setTimeout(() => {
      const a = document.activeElement;
      if (!(a && a.closest && a.closest("#cmd-palette"))) hideCmdPalette();
    }, 180);
  });
}

function bindComposer() {
  const input = $("composer-input");
  if (!input) return;
  attachComposerPalette(input);
  const pinput = $("cmd-palette-input");
  pinput.addEventListener("input", () => filterPalette(pinput.value));
  pinput.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); navigatePalette(e.key === "ArrowDown" ? 1 : -1); }
    if (e.key === "Enter") { e.preventDefault(); selectPaletteItem(activeComposerInput()); }
    if (e.key === "Escape") hideCmdPalette();
  });
  // Dismiss the palette when focus leaves it entirely (click-away); keep it open for focus moves
  // that stay inside the palette (e.g. clicking a result row).
  pinput.addEventListener("blur", (e) => {
    if (e.relatedTarget && e.relatedTarget.closest && e.relatedTarget.closest("#cmd-palette")) return;
    setTimeout(() => {
      const a = document.activeElement;
      if (!(a && a.closest && a.closest("#cmd-palette"))) hideCmdPalette();
    }, 180);
  });
}

function paletteItemsFor(type, prefix) {
  const p = prefix.toLowerCase();
  if (type === "/") {
    const defaults = ["implement", "scout-and-plan", "implement-and-review", "ultra-code-review", "self-improve", "e2e-test"].map((name) => ({ name, description: "workflow preset", source: "prompt" }));
    const cmds = state.commands.length ? state.commands : defaults;
    return cmds
      .filter((c) => c.name.toLowerCase().startsWith(p))
      .map((c) => ({ icon: "/", name: c.name, desc: c.description || "", source: c.source || "cmd", insert: "/" + c.name + " " }));
  }
  if (type === "@") {
    return state.skills
      .filter((s) => s.name.toLowerCase().startsWith(p))
      .slice(0, 40)
      .map((s) => ({ icon: "@", name: s.name, desc: s.description || "", source: s.source || "skill", insert: "@" + s.name + " " }));
  }
  if (type === "#") {
    return state.projectFiles
      .filter((f) => f.path.toLowerCase().includes(p))
      .slice(0, 40)
      .map((f) => ({ icon: f.type === "dir" ? "▤" : "▥", name: f.path, desc: f.type, source: "file", insert: "#" + f.path + " " }));
  }
  return [];
}

let _paletteItems = [];
let _paletteType = null;
function showCmdPalette(items, type, prefix) {
  _paletteItems = items;
  _paletteType = type;
  const palette = $("cmd-palette");
  const list = $("cmd-palette-list");
  const pinput = $("cmd-palette-input");
  palette.classList.remove("hidden");
  pinput.value = prefix;
  pinput.focus();
  renderPaletteList(items, 0);
}

function filterPalette(prefix) {
  const p = prefix.toLowerCase();
  const filtered = _paletteItems.filter((i) => i.name.toLowerCase().includes(p));
  renderPaletteList(filtered, 0);
}

function renderPaletteList(items, activeIdx) {
  const list = $("cmd-palette-list");
  list.innerHTML = "";
  items.forEach((it, idx) => {
    const row = el("button", "cmd-item" + (idx === activeIdx ? " active" : ""));
    row.appendChild(el("span", "cmd-glyph", it.icon));
    row.appendChild(el("span", "cmd-name", it.name));
    row.appendChild(el("span", "cmd-desc", it.desc));
    row.appendChild(el("span", "cmd-source", it.source));
    row.onclick = () => {
      list.dataset.active = String(idx);
      selectPaletteItem(activeComposerInput());
    };
    list.appendChild(row);
  });
  list.dataset.active = String(activeIdx);
}

function navigatePalette(delta) {
  const list = $("cmd-palette-list");
  const rows = list.querySelectorAll(".cmd-item");
  let active = parseInt(list.dataset.active || "0", 10) || 0;
  active = Math.max(0, Math.min(rows.length - 1, active + delta));
  renderPaletteList(Array.from(rows).map((r) => ({
    icon: r.querySelector(".cmd-glyph").textContent,
    name: r.querySelector(".cmd-name").textContent,
    desc: r.querySelector(".cmd-desc").textContent,
    source: r.querySelector(".cmd-source").textContent,
  })), active);
}

function selectPaletteItem(input) {
  if (!input) return;
  const list = $("cmd-palette-list");
  const rows = list.querySelectorAll(".cmd-item");
  const active = parseInt(list.dataset.active || "0", 10) || 0;
  const selected = rows[active];
  if (!selected) return;
  const name = selected.querySelector(".cmd-name").textContent;
  const insert = (_paletteType === "/" ? "/" : _paletteType) + name + " ";
  const text = input.value;
  const sel = input.selectionStart;
  const before = text.slice(0, sel);
  const m = before.match(/(^|\s)([/@#])(\w*)$/);
  if (!m) return;
  const newBefore = before.slice(0, m.index + m[1].length) + insert;
  input.value = newBefore + text.slice(sel);
  input.selectionStart = input.selectionEnd = newBefore.length;
  input.focus();
  autoGrow();
  hideCmdPalette();
}

function isPaletteOpen() {
  return !$('cmd-palette').classList.contains('hidden');
}

function hideCmdPalette() {
  $("cmd-palette").classList.add("hidden");
  _paletteItems = [];
}

/* ---------- transcript ---------- */
function clearTranscript() {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (t) { t.innerHTML = ""; state.cur = null; state.toolCards = {}; state.chatAutoScroll = true; }
}
function scrollBottom(force = false) {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t || (!force && !state.chatAutoScroll)) return;
  state.chatAutoScroll = true;
  requestAnimationFrame(() => {
    // Re-check inside the frame: a user scroll can occur after this callback was queued by a
    // streaming token. In that case the stale callback must not yank them back to the tail.
    if (force || state.chatAutoScroll) t.scrollTop = t.scrollHeight;
  });
}
function renderUserMessage(message) {
  const text = (message.content || []).map((c) => c.text || "").join("");
  const m = el("div", "msg user");
  m.appendChild(el("div", "msg-role", "you"));
  m.appendChild(el("div", "bubble", text));
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (t) { t.appendChild(m); scrollBottom(true); }
}
function ensureAssistantBubble() {
  if (state.cur && state.cur.bubble.isConnected) return state.cur;
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "dotz"));
  const bubble = el("div", "bubble");
  m.appendChild(bubble);
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t) return state.cur;
  t.appendChild(m);
  state.cur = { bubble, partial: { content: [] } };
  return state.cur;
}
function hasRenderable(partial) {
  return (partial.content || []).some((b) => (b.type === "text" && b.text) || (b.type === "thinking" && b.thinking && b.thinking.trim()));
}
/* Minimal, dependency-free Markdown -> safe HTML for assistant output. Escapes ALL text first,
   then layers block + inline formatting so LLM replies render as real prose instead of a raw blob. */
function renderMarkdown(src) {
  if (src == null) return "";
  const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  const fences = [];
  const s = String(src).replace(/```(\w*)\r?\n?([\s\S]*?)```/g, (_, _lang, code) => {
    fences.push(`<pre class="md-pre"><code>${esc(code.replace(/\n+$/, ""))}</code></pre>`);
    return `\nFENCE_${fences.length - 1}_FENCE\n`;
  });
  const inline = (line) => esc(line)
    .replace(/`([^`]+)`/g, (_, c) => `<code class="md-code">${c}</code>`)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  const out = [];
  let list = null;
  let para = [];
  const flushPara = () => { if (para.length) { out.push(`<p>${para.map(inline).join("<br>")}</p>`); para = []; } };
  const flushList = () => { if (list) { out.push(`<${list.tag}>${list.items.join("")}</${list.tag}>`); list = null; } };
  const flush = () => { flushPara(); flushList(); };
  for (const raw of s.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    let m;
    if ((m = line.match(/^FENCE_(\d+)_FENCE$/))) { flush(); out.push(fences[+m[1]]); continue; }
    if (!line.trim()) { flush(); continue; }
    if ((m = line.match(/^(#{1,4})\s+(.*)$/))) { flush(); out.push(`<h${m[1].length} class="md-h">${inline(m[2])}</h${m[1].length}>`); continue; }
    if ((m = line.match(/^\s*[-*+]\s+(.*)$/))) { flushPara(); if (!list || list.tag !== "ul") { flushList(); list = { tag: "ul", items: [] }; } list.items.push(`<li>${inline(m[1])}</li>`); continue; }
    if ((m = line.match(/^\s*\d+[.)]\s+(.*)$/))) { flushPara(); if (!list || list.tag !== "ol") { flushList(); list = { tag: "ol", items: [] }; } list.items.push(`<li>${inline(m[1])}</li>`); continue; }
    if ((m = line.match(/^>\s?(.*)$/))) { flush(); out.push(`<blockquote>${inline(m[1])}</blockquote>`); continue; }
    if (/^(---+|\*\*\*+|___+)$/.test(line.trim())) { flush(); out.push("<hr>"); continue; }
    flushList(); para.push(line);
  }
  flush();
  return out.join("");
}

/* One-line summary of a tool call for the collapsed card header, so the transcript reads as a
   compact list of actions instead of a wall of raw JSON args + output. */
function toolPreview(name, args) {
  if (!args || typeof args !== "object") return typeof args === "string" ? args : "";
  const a = args;
  switch (name) {
    case "bash": return a.command || "";
    case "read": case "write": case "edit": return a.file_path || a.path || "";
    case "grep": return (a.pattern ? "/" + a.pattern + "/" : "") + (a.path ? " in " + a.path : "");
    case "find": return (a.pattern || "*") + (a.path ? " in " + a.path : "");
    case "ls": return a.path || ".";
    case "subagent": return a.tasks ? `${a.tasks.length} parallel` : a.chain ? `${a.chain.length}-step chain` : (a.agent || "");
    case "skill": return a.name || a.command || "";
    default: { try { const s = JSON.stringify(a); return s.length > 90 ? s.slice(0, 90) + "…" : s; } catch { return ""; } }
  }
}

function renderAssistantPartial(partial) {
  if (!hasRenderable(partial)) return;
  const cur = ensureAssistantBubble();
  cur.partial = partial;
  renderAssistant(cur, true);
}
function renderAssistant(cur, streaming) {
  cur.bubble.innerHTML = "";
  (cur.partial.content || []).forEach((block) => {
    if (block.type === "thinking") {
      if (!block.thinking || !block.thinking.trim()) return;
      const d = el("details", "thinking");
      if (streaming) d.open = true;
      d.appendChild(el("summary", null, "▶ THINKING"));
      d.appendChild(el("div", "think-body", block.thinking));
      cur.bubble.appendChild(d);
    } else if (block.type === "text") {
      const t = el("div", "assistant-text md");
      t.innerHTML = renderMarkdown(block.text || "");
      if (streaming) t.appendChild(el("span", "cursor", "█"));
      cur.bubble.appendChild(t);
    }
  });
}
function finalizeAssistant(message) {
  if (hasRenderable(message) || (message.stopReason === "error" && message.errorMessage)) {
    const cur = ensureAssistantBubble();
    cur.partial = message;
    renderAssistant(cur, false);
    if (message.stopReason === "error" && message.errorMessage) cur.bubble.appendChild(el("div", "msg-error", "⚠ " + message.errorMessage));
  }
  state.cur = null;
}
function toolCard(id, patch) {
  let tc = state.toolCards[id];
  if (!tc) {
    // A <details> so each tool call is a compact, collapsed one-liner (icon · name · preview · status)
    // that the user can expand for full args/output — instead of a wall of raw JSON in the transcript.
    const card = el("details", "toolcard");
    card.dataset.tc = id;
    const head = el("summary", "toolcard-head");
    const glyph = el("span", "toolcard-glyph", "⚙");
    const nameEl = el("span", "toolcard-name", "tool");
    const previewEl = el("span", "toolcard-preview");
    const badge = el("span", "toolcard-status status-run", "● RUN");
    head.appendChild(glyph); head.appendChild(nameEl); head.appendChild(previewEl); head.appendChild(badge);
    const body = el("div", "toolcard-body");
    const argsEl = el("pre", "toolcard-args");
    const outEl = el("pre", "toolcard-out");
    outEl.style.display = "none";
    body.appendChild(argsEl); body.appendChild(outEl);
    card.appendChild(head); card.appendChild(body);
    const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
    if (!t) return;
    t.appendChild(card);
    tc = state.toolCards[id] = { card, nameEl, previewEl, badge, argsEl, outEl, data: {} };
    // Chat→graph cross-link: Alt+click the toolcard head to jump to the workflow graph + select
    // the step that owns this tool call. A plain click just expands the card (no panel yank) so
    // the existing expand-to-read behavior is preserved. The shared toolCallId joins the surfaces.
    head.style.cursor = "pointer";
    head.title = "Alt+click to open on the workflow graph";
    head.addEventListener("click", (ev) => {
      if (!ev.altKey) return; // plain click: let the <details> toggle normally
      ev.preventDefault();
      for (const run of state.workflows.values()) {
        const step = run.steps.find((s) =>
          (s.toolCallIds && s.toolCallIds.includes(id)) ||
          (s._liveTools && s._liveTools[id]) ||
          (s.toolCalls && s.toolCalls.some((c) => c.toolCallId === id))
        );
        if (step) {
          openPanel("graph");
          state.activeWfId = run.id;
          showNodeDetail(run, step);
          refreshWorkflowGraph();
          break;
        }
      }
    });
  }
  Object.assign(tc.data, patch);
  const d = tc.data;
  if (d.name) { tc.nameEl.textContent = d.name; tc.previewEl.textContent = toolPreview(d.name, d.args); }
  if (d.args !== undefined) tc.argsEl.textContent = typeof d.args === "string" ? d.args : JSON.stringify(d.args, null, 2);
  if (d.output) { tc.outEl.style.display = ""; tc.outEl.textContent = d.output; }
  // Live subagent thinking: a compact preview of the subagent's streamed reasoning,
  // shown inline while the subagent is still running.
  if (d.liveThinking !== undefined) {
    if (!tc.liveEl) {
      tc.liveEl = el("div", "toolcard-live");
      tc.liveEl.style.display = "none";
      tc.outEl.parentNode.insertBefore(tc.liveEl, tc.outEl);
    }
    tc.liveEl.style.display = "";
    tc.liveEl.textContent = d.liveThinking;
  }
  const st = d.status || "run";
  tc.card.classList.toggle("is-running", st === "run");
  tc.badge.className = "toolcard-status " + (st === "done" ? "status-done" : st === "err" ? "status-err" : "status-run");
  tc.badge.textContent = st === "done" ? "✓ DONE" : st === "err" ? "✕ ERR" : "● RUN";
}
function showToast(msg) {
  const host = $("toast"); if (!host) { console.error(msg); return; }
  const n = el("div", "toast-item", msg); host.appendChild(n);
  setTimeout(() => n.remove(), 6000);
}
function pushError(msg) {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t) { showToast("⚠ " + msg); return; }
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "system"));
  const b = el("div", "bubble");
  b.appendChild(el("div", "msg-error", "⚠ " + msg));
  m.appendChild(b);
  t.appendChild(m);
  scrollBottom(true);
}

/// A non-error system notice in the chat transcript (used by the run-record/replay UI).
function pushInfo(msg) {
  const t = document.querySelector('.panel[data-panel="chat"] [data-role="transcript"]');
  if (!t) { showToast(msg); return; }
  const m = el("div", "msg assistant");
  m.appendChild(el("div", "msg-role", "system"));
  const b = el("div", "bubble");
  b.appendChild(el("div", "msg-info", msg));
  m.appendChild(b);
  t.appendChild(m);
  scrollBottom(true);
}

export {
  wireChatPanel,
  autoGrow,
  sendFrom,
  setStreaming,
  insertCommand,
  activeComposerInput,
  attachComposerPalette,
  bindComposer,
  paletteItemsFor,
  showCmdPalette,
  filterPalette,
  renderPaletteList,
  navigatePalette,
  selectPaletteItem,
  isPaletteOpen,
  hideCmdPalette,
  clearTranscript,
  scrollBottom,
  renderUserMessage,
  ensureAssistantBubble,
  hasRenderable,
  renderMarkdown,
  toolPreview,
  renderAssistantPartial,
  renderAssistant,
  finalizeAssistant,
  toolCard,
  showToast,
  pushError,
  pushInfo,
};