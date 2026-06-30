//! Self-eval gate runner — drives the fixed bundled suite through dotz's own agent runtime
//! (`subagent::run_single_agent_public`) and exits non-zero when the pass-rate falls below the
//! configured floor, so a release whose orchestration regressed fails the gate.
//!
//! Configuration (env vars, no clap to keep the dependency surface flat):
//!   DOTZ_SELF_EVAL_RELEASE          release tag stamped on the report (default: crate version)
//!   DOTZ_SELF_EVAL_MIN_PASS_RATE     gate floor in [0.0, 1.0] (default: 1.0 — every task passes)
//!   DOTZ_SELF_EVAL_DIR               where reports + history are written (default: <dotz_dir>/ai-agents/self-eval)
//!   DOTZ_SELF_EVAL_MODEL             model override applied to every task (provider/model-id)
//!   DOTZ_SELF_EVAL_AGENT             agent to drive (default: each task's own, normally "worker")
//!   DOTZ_SELF_EVAL_FILTER            comma-separated task ids to run (default: the whole suite)
//!   DOTZ_SELF_EVAL_SKIP_RUN=1        print the suite + the gate config and exit 0 WITHOUT calling
//!                                    the provider — used by CI to assert the suite is well-formed
//!                                    (stable ids, real graders) without spending tokens.
//!
//! The harness itself is in `dotz_core::self_eval`; this binary is the thin operator entry point.
use dotz_core::self_eval::{self, Harness, SubagentRunner};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let release = self_eval::release_tag();
    let min = self_eval::min_pass_rate();
    let model_override = std::env::var("DOTZ_SELF_EVAL_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let mut suite = self_eval::bundled_suite();

    // Optional agent override: rewrite every task's agent (e.g. pin to "worker" for a smoke run).
    if let Some(agent) = std::env::var("DOTZ_SELF_EVAL_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        for t in suite.iter_mut() {
            t.agent = agent.clone();
        }
    }

    // Optional filter: run only the named task ids (handy for iterating on one task).
    if let Some(filter) = std::env::var("DOTZ_SELF_EVAL_FILTER")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        let want: std::collections::HashSet<&str> = filter.split(',').map(str::trim).collect();
        suite.retain(|t| want.contains(t.id.as_str()));
    }

    eprintln!("dotz self-eval: release={release} min_pass_rate={min} tasks={}", suite.len());
    for t in &suite {
        eprintln!("  - {} ({})", t.id, t.agent);
    }

    // Dry-run mode: assert the suite is well-formed without spending tokens. CI uses this to
    // gate that the suite ids are stable + every task has a grader, independent of provider keys.
    if std::env::var("DOTZ_SELF_EVAL_SKIP_RUN").as_deref() == Ok("1") {
        eprintln!("DOTZ_SELF_EVAL_SKIP_RUN=1 — suite validated, not running the provider.");
        return ExitCode::SUCCESS;
    }

    if suite.is_empty() {
        eprintln!("error: suite is empty after filtering — nothing to run");
        return ExitCode::FAILURE;
    }

    let runner = SubagentRunner { model_override };
    let harness = Harness::new(suite, Box::new(runner), release.clone());
    let report = harness.run().await;

    // Persist the report + rolling history line. A persist failure is logged but does not
    // change the gate verdict — a release either passed the tasks or it didn't, regardless of
    // whether the report file landed on disk.
    match harness.persist(&report) {
        Ok(path) => eprintln!("report written: {}", path.display()),
        Err(e) => eprintln!("warning: could not persist report: {e}"),
    }

    // Print a compact summary table for the operator.
    eprintln!("\nresults:");
    for c in &report.cases {
        let mark = if c.pass { "PASS" } else { "FAIL" };
        eprintln!(
            "  {mark} {id:<20} cost=${cost:.4} {wall}ms  {reason}",
            id = c.task_id,
            cost = c.cost,
            wall = c.wall_ms,
            reason = if c.pass { String::new() } else { c.grade_reason.clone() }
        );
    }
    eprintln!(
        "\npass-rate: {}/{} ({:.0}%)  total cost: ${:.4}  total wall: {}ms",
        report.passed, report.total, report.pass_rate * 100.0, report.total_cost, report.total_wall_ms
    );

    if report.gate_ok(min) {
        eprintln!("GATE: green (pass-rate {:.0}% >= floor {:.0}%)", report.pass_rate * 100.0, min * 100.0);
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "GATE: red (pass-rate {:.0}% < floor {:.0}%) — orchestration regression detected",
            report.pass_rate * 100.0,
            min * 100.0
        );
        ExitCode::FAILURE
    }
}