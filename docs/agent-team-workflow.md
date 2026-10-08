# Agent-team workflow

This guide describes the bundled prompts and runtime in this checkout. A chat prompt alone does not
guarantee delegation: the selected profile or slash-command prompt guides the lead agent, which
decides whether to call `subagent`. The workflow graph records the subagent calls that actually run.

## Run a reviewed implementation task

1. Start the headless app with `cargo run -p dotz-core --bin serve` and open
   <http://127.0.0.1:4317>, or launch the desktop app.
2. Open or create a project for the repository you intend to change. Configure a provider in the
   Connections panel or with an environment variable; see [provider setup](provider-setup.md). The
   lead and subagents make model-provider requests, so their usage and charges follow that provider.
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
  of up to 8 tasks with at most 4 running at once. A subagent has a five-minute default timeout;
  `DOTZ_SUBAGENT_TIMEOUT_MS` can change it.
- A dispatch-specific model choice takes precedence. Otherwise the configured Subagent model is
  used, then the agent profile's model, then `ollama/minimax-m3` as the fallback. The configured
  Subagent model applies to bundled agents as well as custom agents.
- A reviewer is another model run. It can miss defects and does not replace human review, project
  tests, or inspection of the resulting diff. Provider calls, edits, shell commands, commits, and
  pull requests depend on the tools enabled for the run and the actions requested by its prompt.

For setup and runtime details, see the top-level [README](../README.md),
[provider setup](provider-setup.md), and the bundled workflow prompts under `.pi/prompts/`.
