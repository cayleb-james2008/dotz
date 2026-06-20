/** Contract verification for the local-login Connections panel (GitHub / Vercel / Neon). */
import assert from "node:assert/strict";
import fs from "node:fs/promises";

const read = (path) => fs.readFile(new URL(path, import.meta.url), "utf8");
const [connSource, serverSource, appSource, htmlSource, cssSource] = await Promise.all([
  read("../src/connections.ts"),
  read("../src/server.ts"),
  read("../web/app.js"),
  read("../web/index.html"),
  read("../web/styles.css"),
]);

// --- backend covers all three providers, drives the local CLI browser login, hides the console window, never logs secrets ---
for (const provider of ["github", "vercel", "neon"]) {
  assert.match(connSource, new RegExp(`["']${provider}["']`), `${provider} is a known connection provider`);
}
assert.match(connSource, /windowsHide:\s*true/, "CLI spawns hide the console window (build guard)");
assert.match(connSource, /\bgh\b[\s\S]{0,80}auth/, "GitHub login shells the gh CLI");
assert.match(connSource, /vercel/, "Vercel login shells the vercel CLI");
assert.match(connSource, /neonctl/, "Neon login shells neonctl");
assert.doesNotMatch(connSource, /auth\s+--logout/, "Neon logout never re-runs neonctl's (nonexistent) logout — that command actually launches login");
assert.match(connSource, /credentials\.json/, "Neon status/logout key off the neonctl credentials file so an npx login is reflected");
assert.doesNotMatch(connSource, /console\.log\([^)]*token/i, "tokens are never logged");

// --- server exposes status + per-provider browser login (start + stream) + logout ---
assert.match(serverSource, /\/api\/connections\b/, "connections status endpoint is registered");
assert.match(serverSource, /\/api\/connections\/:provider\/login/, "per-provider login endpoint is registered");
assert.match(serverSource, /\/api\/connections\/:provider\/logout/, "per-provider logout endpoint is registered");

// --- the panel auto-registers in the palette and is wired with a polling lifecycle ---
assert.match(appSource, /PANEL_NAMES\s*=\s*\[[^\]]*["']connections["']/s, "connections panel is registered in PANEL_NAMES");
assert.match(appSource, /connections:\s*\{[^}]*label:\s*["']CONNECTIONS["']/s, "connections panel has palette metadata");
assert.match(appSource, /name === ["']connections["'][\s\S]{0,80}wireConnectionsPanel/, "connections panel mounts its wiring");
assert.match(appSource, /function wireConnectionsPanel/, "wireConnectionsPanel is defined");
assert.match(appSource, /connectionsPollTimer/, "connections polling has an explicit lifecycle handle");
assert.match(appSource, /\/api\/connections\/[^"'`]*login/, "the panel triggers the browser login endpoint");

// --- the panel template + provider list + styling exist ---
assert.match(htmlSource, /id=["']tpl-connections["']/, "connections panel template exists");
assert.match(htmlSource, /data-panel=["']connections["']/, "connections template targets the connections panel");
assert.match(htmlSource, /id=["']conn-list["']/, "connections panel renders a provider list");
assert.match(cssSource, /\.connections-body|\.conn-row/, "connections panel has styling");

console.log("CONNECTIONS PANEL CONTRACT PASSED");
