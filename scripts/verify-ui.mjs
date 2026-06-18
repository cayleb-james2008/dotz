/** UI smoke test: boot server, fetch the SPA root + assets, and confirm the profile picker
 *  markup and the profiles API are wired for the frontend. Exits 0 on success. */
import { buildServer } from "../src/server.ts";

const PORT = 4320;
const { app } = await buildServer();
await app.listen({ host: "127.0.0.1", port: PORT });
const base = `http://127.0.0.1:${PORT}`;
const fails = [];
const ok = (c, m) => { console.log((c ? "  ✓ " : "  ✕ ") + m); if (!c) fails.push(m); };

const root = await fetch(base + "/");
ok(root.status === 200, `GET / → ${root.status}`);
const html = await root.text();
ok(html.includes('id="profile-picker"'), "index.html contains #profile-picker");
ok(html.includes("/app.js"), "index.html loads app.js");
ok(html.includes("/styles.css"), "index.html loads styles.css");

const css = await (await fetch(base + "/styles.css")).text();
ok(css.includes(".profile-picker"), "styles.css has .profile-picker rules");

const js = await (await fetch(base + "/app.js")).text();
ok(js.includes("loadProfiles"), "app.js defines loadProfiles");
ok(js.includes('post("/api/sessions", { profileId:'), "app.js sends profileId on session create");
ok(js.includes("renderProfilePicker"), "app.js renders the picker");

const prof = await (await fetch(base + "/api/profiles")).json();
ok(prof.profiles.length === 5, `/api/profiles → ${prof.profiles.length} profiles`);

await app.close();
console.log("\n" + (fails.length ? `${fails.length} FAIL` : "UI SMOKE PASSED"));
process.exit(fails.length ? 1 : 0);