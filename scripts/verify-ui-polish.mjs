import assert from "node:assert/strict";
import fs from "node:fs";

const app = fs.readFileSync(new URL("../web/app.js", import.meta.url), "utf8");
const html = fs.readFileSync(new URL("../web/index.html", import.meta.url), "utf8");

assert.match(app, /chatAutoScroll/, "chat keeps explicit follow-tail state");
assert.match(app, /scrollHeight\s*-\s*scrollTop\s*-\s*clientHeight/, "chat detects when the user scrolls away from the tail");
assert.match(app, /formatTokenBreakdown/, "status bar and brain use one token formatter");
assert.match(app, /const seen = new Set/, "quick commands are deduplicated");
assert.doesNotMatch(app, /state\.commands\.slice\(0, 6\)\.forEach/, "raw commands are not appended after defaults");

const chatTemplate = html.match(/<template id="tpl-chat">([\s\S]*?)<\/template>/)?.[1] || "";
for (const id of ["transcript", "quick-chips", "composer-input", "send-btn", "stop-btn"]) {
  assert.doesNotMatch(chatTemplate, new RegExp(`id="${id}"`), `chat clone does not duplicate #${id}`);
}

console.log("UI POLISH CONTRACT PASSED");
