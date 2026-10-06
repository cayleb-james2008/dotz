import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createDriverCallError, projectFrameToWindowScreenshot, sendGuardedScroll } from "./windows-installer-runtime.mjs";

const css = readFileSync(new URL("../web/styles.css", import.meta.url), "utf8");
const bento = css.match(/\.bento\s*\{([\s\S]*?)\n\}/)?.[1];
assert.ok(bento, "bento panel grid must remain defined");
assert.match(
  bento,
  /overflow-x:\s*auto\s*;/,
  "the bento must provide a user-scrollable horizontal path when a persisted panel is outside the native viewport",
);

// These are the captured native geometry dimensions/coordinates from the verified
// 2026-10-06 restart screenshot: the old harness saw the saved text outside the window.
const window = { bounds: { x: 7, y: 0, width: 1030, height: 781 } };
const offscreen = projectFrameToWindowScreenshot(
  { x: 1087, y: 210, w: 457, h: 28 }, window, 1028, 779,
);
assert.deepEqual(offscreen, { x: 1079, y: 209, w: 457, h: 28, inside: false });
const visible = projectFrameToWindowScreenshot(
  { x: 523, y: 208, w: 457, h: 28 }, window, 1028, 779,
);
assert.deepEqual(visible, { x: 515, y: 207, w: 457, h: 28, inside: true });
assert.equal(projectFrameToWindowScreenshot(
  { x: 1020, y: 210, w: 20, h: 28 }, window, 1028, 779,
).inside, false, "partially clipped text is not visible acceptance evidence");
assert.equal(projectFrameToWindowScreenshot(
  { x: 20, y: 20, w: 40, h: 20 }, { bounds: null }, 1028, 779,
).inside, false, "missing native window geometry fails closed");
assert.equal(projectFrameToWindowScreenshot(
  { x: 20, y: 20, w: 0, h: 0 }, window, 1028, 779,
).inside, false, "zero-area UIA text bounds cannot prove visible pixels");

const backgroundUnavailable = createDriverCallError("scroll", {
  status: 1,
  stdout: "",
  stderr: JSON.stringify({ code: "background_unavailable", escalation: { recommended: "foreground" } }),
});
const scrollCalls = [];
const verifyStages = [];
const guarded = sendGuardedScroll({
  pid: 1364,
  windowId: 196944,
  payload: { direction: "right", by: "page", amount: 8, x: 514, y: 400 },
  verifyTarget: (stage) => { verifyStages.push(stage); return { valid: true, stage }; },
  sendScroll: (request) => {
    scrollCalls.push(request);
    if (request.delivery_mode === "background") throw backgroundUnavailable;
    return { delivered: true };
  },
});
assert.equal(guarded.ok, true);
assert.deepEqual(verifyStages, ["before-background-scroll", "before-foreground-scroll"]);
assert.equal(scrollCalls.length, 2, "one explicitly recommended foreground retry is allowed");
assert.equal(scrollCalls[0].delivery_mode, "background");
assert.equal(scrollCalls[1].delivery_mode, "foreground");
assert.equal(scrollCalls[1].pid, 1364);
assert.equal(scrollCalls[1].window_id, 196944);
assert.equal(scrollCalls[1].x, 514);
assert.equal(scrollCalls[1].y, 400);

const scrollPatternCalls = [];
const scrollPatternOutcome = sendGuardedScroll({
  pid: 1364,
  windowId: 196944,
  payload: { direction: "right", by: "page", amount: 8, element_token: "fresh-document-scroll-token" },
  verifyTarget: () => ({ valid: true }),
  sendScroll: (request) => {
    scrollPatternCalls.push(request);
    if (request.delivery_mode === "background") throw backgroundUnavailable;
    return { path: "uia", delivery_mode: "foreground" };
  },
});
assert.equal(scrollPatternOutcome.ok, true);
assert.equal(scrollPatternCalls.length, 2);
assert.equal(scrollPatternCalls[1].element_token, "fresh-document-scroll-token", "the exact fresh native UIA scroll token survives guarded foreground escalation");
assert.equal(scrollPatternCalls[1].pid, 1364);
assert.equal(scrollPatternCalls[1].window_id, 196944);
assert.equal(scrollPatternCalls[1].delivery_mode, "foreground");

const invalidForegroundCalls = [];
const invalidForeground = sendGuardedScroll({
  pid: 1364,
  windowId: 196944,
  payload: { direction: "right", x: 514, y: 400 },
  verifyTarget: (stage) => ({ valid: stage !== "before-foreground-scroll" }),
  sendScroll: (request) => {
    invalidForegroundCalls.push(request);
    if (request.delivery_mode === "background") throw backgroundUnavailable;
    return { delivered: true };
  },
});
assert.equal(invalidForeground.ok, false);
assert.equal(invalidForegroundCalls.length, 1, "a failed exact-window recheck refuses foreground input");

const unrelatedCalls = [];
const unrelatedFailure = sendGuardedScroll({
  pid: 1364,
  windowId: 196944,
  payload: { direction: "right", x: 514, y: 400 },
  verifyTarget: () => ({ valid: true }),
  sendScroll: (request) => {
    unrelatedCalls.push(request);
    throw new Error("unstructured timeout");
  },
});
assert.equal(unrelatedFailure.ok, false);
assert.equal(unrelatedCalls.length, 1, "unknown failures never trigger foreground delivery");

const backgroundSuccessCalls = [];
const backgroundSuccess = sendGuardedScroll({
  pid: 1364,
  windowId: 196944,
  payload: { direction: "right", x: 514, y: 400 },
  verifyTarget: () => ({ valid: true }),
  sendScroll: (request) => { backgroundSuccessCalls.push(request); return { delivered: true }; },
});
assert.equal(backgroundSuccess.ok, true);
assert.equal(backgroundSuccessCalls.length, 1, "successful background input is never duplicated in foreground");

const acceptanceSource = readFileSync(new URL("./windows-installer-acceptance.mjs", import.meta.url), "utf8");
const visibilityJourney = acceptanceSource.match(/async function ensureMemoryVisibleAfterRestart\(\) \{([\s\S]*?)\n\}/)?.[1];
assert.ok(visibilityJourney, "restart acceptance must define a visibility journey distinct from UIA text persistence");
assert.match(visibilityJourney, /getState\(true\)/, "visibility must come from a fresh native UIA/screenshot state");
assert.match(visibilityJourney, /sendGuardedScroll\(/, "offscreen memory must be reached with guarded native scroll input");
assert.match(visibilityJourney, /call\("scroll", request\)/, "navigation must use CUA's native scroll action rather than DOM automation");
const uiAutomationFallback = acceptanceSource.match(/const scrollSurface = findElement\(state, \{ text: [^,]+, role: "Document" \}\);([\s\S]*?)if \(!uiaOutcome\.ok\)/)?.[1];
assert.ok(uiAutomationFallback, "after user wheel fails to reveal the native panel, use a fresh scrollable Document target from the native UIA tree");
assert.match(uiAutomationFallback, /element_token/, "native fallback must use the current snapshot's UIA scroll token");
assert.match(uiAutomationFallback, /sendGuardedScroll\(/, "UIA scroll must use the same exact-process/window delivery guard");
const wheelScreenshot = visibilityJourney.indexOf('captureScreenshot("reuse-memory-after-native-wheel-before-uia-scroll-after-restart"');
const refreshedUiTree = visibilityJourney.indexOf("state = getState(true);", wheelScreenshot + 1);
const refreshedVisibilityReadback = visibilityJourney.indexOf("after = memoryVisibilityReadback(state);", refreshedUiTree + 1);
const freshReachabilityCheck = visibilityJourney.indexOf("if (!memoryIsFullyReachable(after)) {", refreshedVisibilityReadback + 1);
const uiaScroll = visibilityJourney.indexOf("const scrollSurface = findElement");
assert.ok(wheelScreenshot >= 0 && refreshedUiTree > wheelScreenshot && refreshedVisibilityReadback > refreshedUiTree && freshReachabilityCheck > refreshedVisibilityReadback && freshReachabilityCheck < uiaScroll, "refresh native UIA after screenshot capture, recompute bounds, then recheck reachability before ScrollPattern input");
assert.match(visibilityJourney, /captureScreenshot\("reuse-memory-after-restart"/, "the final post-navigation native pixels must be preserved");
const reachability = acceptanceSource.match(/function memoryIsFullyReachable\(readback\) \{([\s\S]*?)\n\}/)?.[1];
assert.ok(reachability, "post-restart acceptance must define a fail-closed native visibility predicate");
assert.match(reachability, /memory_text[\s\S]*?screenshot_frame\?\.inside/);
assert.match(reachability, /memory_panel_header[\s\S]*?screenshot_frame\?\.inside/);
assert.match(reachability, /add_button[\s\S]*?screenshot_frame\?\.inside/);
console.log("post-restart Memory visibility and guarded scroll tests passed");
