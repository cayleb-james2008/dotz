/**
 * Unit tests for the workflow template library (src/templates.ts).
 *
 * Verifies:
 *   - bundled presets are surfaced from `.pi/prompts/`
 *   - user templates shadow bundled presets by id
 *   - CRUD create/update/delete
 *   - fork copies a bundled preset into the user store
 *   - run expands `$@` and sends the expanded body to a session
 *
 * Isolated under a temp DOTZ_CONFIG_DIR so it never touches the operator's real templates.
 *
 *   node --test --import tsx scripts/verify-templates.ts
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { TemplateStore, templateStore, userTemplatesDir } from "../src/templates";

let tmpDir: string;

before(async () => {
  tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-tpl-test-"));
  process.env.DOTZ_CONFIG_DIR = tmpDir;
});

after(async () => {
  delete process.env.DOTZ_CONFIG_DIR;
  await fs.rm(tmpDir, { recursive: true, force: true });
});

test("list: surfaces the 6 bundled workflow presets", async () => {
  const store = new TemplateStore();
  const list = await store.list();
  const ids = list.map((t) => t.id);
  for (const preset of ["scout-and-plan", "implement", "implement-and-review", "ultra-code-review", "e2e-test", "self-improve"]) {
    assert.ok(ids.includes(preset), `bundled preset ${preset} is listed`);
  }
});

test("get: returns a bundled preset by id", async () => {
  const store = new TemplateStore();
  const t = await store.get("implement");
  assert.ok(t, "implement preset exists");
  assert.equal(t.source, "bundled");
  assert.ok(t.body.includes("subagent"), "implement body references subagent");
});

test("create: writes a user template and includes it in the list", async () => {
  const store = new TemplateStore();
  const t = await store.create({
    name: "my-custom",
    description: "custom workflow",
    body: "Run a custom workflow for: $@",
  });
  assert.equal(t.source, "user");
  assert.equal(t.name, "my-custom");
  assert.ok(t.body.includes("$@"));
  const list = await store.list();
  assert.ok(list.some((x) => x.id === t.id), "new user template appears in list");
});

test("create: rejects a duplicate user template id", async () => {
  const store = new TemplateStore();
  await store.create({ name: "dup", body: "body 1" });
  await assert.rejects(
    store.create({ name: "dup", body: "body 2" }),
    /already exists/,
  );
});

test("update: edits a user template and preserves origin/createdAt", async () => {
  const store = new TemplateStore();
  const t = await store.create({ name: "editable", body: "v1" });
  const updated = await store.update(t.id, { body: "v2" });
  assert.ok(updated, "update returns the template");
  assert.ok(updated.body.includes("v2"), "body contains updated content");
  assert.equal(updated.source, "user");
  assert.equal(updated.createdAt, t.createdAt, "createdAt is preserved");
  assert.ok((updated.updatedAt || 0) >= t.updatedAt, "updatedAt is bumped");
});

test("update: bundled presets are read-only", async () => {
  const store = new TemplateStore();
  await assert.rejects(
    store.update("implement", { body: "hacked" }),
    /read-only/,
  );
});

test("remove: deletes a user template", async () => {
  const store = new TemplateStore();
  const t = await store.create({ name: "delete-me", body: "x" });
  assert.equal(await store.remove(t.id), true);
  assert.equal(await store.get(t.id), null);
});

test("remove: returns false for non-existent template", async () => {
  const store = new TemplateStore();
  assert.equal(await store.remove("no-such-template"), false);
});

test("fork: copies a bundled preset to the user store", async () => {
  const store = new TemplateStore();
  const forked = await store.fork("implement", "my-implement");
  assert.ok(forked, "forked template returned");
  assert.equal(forked.source, "user");
  assert.equal(forked.origin, "implement");
  assert.equal(forked.name, "my-implement");
  assert.ok((await fs.readFile(path.join(userTemplatesDir(), `${forked.id}.md`), "utf-8")).includes("name: my-implement"));
});

test("user template shadows bundled preset with the same id", async () => {
  const store = new TemplateStore();
  // Create a user template whose sanitized id matches the bundled "implement" preset.
  const user = await store.create({ name: "implement", body: "USER OVERRIDE" });
  assert.equal(user.id, "implement");
  const got = await store.get("implement");
  assert.equal(got?.source, "user");
  assert.ok(got?.body.includes("USER OVERRIDE"), "user body overrides bundled content");
  const list = await store.list();
  const entry = list.find((t) => t.id === "implement");
  assert.equal(entry?.source, "user");
});

test("run: expands $@ with args and sends the result to the session", async () => {
  const store = new TemplateStore();
  const t = await store.create({ name: "echo", body: "echo $@" });
  let sent = "";
  const fakeSession = {
    prompt: async (text: string) => { sent = text; },
    isStreaming: false,
  } as any;
  const ok = await store.run(t.id, fakeSession, "hello world");
  assert.equal(ok, true);
  assert.equal(sent, "echo hello world");
});

test("run: returns false for missing template", async () => {
  const store = new TemplateStore();
  const fakeSession = { prompt: async () => {} } as any;
  const ok = await store.run("missing", fakeSession, "x");
  assert.equal(ok, false);
});

test("templateStore singleton uses the configured DOTZ_CONFIG_DIR", async () => {
  const t = await templateStore.create({ name: "singleton-test", body: "x" });
  const file = path.join(userTemplatesDir(), `${t.id}.md`);
  assert.ok(await fs.stat(file).then(() => true, () => false), "template written under temp DOTZ_CONFIG_DIR");
});
