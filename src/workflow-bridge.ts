/**
 * dotz workflow bridge — synthesizes WorkflowRun objects from the subagent extension's
 * tool_execution_* events so the UI graph auto-populates when the agent runs /implement,
 * /implement-and-review, or any subagent dispersal.
 *
 * The subagent extension produces rich per-step data in its `SubagentDetails` (mode, results
 * with exitCode/usage/model), but that data is opaque to the UI (it appears as a single tool
 * card). This bridge subscribes to pi session events, detects subagent tool calls, and creates
 * a WorkflowRun with one step per subagent result, updating step states as the tool progresses.
 *
 * This is the glue between the subagent extension (execution backend) and WorkflowStore
 * (observability layer). It does NOT spawn agents — it only observes and records.
 */
import type { WorkflowRun } from "./types";
import { workflowStore } from "./workflows";

/** The shape of SubagentDetails from the subagent extension (kept loose to avoid a hard dep). */
interface SubagentDetailsLike {
  mode: "single" | "parallel" | "chain";
  agentScope: string;
  projectAgentsDir: string | null;
  results: Array<{
    agent: string;
    task: string;
    exitCode: number;
    stopReason?: string;
    usage?: { input?: number; output?: number; cost?: number; turns?: number };
    model?: string;
    step?: number;
    sandboxRunId?: string | null;
    browserSessionId?: string | null;
    toolCallIds?: string[];
    thinking?: string;
  }>;
}

/** Extract SubagentDetails from a tool_execution event's result/partialResult. */
function extractDetails(result: unknown): SubagentDetailsLike | undefined {
  if (!result || typeof result !== "object") return undefined;
  const r = result as { details?: SubagentDetailsLike };
  return r.details;
}

/** Map a subagent result's exitCode + stopReason to a WorkflowStep status. */
function stepStatus(exitCode: number, stopReason?: string): "running" | "done" | "error" {
  if (exitCode === -1) return "running"; // -1 = still running (subagent convention)
  if (exitCode === 0 && stopReason !== "error" && stopReason !== "aborted") return "done";
  return "error";
}

export class WorkflowBridge {
  /** Active run per toolCallId — one subagent tool call = one workflow run. */
  private runsByToolCall = new Map<string, string>();
  /** Step id per (runId, agent, task, stepIndex) — stable across updates. */
  private stepIds = new Map<string, string>();

  /** Process a pi session event. Call this for every event from a subscribed session. */
  handleEvent(sessionId: string, projectId: string | null, event: unknown): void {
    const e = event as { type?: string; toolName?: string; toolCallId?: string; args?: unknown; result?: unknown; partialResult?: unknown; isError?: boolean };
    if (!e || !e.type) return;

    if (e.type === "tool_execution_start" && e.toolName === "subagent") {
      this.onStart(sessionId, projectId, e.toolCallId!, e.args);
    } else if (e.type === "tool_execution_update" && e.toolName === "subagent") {
      this.onUpdate(e.toolCallId!, e.partialResult);
    } else if (e.type === "tool_execution_end" && e.toolName === "subagent") {
      this.onEnd(e.toolCallId!, e.result, e.isError);
    }
  }

  private onStart(sessionId: string, projectId: string | null, toolCallId: string, args: unknown): void {
    const a = (args || {}) as { agent?: string; task?: string; tasks?: Array<{ agent: string; task: string }>; chain?: Array<{ agent: string; task: string }> };
    // Build steps from the args (we know the plan up-front)
    const steps: Array<{ agent: string; task: string; parents?: string[] }> = [];
    if (a.chain && a.chain.length > 0) {
      a.chain.forEach((step, i) => {
        steps.push({ agent: step.agent, task: step.task, parents: i > 0 ? [String(i - 1)] : [] });
      });
    } else if (a.tasks && a.tasks.length > 0) {
      a.tasks.forEach((step) => steps.push({ agent: step.agent, task: step.task }));
    } else if (a.agent && a.task) {
      steps.push({ agent: a.agent, task: a.task });
    }
    if (steps.length === 0) return;

    // Create the workflow run (async, but we fire-and-forget; updates will arrive)
    const label = `subagent: ${steps.length} step${steps.length > 1 ? "s" : ""} (${steps.map((s) => s.agent).join("→")})`;
    workflowStore
      .create({ sessionId, projectId, label, origin: "subagent-bridge", steps })
      .then((run) => {
        this.runsByToolCall.set(toolCallId, run.id);
        workflowStore.start(run.id);
        // mark all ready steps as running (the subagent extension runs them immediately)
        for (const step of run.steps) {
          if (step.status === "ready") {
            workflowStore.stepState(run.id, step.id, { status: "running" });
          }
        }
      })
      .catch(() => { /* bridge is best-effort */ });
  }

  private onUpdate(toolCallId: string, partialResult: unknown): void {
    const runId = this.runsByToolCall.get(toolCallId);
    if (!runId) return;
    const details = extractDetails(partialResult);
    if (!details) return;
    this.syncSteps(runId, details);
  }

  private onEnd(toolCallId: string, result: unknown, isError?: boolean): void {
    const runId = this.runsByToolCall.get(toolCallId);
    if (!runId) return;
    const details = extractDetails(result);
    if (details) this.syncSteps(runId, details, true);
    // The subagent tool call has ended, so every step's result should have arrived. Sweep any step
    // still in a non-terminal state (no result reported, or a result that never matched even
    // positionally) to error so the graph never shows a permanently "running"/"ready" node after
    // the run completes. This also covers a tool-call failure that returned no details.
    const run = workflowStore.get(runId);
    if (run) {
      const reason = isError && !details ? "subagent tool call failed" : "step result not reported";
      for (const step of run.steps) {
        if (step.status === "running" || step.status === "ready" || step.status === "pending") {
          workflowStore.stepState(runId, step.id, { status: "error", error: reason }).catch(() => {});
        }
      }
    }
    this.runsByToolCall.delete(toolCallId);
  }

  private syncSteps(runId: string, details: SubagentDetailsLike, final = false): void {
    const run = workflowStore.get(runId);
    if (!run) return;
    details.results.forEach((res, idx) => {
      const key = `${runId}:${idx}:${res.agent}:${res.task.slice(0, 40)}`;
      let stepId = this.stepIds.get(key);
      if (!stepId) {
        // Match the step by agent+task; if the extension reordered or truncated the task string
        // so the exact match misses, fall back to positional match (results arrive in step order).
        const step = run.steps.find((s) => s.agent === res.agent && s.task === res.task) ?? run.steps[idx];
        if (!step) return;
        stepId = step.id;
        this.stepIds.set(key, stepId);
      }
      const status = stepStatus(res.exitCode, res.stopReason);
      const patch: { status: "running" | "done" | "error"; output?: string; error?: string; usage?: { input?: number; output?: number; cost?: number; turns?: number }; sandboxRunId?: string | null; browserSessionId?: string | null; toolCallIds?: string[]; thinking?: string } = { status };
      if (status === "done") patch.output = `(subagent completed on ${res.model || "default model"})`;
      if (status === "error") patch.error = res.stopReason || `exit code ${res.exitCode}`;
      if (res.usage) patch.usage = { input: res.usage.input, output: res.usage.output, cost: res.usage.cost, turns: res.usage.turns };
      if (res.sandboxRunId !== undefined) patch.sandboxRunId = res.sandboxRunId;
      if (res.browserSessionId !== undefined) patch.browserSessionId = res.browserSessionId;
      if (res.toolCallIds !== undefined) patch.toolCallIds = res.toolCallIds;
      if (res.thinking !== undefined) patch.thinking = res.thinking;
      workflowStore.stepState(runId, stepId, patch).catch(() => {});
    });
  }
}

export const workflowBridge = new WorkflowBridge();