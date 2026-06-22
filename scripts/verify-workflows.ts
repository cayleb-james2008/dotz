/**
 * Unit tests for the workflow store (src/workflows.ts).
 *
 * Tests cycle detection (Kahn's algorithm), parent/child resolution, step state
 * propagation (done → children become ready), and run lifecycle (all-terminal → done).
 *
 * Sets DOTZ_CONFIG_DIR to a temp dir so tests never touch the operator's real
 * ~/.dotz/ai-agents/workflows.json.
 *
 *   node --test --import tsx scripts/verify-workflows.ts
 *   (wired as `npm run test:workflows`)
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { WorkflowStore, WorkflowCycleError, workflowStore } from "../src/workflows";
import { WorkflowBridge } from "../src/workflow-bridge";

let tmpDir: string;

before(async () => {
  tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "dotz-wf-test-"));
  process.env.DOTZ_CONFIG_DIR = tmpDir;
});

after(async () => {
  delete process.env.DOTZ_CONFIG_DIR;
  await fs.rm(tmpDir, { recursive: true, force: true });
});

test("create: linear chain resolves parents and sets initial readiness", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "chain",
    steps: [
      { agent: "scout", task: "map code" },
      { agent: "planner", task: "plan", parents: ["0"] },
      { agent: "worker", task: "implement", parents: ["1"] },
    ],
  });
  assert.equal(run.steps[0].status, "ready");
  assert.equal(run.steps[1].status, "pending");
  assert.equal(run.steps[2].status, "pending");
  assert.deepEqual(run.steps[0].children, [run.steps[1].id]);
  assert.deepEqual(run.steps[1].children, [run.steps[2].id]);
  assert.deepEqual(run.steps[1].parents, [run.steps[0].id]);
  assert.deepEqual(run.steps[2].parents, [run.steps[1].id]);
});

test("create: parallel steps all start ready (no parents)", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "parallel",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb" },
      { agent: "c", task: "tc" },
    ],
  });
  for (const step of run.steps) assert.equal(step.status, "ready");
  for (const step of run.steps) assert.deepEqual(step.parents, []);
});

test("create: rejects cyclic steps (A→B→A)", async () => {
  const store = new WorkflowStore();
  await assert.rejects(
    store.create({
      label: "cyclic",
      steps: [
        { agent: "a", task: "ta", parents: ["1"] },
        { agent: "b", task: "tb", parents: ["0"] },
      ],
    }),
    WorkflowCycleError,
  );
});

test("create: self-referencing parent is skipped (not a cycle, step stays ready)", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "self-ref",
    steps: [{ agent: "a", task: "ta", parents: ["0"] }],
  });
  assert.equal(run.steps[0].status, "ready");
  assert.deepEqual(run.steps[0].parents, []);
});

test("create: duplicate parent ref is deduplicated", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "dup-parents",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb", parents: ["0", "0", "0"] },
    ],
  });
  assert.equal(run.steps[1].parents.length, 1);
});

test("stepState: done propagates readiness to children", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "chain",
    steps: [
      { agent: "scout", task: "map" },
      { agent: "planner", task: "plan", parents: ["0"] },
    ],
  });
  await store.stepState(run.id, run.steps[0].id, { status: "done", output: "mapped" });
  assert.equal(run.steps[1].status, "ready");
});

test("stepState: partial done does not make child ready (other parent pending)", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "fan-in",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb" },
      { agent: "c", task: "tc", parents: ["0", "1"] },
    ],
  });
  await store.stepState(run.id, run.steps[0].id, { status: "done" });
  assert.equal(run.steps[2].status, "pending", "child should still be pending — parent 1 not done");
  await store.stepState(run.id, run.steps[1].id, { status: "done" });
  assert.equal(run.steps[2].status, "ready");
});

test("stepState: all steps done finishes the run", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "parallel",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb" },
    ],
  });
  await store.start(run.id);
  assert.equal(run.status, "running");
  await store.stepState(run.id, run.steps[0].id, { status: "done" });
  assert.equal(run.status, "running");
  await store.stepState(run.id, run.steps[1].id, { status: "done" });
  assert.equal(run.status, "done");
  assert.ok(run.endedAt);
});

test("stepState: error on one step sweeps remaining runnable steps to skipped", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "error-sweep",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb" },
    ],
  });
  await store.start(run.id);
  await store.stepState(run.id, run.steps[0].id, { status: "error", error: "boom" });
  assert.equal(run.status, "error");
  // step 1 was ready/running — should be swept to skipped
  assert.equal(run.steps[1].status, "skipped");
  assert.ok(run.endedAt);
});

test("abort: sweeps all non-terminal steps to skipped and marks run aborted", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "abort-test",
    steps: [
      { agent: "a", task: "ta" },
      { agent: "b", task: "tb", parents: ["0"] },
    ],
  });
  await store.start(run.id);
  await store.abort(run.id);
  assert.equal(run.status, "aborted");
  assert.equal(run.steps[0].status, "skipped");
  assert.equal(run.steps[1].status, "skipped");
});

test("persist: run is retrievable from history after persist", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "persist-test",
    steps: [{ agent: "a", task: "ta" }],
  });
  await store.start(run.id);
  await store.stepState(run.id, run.steps[0].id, { status: "done" });
  // The run should have been persisted to the (temp) workflows.json.
  const history = await store.listHistory();
  const found = history.find((r) => r.id === run.id);
  assert.ok(found, "run should be in history after persist");
  assert.equal(found!.status, "done");
});

test("persist: start is persisted before any step state changes", async () => {
  const store = new WorkflowStore();
  const run = await store.create({
    label: "start-persist-test",
    steps: [{ agent: "a", task: "ta" }],
  });
  await store.start(run.id);
  // Simulate a server restart: a fresh store reading from disk must see the run as running.
  const restarted = new WorkflowStore();
  const history = await restarted.listHistory();
  const found = history.find((r) => r.id === run.id);
  assert.ok(found, "run should be in history immediately after start");
  assert.equal(found!.status, "running", "history must reflect running status after start");
  assert.ok(found!.startedAt, "startedAt must be set after start");
});

test("workflow-bridge: stepState rejection during start is caught, not unhandled", async () => {
  const bridge = new WorkflowBridge();
  const originalStepState = workflowStore.stepState.bind(workflowStore);
  let unhandled = 0;
  const onUnhandled = () => { unhandled++; };
  process.on("unhandledRejection", onUnhandled);

  // Force every stepState call to reject so the bridge must swallow the rejection rather than
  // leave it as an unhandled promise (which crashes Node v15+ by default).
  workflowStore.stepState = async () => { throw new Error("forced stepState failure"); };

  try {
    bridge.handleEvent("bridge-test-session", null, {
      type: "tool_execution_start",
      toolName: "subagent",
      toolCallId: "bridge-stepstate-fail",
      args: { agent: "worker", task: "do something" },
    });
    // Let the create() + start() + stepState promises settle.
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(unhandled, 0, "stepState rejection must not surface as an unhandled rejection");

    const runs = workflowStore.list();
    const run = runs.find((r) => r.sessionId === "bridge-test-session");
    assert.ok(run, "bridge should still have created the run");
    assert.equal(run!.status, "running", "run should be started even though stepState failed");
    assert.equal(run!.steps[0].status, "ready", "step status should remain ready because stepState rejected");
  } finally {
    process.off("unhandledRejection", onUnhandled);
    workflowStore.stepState = originalStepState;
  }
});
