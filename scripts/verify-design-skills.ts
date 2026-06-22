/**
 * verify-design-skills — guards the DESIGN-mode skill integration:
 *   1. the 150+ vendored Open Design skills load (source: "design"), and
 *   2. they are EXCLUDED from the always-on system-prompt index (so they never evict dotz's own
 *      skills past INDEX_CAP), while staying loadable by name via the `skill` tool.
 * Pure file/loader logic (no better-sqlite3) so it runs headless: `npx tsx scripts/verify-design-skills.ts`.
 */
import assert from "node:assert";
import { skillLoader } from "../src/skills";

await skillLoader.load();
const all = skillLoader.list();
const design = all.filter((s) => s.source === "design");
const index = skillLoader.renderIndex();
const designNames = new Set(design.map((s) => s.name));
const indexNames = (index.match(/^- ([a-z0-9-]+):/gim) || []).map((l) => l.replace(/^- /, "").replace(/:.*/, ""));
const leaked = indexNames.filter((n) => designNames.has(n));

console.log(`design-source skills: ${design.length} (of ${all.length} total)`);
console.log(`prompt-index entries: ${indexNames.length}; design skills leaked: ${leaked.length}`);

// Floor allows for same-named skills that the operator's higher-priority pools legitimately shadow
// (design-skills is the LOWEST-priority root), so a handful of the 156 vendored names resolve elsewhere.
assert(design.length >= 140, `expected >=140 design skills, got ${design.length}`);
assert(leaked.length === 0, `design skills must NOT appear in the prompt index; leaked: ${leaked.slice(0, 5)}`);
assert(skillLoader.has(design[0].name), "design skills must be loadable by name via the skill tool");
console.log("OK: design skills load, are loadable by name, and stay out of the prompt index");
