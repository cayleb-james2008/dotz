/** Live acceptance for the pinned, isolated browser worker. Requires a Dotz server URL. */
import assert from "node:assert/strict";
import { BrowserController } from "../src/browser.ts";

const url = process.argv[2] || "http://127.0.0.1:4317";
const controller = new BrowserController();
const start = await controller.start({
  projectId: "browser-acceptance",
  workflowId: "start-observe-click-type-stop",
  url,
  allowedOrigins: [new URL(url).origin],
  viewport: { width: 1100, height: 760 },
});

try {
  assert.equal(start.status, "ready");
  assert.equal(start.schemaVersion, 1);
  assert.ok(start.frame?.available, "initial frame is available separately");
  assert.equal("dataUrl" in (start.frame || {}), false, "binary frame is not embedded in observations");
  assert.ok(controller.frame(start.sessionId, -1)?.data.length, "binary frame can be read separately");

  let observation = start;
  const textbox = observation.elements.find((item) => /textbox/i.test(item.role));
  assert.ok(textbox, `expected a textbox in snapshot: ${observation.snapshot.slice(0, 500)}`);
  observation = await controller.act({
    sessionId: observation.sessionId,
    action: "click",
    targetRef: textbox.ref,
    expectedSeq: observation.seq,
  });
  observation = await controller.act({
    sessionId: observation.sessionId,
    action: "type",
    expectedSeq: observation.seq,
    text: "dotz browser acceptance",
  });
  assert.equal(observation.currentAction?.summary, "type into focused element");

  observation = await controller.act({
    sessionId: observation.sessionId,
    action: "clickAt",
    expectedSeq: observation.seq,
    x: 4,
    y: 4,
  });
  assert.equal(observation.cursor?.kind, "clickAt");
  observation = await controller.act({ sessionId: observation.sessionId, action: "reload" });

  const button = observation.elements.find((item) => /button/i.test(item.role));
  assert.ok(button, "expected a button in the observed page");
  observation = await controller.act({
    sessionId: observation.sessionId,
    action: "click",
    targetRef: button.ref,
    expectedSeq: observation.seq,
  });
  assert.equal(observation.status, "ready");
  console.log(JSON.stringify({
    status: observation.status,
    seq: observation.seq,
    url: observation.page.url,
    typed: textbox.ref,
    clicked: button.ref,
    frameBytes: controller.frame(observation.sessionId, -1)?.data.length || 0,
  }));
} finally {
  const stopped = await controller.stop(start.sessionId);
  assert.equal(stopped.status, "stopped");
  assert.equal(stopped.frame, undefined);
}

console.log("LIVE BROWSER ACCEPTANCE PASSED");
