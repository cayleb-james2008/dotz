import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const temp = fs.mkdtempSync(path.join(os.tmpdir(), "dotz-dynamic-resources-"));
process.env.PI_CODING_AGENT_DIR = path.join(temp, "pi-agent");
process.env.DOTZ_CONFIG_DIR = path.join(temp, "dotz");
process.env.DOTZ_SKILLS_PATHS = path.join(temp, "dotz", "ai-agents", "skills");

const { createUserAgent, discoverAgents } = await import("../.pi/extensions/subagent/agents.ts");
const { createUserSkill, SkillLoader } = await import("../src/skills.ts");

try {
  const agent = createUserAgent({
    name: "release-auditor",
    description: "Audits a release before it ships.",
    systemPrompt: "Inspect the release evidence and report only reproducible findings.",
    tools: ["read", "grep"],
  });
  assert.equal(agent.model, "ollama/minimax-m3");
  assert.equal(agent.source, "user");
  assert.ok(fs.existsSync(agent.filePath));

  const discovered = discoverAgents(temp, "both").agents.find((item) => item.name === agent.name);
  assert.ok(discovered, "created agent is immediately discoverable");
  assert.deepEqual(discovered.tools, ["read", "grep"]);
  assert.equal(discovered.systemPrompt, "Inspect the release evidence and report only reproducible findings.");

  const skill = await createUserSkill({
    name: "release-evidence",
    description: "Collects reproducible release evidence.",
    body: "# Release evidence\n\nRun the build, test the artifact, and record exact results.",
  });
  assert.ok(fs.existsSync(skill.path));

  const loader = new SkillLoader();
  await loader.load();
  assert.equal(loader.get("release-evidence")?.path, skill.path);
  assert.match(await loader.loadBody("release-evidence"), /test the artifact/);

  assert.throws(
    () => createUserAgent({ name: "../escape", description: "bad", systemPrompt: "bad" }),
    /lowercase letter/,
  );
  await assert.rejects(
    () => createUserSkill({ name: "../escape", description: "bad", body: "bad" }),
    /lowercase letter/,
  );
  assert.throws(
    () => createUserAgent({ name: agent.name, description: "duplicate", systemPrompt: "duplicate" }),
    /already exists/,
  );
  await assert.rejects(
    () => createUserSkill({ name: skill.name, description: "duplicate", body: "duplicate" }),
    /already exists/,
  );

  console.log("DYNAMIC AGENT + SKILL CONTRACT PASSED");
} finally {
  fs.rmSync(temp, { recursive: true, force: true });
}
