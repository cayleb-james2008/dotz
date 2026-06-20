/** Contract verification for the isolated Pi browser controller. */
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { BrowserController } from "../src/browser.ts";

const packageJson = JSON.parse(await fs.readFile(new URL("../package.json", import.meta.url), "utf8"));
assert.equal(packageJson.version, "0.2.0", "release version is 0.2.0");
assert.equal(packageJson.dependencies["agent-browser"], "0.27.0", "agent-browser is pinned exactly");

const controller = new BrowserController({ executable: "__missing_agent_browser__" });
await assert.rejects(
  controller.start({ projectId: "verify", url: "file:///C:/Windows/win.ini", allowedOrigins: [] }),
  /http or https/i,
  "file navigation is rejected before process launch",
);
await assert.rejects(
  controller.start({ projectId: "verify", url: "https://example.com", allowedOrigins: ["https://other.example"] }),
  /allowlist/i,
  "initial navigation must be allowlisted",
);

const toolsSource = await fs.readFile(new URL("../.pi/extensions/dotz-tools/index.ts", import.meta.url), "utf8");
for (const name of ["browser_start", "browser_act", "browser_stop"]) {
  assert.match(toolsSource, new RegExp(`name:\\s*[\"']${name}[\"']`), `${name} is registered`);
}
assert.doesNotMatch(toolsSource, /name:\s*["']browser_eval["']/, "raw eval is not exposed");

const serverSource = await fs.readFile(new URL("../src/server.ts", import.meta.url), "utf8");
assert.doesNotMatch(serverSource, /\/api\/browser\/eval/, "browser eval REST endpoint is removed");
assert.match(serverSource, /\/api\/browser\/start/, "browser start REST endpoint exists");
assert.match(serverSource, /\/api\/browser\/act/, "browser act REST endpoint exists");
assert.match(serverSource, /\/api\/browser\/stop/, "browser stop REST endpoint exists");
assert.match(serverSource, /\/api\/browser\/frame/, "binary frames use a separate endpoint");

const preloadSource = await fs.readFile(new URL("../src/preload.ts", import.meta.url), "utf8");
assert.match(preloadSource, /check:\s*\(\)/, "source-rebuild update check bridge is exposed");
assert.match(preloadSource, /apply:\s*\(\)/, "source-rebuild update apply bridge is exposed");
const mainSource = await fs.readFile(new URL("../src/main.ts", import.meta.url), "utf8");
assert.doesNotMatch(mainSource, /WebContentsView|browser\.attach/, "remote pages are never embedded with the Dotz preload");
const updaterSource = await fs.readFile(new URL("../src/updater.ts", import.meta.url), "utf8");
assert.match(updaterSource, /ipcMain\.on\(["']dotz-update-check["']/, "update check IPC is registered");
assert.match(updaterSource, /ipcMain\.on\(["']dotz-update-apply["']/, "update apply IPC is registered");
for (const state of ["checked", "available", "not-available", "applying", "failed"]) {
  assert.match(updaterSource, new RegExp(`sendToRenderer\\(["']${state}["']`), `updater models ${state}`);
}
assert.match(updaterSource, /git pull --ff-only/, "portable updates use the source-rebuild contract");

console.log("BROWSER CONTROLLER CONTRACT PASSED");
