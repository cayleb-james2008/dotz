# Agent-team workflow

This guide describes the bundled prompts and runtime in this checkout. A chat prompt alone does not
guarantee delegation: the selected profile or slash-command prompt guides the lead agent, which
decides whether to call `subagent`. Lead-session dispatches run through the workflow graph; custom
nested dispatches use the separate path described below.

## Run a reviewed implementation task

1. Start the headless app with `cargo run -p dotz-core --bin serve` and open
   <http://127.0.0.1:4317>, or launch the desktop app.
2. Open or create a project for the repository you intend to change. Configure a provider in the
   Connections panel or with an environment variable; see [provider setup](provider-setup.md). The
   lead and subagents make model-provider requests. Provider-health failover can automatically use
   a backup provider, and a dispatch-specific model can name another provider. Usage and charges
   follow the provider that actually serves each request.
3. Choose a model and subagent model, then submit `/implement-and-review <small, specific change>`.
4. Watch the workflow graph for the dispatched agents and their tool calls. Inspect the review
   output and the actual diff, and run the project checks before accepting the result.

The bundled `/implement-and-review` prompt checks the OpenSpec state, proposes a change if needed,
creates a VCS branch and applies the spec, then runs a sequential `worker → reviewer → worker`
chain. It asks for tests/build and verification, syncs the spec, and commits when verification is
green. It may open a pull request when the GitHub CLI reports a ready state. Use a disposable project
for a first run, and review the diff before accepting any commit or pull request.

For a plan-only pass, `/scout-and-plan` checks/proposes the OpenSpec change and runs a sequential
`scout → planner` chain. It writes spec artifacts only and asks the agents not to edit product code.
The `/implement` preset runs `scout → planner → worker`, followed by the configured project gate,
OpenSpec verification, and sync; it does not include a separate reviewer step. An ordinary WORKFLOW
chat can also delegate, but the prompt itself does not guarantee a particular team or review sequence.

## What the graph and agents mean

- Each subagent is a fresh LLM run with its own prompt and message history, inside the same dotz
  process. It is not a separate operating-system process or an independent security boundary.
- The `subagent` tool supports one run, a sequential chain of up to 16 steps, or parallel dispatch
  of up to 8 tasks. Lead-session calls use the workflow executor, which defaults to 4 concurrent
  steps per workflow run. `DOTZ_WF_CONCURRENCY` is clamped to 1–16; raising it can let all 8 tasks
  in a lead-session parallel dispatch run at once, but does not increase the 8-task dispatch limit.
- Subagents do not have the recursive `subagent` tool enabled by default. A custom agent can
  explicitly include it in its tool list. These nested calls use the direct dispatcher, with at
  most 4 parallel tasks running per dispatch; `DOTZ_WF_CONCURRENCY` does not change that limit.
  They do not create their own executor steps or workflow graph nodes. The enclosing lead-session
  step can show the nested tool call and stop waiting for it, but each nested run has no separate
  executor step deadline.
- A dispatch-specific model choice takes precedence. Otherwise the configured Subagent model is
  used, then the agent profile's model, then `ollama/minimax-m3` as the fallback. The configured
  Subagent model applies to bundled agents as well as custom agents.
- A reviewer is another model run. It can miss defects and does not replace human review, project
  tests, or inspection of the resulting diff. Provider calls, edits, shell commands, commits, and
  pull requests depend on the tools enabled for the run and the actions requested by its prompt.

## Timeout layers

| Setting | Scope |
| --- | --- |
| `DOTZ_PROVIDER_TIMEOUT_MS` | Adapter HTTP request, including its streamed response, for lead and subagent calls. |
| `DOTZ_SUBAGENT_TIMEOUT_MS` | Subagent's wait for each model response stream. |
| `DOTZ_WF_STEP_TIMEOUT_MS` | Lead-session executor's wait for an executing step, after it acquires a concurrency slot. |

All three default to five minutes and are clamped to 1 second–1 hour. To allow a lead-session step
with response streams longer than five minutes, configure all three limits high enough. Increasing
only the subagent and executor limits leaves the provider request's five-minute deadline in place.
Direct nested dispatches use the provider and subagent limits without their own executor deadline.

An executor timeout marks the step errored and stops waiting; it does not reliably abort the
separately spawned provider-stream task. That request may continue until its own deadline or
completion. A workflow timeout or error is not a guarantee of provider cancellation or a billing
ceiling.

For setup and runtime details, see the top-level [README](../README.md),
[provider setup](provider-setup.md), and the bundled workflow prompts under `.pi/prompts/`.
