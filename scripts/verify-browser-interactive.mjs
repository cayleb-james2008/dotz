/** Contract verification for the user-interactive browser panel. */
import assert from "node:assert/strict";
import fs from "node:fs/promises";

const read = (path) => fs.readFile(new URL(path, import.meta.url), "utf8");
const [browserSource, toolsSource, appSource, htmlSource, cssSource] = await Promise.all([
  read("../src/browser.ts"),
  read("../.pi/extensions/dotz-tools/index.ts"),
  read("../web/app.js"),
  read("../web/index.html"),
  read("../web/styles.css"),
]);

for (const action of ["clickAt", "back", "forward", "reload"]) {
  assert.match(browserSource, new RegExp(`(?:case\\s+["']${action}["']|["']${action}["'])`), `${action} is a typed browser action`);
  assert.match(toolsSource, new RegExp(`Type\\.Literal\\(["']${action}["']\\)`), `${action} is exposed through browser_act`);
}
assert.match(browserSource, /expectedSeq[^\n]+observation\.seq/, "coordinate actions are sequence guarded");
assert.match(browserSource, /mouse["'],\s*["']move/, "frame clicks use agent-browser mouse coordinates");
assert.match(browserSource, /keyboard["'],\s*["']inserttext/, "focused controls accept safe text without raw eval");
assert.doesNotMatch(browserSource, /case\s+["']eval["']|BrowserActionName[^;]+["']eval["']/s, "controller does not expose page evaluation");

for (const id of ["br-stop", "br-scroll-up", "br-scroll-down", "br-type-text", "br-type-send"]) {
  assert.match(htmlSource, new RegExp(`id=["']${id}["']`), `${id} is rendered in the browser panel`);
}
assert.doesNotMatch(htmlSource, /id=["']br-eval/, "raw JavaScript controls stay removed");
assert.match(htmlSource, /tabindex=["']0["'][^>]*id=["']br-shot["']|id=["']br-shot["'][^>]*tabindex=["']0["']/, "the frame is keyboard focusable");

assert.match(appSource, /setInterval\([^)]*refreshBrowserScreenshot/s, "live browser state is polled while the panel is open");
assert.match(appSource, /getBoundingClientRect\(\)/, "frame clicks are mapped from rendered pixels");
assert.match(appSource, /action:\s*["']clickAt["']/, "frame clicks dispatch clickAt actions");
assert.match(appSource, /expectedSeq:\s*state\.browserObservation\.seq/, "user actions carry the current observation sequence");
assert.match(appSource, /action:\s*["']type["']/, "the type control dispatches focused text");
assert.match(appSource, /action:\s*["']scroll["']/, "scroll controls dispatch browser scroll actions");
assert.match(appSource, /browserPollTimer/, "browser polling has an explicit lifecycle handle");

assert.match(cssSource, /\.br-shot\s*\{[^}]*cursor:\s*crosshair/s, "the live frame advertises click interaction");
assert.match(cssSource, /\.browser-interact-row/, "typing and scroll controls have panel styling");

console.log("INTERACTIVE BROWSER CONTRACT PASSED");
