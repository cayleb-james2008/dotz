/**
 * dotz workflow store — first-class WorkflowRun objects that make the implicit subagent
 * orchestration (single/parallel/chain) observable and controllable by the UI.
 *
 * The execution backend stays the existing `subagent` extension (it spawns isolated pi
 * processes). This module is the *observability + control* layer: it records step DAGs,
 * their statuses, outputs, and usage, and emits `workflow_*` events the UI renders as an
 * interactive node/edge graph.
 *
 * Storage: JSON at ~/.dotz/ai-agents/workflows.json (global) — runs are global history,
 * optionally filtered by projectId. Pure persistence, no agent runtime (per the dotz
 * convention that stores stay separate from the agent loop).
 */
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import type { WorkflowRun, WorkflowStep } from "./types";

const DOTZ_DIR = path.join(os.homedir(), ".dotz", "ai-agents");
const WORKFLOWS_FILE = path.join(DOTZ_DIR, "workflows.json");

type WorkflowListener = (runId: string, event: WorkflowEvent) => void;

export type WorkflowEvent =
  | { type: "workflow_start"; run: WorkflowRun }
  | { type: "workflow_end"; run: WorkflowRun }
  | { type: "step_state"; stepId: string; status: WorkflowStep["status"]; output?: string; error?: string; usage?: WorkflowStep["usage"]; sandboxRunId?: string | null; browserSessionId?: string | null; toolCallIds?: string[]; thinking?: string }
  | { type: "step_added"; step: WorkflowStep };

async function ensureDir() {
  await fs.mkdir(DOTZ_DIR, { recursive: true });
}

async function readAll(): Promise<WorkflowRun[]> {
  try {
    const raw = await fs.readFile(WORKFLOWS_FILE, "utf-8");
    return JSON.parse(raw) as WorkflowRun[];
  } catch {
    return [];
  }
}

async function writeAll(runs: WorkflowRun[]): Promise<void> {
  await ensureDir();
  await fs.writeFile(WORKFLOWS_FILE, JSON.stringify(runs, null, 2), "utf-8");
}

export interface CreateStepInput {
  agent: string;
  task: string;
  parents?: string[];
  batch?: string;
  sandboxRunId?: string | null;
  browserSessionId?: string | null;
  toolCallIds?: string[];
  thinking?: string;
}

export class WorkflowStore {
  private active = new Map<string, WorkflowRun>();
  private listeners = new Set<WorkflowListener>();

  onEvent(listener: WorkflowListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(runId: string, event: WorkflowEvent) {
    for (const l of [...this.listeners]) {
      try {
        l(runId, event);
      } catch {
        /* never let a listener break the workflow event loop */
      }
    }
  }

  /** Create a new run with the given steps (parents/children resolved from inputs). */
  async create(opts: {
    projectId?: string | null;
    sessionId?: string | null;
    label: string;
    origin?: string;
    steps: CreateStepInput[];
  }): Promise<WorkflowRun> {
    const now = Date.now();
    const steps: WorkflowStep[] = opts.steps.map((s) => ({
      id: randomUUID(),
      agent: s.agent,
      task: s.task,
      status: s.parents && s.parents.length > 0 ? "pending" : "ready",
      parents: [],
      children: [],
      batch: s.batch,
      sandboxRunId: s.sandboxRunId,
      browserSessionId: s.browserSessionId,
      toolCallIds: s.toolCallIds,
      thinking: s.thinking,
    }));
    // Resolve parent refs to real step ids. A ref may be a positional index into the input steps
    // (what the workflow bridge emits, e.g. "0") OR an already-assigned step id; map both so the
    // graph actually draws chain/fan-out edges instead of silently dropping them.
    opts.steps.forEach((s, idx) => {
      for (const ref of s.parents ?? []) {
        const n = Number(ref);
        const id = Number.isInteger(n) && n >= 0 && n < steps.length ? steps[n].id : ref;
        // Skip self-references and duplicates — a step that is its own parent can never become
        // `ready`, stalling the run as permanently non-terminal (workflow_end never fires).
        if (id !== steps[idx].id && !steps[idx].parents.includes(id) && steps.some((st) => st.id === id)) {
          steps[idx].parents.push(id);
        }
      }
    });
    // resolve children from parents
    for (const step of steps) {
      for (const pid of step.parents) {
        const parent = steps.find((s) => s.id === pid);
        if (parent) parent.children.push(step.id);
      }
    }
    // A step left with no valid parents after resolution must be runnable, not stuck "pending"
    // (e.g. all its parent refs were self-references and got filtered out above).
    for (const step of steps) {
      if (step.parents.length === 0 && step.status === "pending") step.status = "ready";
    }
    const run: WorkflowRun = {
      id: randomUUID(),
      projectId: opts.projectId ?? null,
      sessionId: opts.sessionId ?? null,
      label: opts.label,
      steps,
      status: "pending",
      origin: opts.origin,
      createdAt: now,
      updatedAt: now,
    };
    this.active.set(run.id, run);
    await this.persist(run);
    return run;
  }

  get(id: string): WorkflowRun | undefined {
    return this.active.get(id);
  }

  list(): WorkflowRun[] {
    return [...this.active.values()];
  }

  async listHistory(projectId?: string): Promise<WorkflowRun[]> {
    const all = await readAll();
    if (projectId) return all.filter((r) => r.projectId === projectId);
    return all;
  }

  /** Mark a run as started. */
  start(id: string): void {
    const run = this.active.get(id);
    if (!run) return;
    run.status = "running";
    run.startedAt = Date.now();
    run.updatedAt = Date.now();
    this.emit(id, { type: "workflow_start", run });
  }

  /** Update a step's state and propagate readiness to children. */
  async stepState(
    runId: string,
    stepId: string,
    patch: Partial<Pick<WorkflowStep, "status" | "output" | "error" | "usage" | "sandboxRunId" | "browserSessionId" | "toolCallIds" | "thinking">>
  ): Promise<void> {
    const run = this.active.get(runId);
    if (!run) return;
    const step = run.steps.find((s) => s.id === stepId);
    if (!step) return;
    step.status = patch.status ?? step.status;
    if (patch.output !== undefined) step.output = patch.output;
    if (patch.error !== undefined) step.error = patch.error;
    if (patch.usage !== undefined) step.usage = patch.usage;
    if (patch.sandboxRunId !== undefined) step.sandboxRunId = patch.sandboxRunId;
    if (patch.browserSessionId !== undefined) step.browserSessionId = patch.browserSessionId;
    if (patch.toolCallIds !== undefined) step.toolCallIds = patch.toolCallIds;
    if (patch.thinking !== undefined) step.thinking = patch.thinking;
    if (step.status === "running" && !step.startedAt) step.startedAt = Date.now();
    if ((step.status === "done" || step.status === "error") && !step.endedAt) step.endedAt = Date.now();
    run.updatedAt = Date.now();
    this.emit(runId, { type: "step_state", stepId, status: step.status, output: step.output, error: step.error, usage: step.usage, sandboxRunId: step.sandboxRunId, browserSessionId: step.browserSessionId, toolCallIds: step.toolCallIds, thinking: step.thinking });

    // propagate readiness: when all parents done, pending → ready
    if (step.status === "done") {
      for (const childId of step.children) {
        const child = run.steps.find((s) => s.id === childId);
        if (child && child.status === "pending") {
          const allParentsDone = child.parents.every((pid) => {
            const p = run.steps.find((s) => s.id === pid);
            return p?.status === "done";
          });
          if (allParentsDone) {
            child.status = "ready";
            this.emit(runId, { type: "step_state", stepId: child.id, status: "ready" });
          }
        }
      }
      // if all steps done, mark run done
      if (run.steps.every((s) => s.status === "done" || s.status === "skipped")) {
        run.status = "done";
        run.endedAt = Date.now();
        run.updatedAt = Date.now();
        this.emit(runId, { type: "workflow_end", run });
      }
    } else if (step.status === "error") {
      // Mark the run errored but leave remaining steps as-is. Emit workflow_end only on the FIRST
      // transition to a terminal state, so a multi-step error sweep (workflow-bridge onEnd) doesn't
      // fire workflow_end once per swept step.
      if (run.status !== "error" && run.status !== "aborted" && run.status !== "done") {
        run.status = "error";
        run.endedAt = Date.now();
        this.emit(runId, { type: "workflow_end", run });
      }
      run.updatedAt = Date.now();
    }
    await this.persist(run);
  }

  async abort(id: string): Promise<void> {
    const run = this.active.get(id);
    if (!run) return;
    run.status = "aborted";
    run.endedAt = Date.now();
    run.updatedAt = Date.now();
    for (const step of run.steps) {
      if (step.status === "running" || step.status === "ready" || step.status === "pending") {
        step.status = "skipped";
      }
    }
    this.emit(id, { type: "workflow_end", run });
    await this.persist(run);
  }

  private writeQueue: Promise<void> = Promise.resolve();
  private persist(run: WorkflowRun): Promise<void> {
    // Serialize the read-modify-write of the shared workflows.json so concurrent persists (e.g. two
    // different runs persisting at once) can't read the same snapshot and clobber each other's entry.
    const result = this.writeQueue.then(async () => {
      const all = await readAll();
      const idx = all.findIndex((r) => r.id === run.id);
      if (idx >= 0) all[idx] = run;
      else all.push(run);
      await writeAll(all);
    });
    // The queue tail swallows errors so one failed write can't poison later writes; the caller still
    // gets `result` and can observe its own write's failure.
    this.writeQueue = result.catch(() => {});
    return result;
  }
}

export const workflowStore = new WorkflowStore();