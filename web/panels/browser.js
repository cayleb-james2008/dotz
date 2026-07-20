/* dotz — BROWSER panel wirer + observation renderer. Split from app.js (C7). No behavior change.
 * renderBrowserObservation is exported because the WS dispatch (ws.js, m.kind === "browser") calls
 * it directly — the browser panel is the only panel whose primary render path is a WS event.
 */
import { el, api, tokenQS } from '../api.js';
import { state } from '../state.js';
import { pushError } from '../chat.js';

function wireBrowserPanel(node) {
  const url = node.querySelector("#br-url");
  const shot = node.querySelector("#br-shot");
  const typeText = node.querySelector("#br-type-text");
  const runAct = async (input) => {
    if (!state.browserSessionId || state.browserBusy) return;
    state.browserBusy = true;
    try {
      const observation = await browserAction("act", { sessionId: state.browserSessionId, ...input });
      renderBrowserObservation(observation);
      return observation;
    } catch (e) { pushError("browser action: " + e.message); }
    finally { state.browserBusy = false; }
  };
  const startOrNavigate = async () => {
    // Guard the start path against a double-click: browserSessionId isn't set until the first start
    // resolves, so without this a second click would spawn a SECOND browser session. (runAct already
    // guards navigate/back/etc.; this covers the initial start.)
    if (state.browserBusy) return;
    const target = url.value.trim();
    if (!target || !state.activeProjectId) return pushError("browser: open a project and enter an http(s) URL");
    try {
      if (state.browserSessionId) {
        await runAct({ action: "navigate", url: target });
      } else {
        const origin = new URL(target).origin;
        state.browserBusy = true;
        const observation = await browserAction("start", { projectId: state.activeProjectId, url: target, allowedOrigins: [origin] });
        renderBrowserObservation(observation);
      }
    } catch (e) { pushError("browser start: " + e.message); }
    finally { state.browserBusy = false; }
  };
  node.querySelector("#br-navigate").onclick = startOrNavigate;
  node.querySelector("#br-back").onclick = () => runAct({ action: "back" });
  node.querySelector("#br-forward").onclick = () => runAct({ action: "forward" });
  node.querySelector("#br-reload").onclick = () => runAct({ action: "reload" });
  node.querySelector("#br-shot-btn").onclick = () => runAct({ action: "observe" });
  node.querySelector("#br-stop").onclick = async () => {
    if (!state.browserSessionId) return;
    try {
      state.browserBusy = true;
      renderBrowserObservation(await browserAction("stop", { sessionId: state.browserSessionId }));
    }
    catch (e) { pushError("browser stop: " + e.message); }
    finally { state.browserBusy = false; }
  };

  const sendType = () => {
    const text = typeText.value;
    if (!text || !state.browserObservation) return;
    typeText.value = "";
    runAct({ action: "type", text, expectedSeq: state.browserObservation.seq });
  };
  node.querySelector("#br-type-send").onclick = sendType;
  typeText.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); sendType(); } });
  node.querySelector("#br-scroll-up").onclick = () => runAct({ action: "scroll", direction: "up", pixels: 500 });
  node.querySelector("#br-scroll-down").onclick = () => runAct({ action: "scroll", direction: "down", pixels: 500 });

  shot.addEventListener("click", (e) => {
    if (!state.browserObservation || state.browserObservation.status !== "ready") return;
    const rect = shot.getBoundingClientRect();
    const viewport = state.browserObservation.page?.viewport;
    if (!rect.width || !rect.height || !viewport?.width || !viewport?.height) return;
    const x = Math.round((e.clientX - rect.left) / rect.width * viewport.width);
    const y = Math.round((e.clientY - rect.top) / rect.height * viewport.height);
    shot.focus();
    runAct({ action: "clickAt", x, y, expectedSeq: state.browserObservation.seq });
  });
  shot.addEventListener("keydown", (e) => {
    if (!state.browserObservation || !["Enter", "Tab", "Escape", "Backspace", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"].includes(e.key)) return;
    e.preventDefault();
    runAct({ action: "key", key: e.key });
  });
  url.addEventListener("keydown", (e) => { if (e.key === "Enter") startOrNavigate(); });
  if (state.browserPollTimer) clearInterval(state.browserPollTimer);
  state.browserPollTimer = setInterval(refreshBrowserScreenshot, 1200);
  refreshBrowserScreenshot();
}

async function browserAction(action, body) {
  const path = "/api/browser/" + action;
  const opts = { method: body ? "POST" : "GET", headers: {} };
  if (body) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
  return api(path, opts);
}

async function refreshBrowserScreenshot() {
  const panel = document.querySelector('.panel[data-panel="browser"]');
  if (!panel || state.browserBusy) return;
  try {
    const query = state.browserSessionId ? `?sessionId=${encodeURIComponent(state.browserSessionId)}` : "";
    const result = await browserAction("state" + query);
    renderBrowserObservation(result.observation);
  } catch (e) { /* transient poll failure (backend down / network blip); keep the last render — do NOT flood the chat */ }
}

function renderBrowserObservation(observation) {
  const panel = document.querySelector('.panel[data-panel="browser"]');
  if (!panel) return;
  const shot = panel.querySelector("#br-shot");
  const ph = panel.querySelector("#br-placeholder");
  if (!observation) {
    state.browserSessionId = null;
    state.browserObservation = null;
    shot.classList.add("hidden"); ph.classList.remove("hidden");
    ph.textContent = "waiting for a Pi browser session";
    return;
  }
  state.browserSessionId = observation.status === "stopped" ? null : observation.sessionId;
  state.browserObservation = observation.status === "stopped" ? null : observation;
  panel.querySelector("#br-url").value = observation.page?.url || "";
  panel.querySelector("#br-navigate").textContent = state.browserSessionId ? "GO" : "START";
  for (const id of ["br-back", "br-forward", "br-reload", "br-shot-btn", "br-stop", "br-scroll-up", "br-scroll-down", "br-type-text", "br-type-send"]) {
    const control = panel.querySelector("#" + id);
    if (control) control.disabled = !state.browserSessionId;
  }
  const action = observation.currentAction;
  const owner = observation.owner || {};
  ph.textContent = `${observation.status} | ${owner.projectId || "unowned"} | seq ${observation.seq}`;
  if (observation.frame?.available && observation.status !== "stopped") {
    shot.src = `/api/browser/frame?sessionId=${encodeURIComponent(observation.sessionId)}&afterSeq=${Math.max(-1, observation.frame.seq - 1)}&t=${observation.frame.seq}${tokenQS("&")}`;
    shot.classList.remove("hidden"); ph.classList.add("hidden");
  } else { shot.classList.add("hidden"); ph.classList.remove("hidden"); }
  let details = panel.querySelector(".browser-observation");
  if (!details) { details = el("div", "browser-observation mono"); panel.querySelector(".browser-body").appendChild(details); }
  const errors = [...(observation.consoleErrors || []), ...(observation.networkErrors || [])];
  const refs = (observation.elements || []).slice(0, 10).map((item) => `@${item.ref} ${item.role} ${item.name}`).join("\n");
  details.textContent = [
    `owner ${owner.projectId || "-"} / workflow ${owner.workflowId || "-"}`,
    `viewport ${observation.page?.viewport?.width || 0}x${observation.page?.viewport?.height || 0} | recording off`,
    `active ${action ? `${action.name} ${action.targetRef || ""}` : "observe"}`,
    `console ${observation.counters?.consoleErrors || 0} | network ${observation.counters?.networkErrors || 0}`,
    errors.slice(-3).join(" | "), refs,
  ].filter(Boolean).join("\n");
  let cursor = panel.querySelector(".browser-agent-cursor");
  const viewport = observation.page?.viewport;
  if (observation.cursor && shot.clientWidth && viewport?.width && viewport?.height) {
    if (!cursor) { cursor = el("span", "browser-agent-cursor"); panel.querySelector(".browser-shot-host").appendChild(cursor); }
    cursor.style.left = `${Math.max(0, Math.min(100, observation.cursor.x / viewport.width * 100))}%`;
    cursor.style.top = `${Math.max(0, Math.min(100, observation.cursor.y / viewport.height * 100))}%`;
  } else if (cursor) cursor.remove();
}

export { wireBrowserPanel, browserAction, refreshBrowserScreenshot, renderBrowserObservation };