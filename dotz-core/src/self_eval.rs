//! Self-eval harness — runs a fixed suite of real coding tasks through dotz's OWN agent
//! runtime and tracks pass-rate / cost / wall-time over releases, so the orchestration
//! itself (not just the Rust code) has a gate that catches regressions.
//!
//! The suite is a fixed list of `Task`s, each with a prompt (the coding task), a `setup`
//! (seed the task's working directory with starter files), and a `Grader` (a predicate over
//! the agent's `TaskOutcome` — usually "did the agent write the right file(s) / does the
//! project still build"). The harness drives each task through a `Runner`; the production
//! runner is `SubagentRunner`, which calls `subagent::run_single_agent_public` — the same
//! in-process agent loop the `subagent` tool and the workflow executor use, NOT a separate
//! harness runtime. So a self-eval run exercises the real agent loop (provider streaming →
//! tool dispatch → file writes) end-to-end.
//!
//! A run produces a `Report` (per-task pass/cost/wall-time + aggregates) that is persisted to
//! `<dotz_dir>/ai-agents/self-eval/<release>-<timestamp>.json`, and a one-line summary is
//! appended to `<dotz_dir>/ai-agents/self-eval/history.jsonl` so two releases can be diffed
//! to see which task's pass-rate or cost drifted. Override the directory with
//! `DOTZ_SELF_EVAL_DIR`.
//!
//! The graders are pure functions over the `TaskOutcome` (which carries the task's `cwd`),
//! so the grading + aggregation + persistence logic is unit-testable WITHOUT a live provider:
//! the tests use a `FakeRunner` that writes predetermined files and returns a synthetic
//! outcome, and assert that the right graders fire, the aggregates are correct, and the
//! report/history files round-trip. The real `SubagentRunner` is exercised only by the
//! `selfeval` binary (which needs API keys + network).
//!
//! Failure-tolerant by design: a hung/errored task is recorded as a failing case, never a
//! panic — a single bad task must not abort the whole suite. Persistence failures degrade
//! to "no report written" (logged), never to a panicking run.
use crate::agent::subagent::{self, SingleResult};
use crate::config::dotz_dir;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as i64
}

/// Directory holding self-eval reports + the rolling history. Override with
/// `DOTZ_SELF_EVAL_DIR`; defaults to `<dotz_dir>/ai-agents/self-eval`. An empty-but-set env
/// var is treated as unset so a stray `DOTZ_SELF_EVAL_DIR=` doesn't point us at cwd.
fn self_eval_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DOTZ_SELF_EVAL_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dotz_dir().join("ai-agents").join("self-eval")
}

/// Serialize report/history writes so two concurrent harness runs (e.g. a gate + a manual
/// re-run) don't interleave `history.jsonl` lines. Writes are rare (one per run) so a single
/// global guard is plenty — mirrors the `run_record.rs` pattern.
fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

// ---- graders ----

/// A grader's verdict for one task.
#[derive(Clone, Debug, Serialize)]
pub struct Grade {
    pub pass: bool,
    /// Human-readable reason (surfaced in the report so a regression is bisectable).
    pub reason: String,
}

impl Grade {
    pub fn pass() -> Self {
        Grade {
            pass: true,
            reason: "ok".into(),
        }
    }
    pub fn fail(reason: impl Into<String>) -> Self {
        Grade {
            pass: false,
            reason: reason.into(),
        }
    }
}

/// A predicate over a `TaskOutcome`. Object-safe so the bundled suite can compose graders
/// (All/Any) without generics, and so a task can carry a custom grader closure.
pub trait Grader: Send + Sync {
    fn grade(&self, outcome: &TaskOutcome) -> Grade;
}

/// The agent's observable result for one task. Carries the task's `cwd` so file/command
/// graders read from the directory the agent actually operated in.
#[derive(Clone, Debug)]
pub struct TaskOutcome {
    pub task_id: String,
    /// The agent's final assistant text (concatenated text blocks).
    pub output: String,
    pub exit_code: i64,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    pub cost: f64,
    pub wall_ms: u64,
    pub cwd: PathBuf,
}

// ---- concrete graders ----

/// Pass iff the agent's final text output contains `needle`.
pub struct OutputContains(pub String);
impl Grader for OutputContains {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        if outcome.output.contains(&self.0) {
            Grade::pass()
        } else {
            Grade::fail(format!(
                "agent output did not contain {:?} (got {} bytes)",
                self.0,
                outcome.output.len()
            ))
        }
    }
}

/// Pass iff the runner reported a clean run: no error and exit_code 0. This is the baseline
/// "did the orchestration itself not blow up" gate, independent of the task's artifact.
pub struct RunnerOk;
impl Grader for RunnerOk {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        if outcome.error.is_some() {
            return Grade::fail(format!("runner reported error: {:?}", outcome.error));
        }
        if outcome.exit_code != 0 {
            return Grade::fail(format!("runner exit_code={}", outcome.exit_code));
        }
        Grade::pass()
    }
}

/// Pass iff the file at `path` (relative to the task cwd) exists. A missing file is the most
/// common orchestration regression (the agent claimed success but wrote nothing).
pub struct FileExists(pub String);
impl Grader for FileExists {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let p = outcome.cwd.join(&self.0);
        if p.exists() {
            Grade::pass()
        } else {
            Grade::fail(format!("expected file not found: {}", self.0))
        }
    }
}

/// Pass iff the file at `path` exists and its content contains `needle`.
pub struct FileContains {
    pub path: String,
    pub needle: String,
}
impl Grader for FileContains {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let p = outcome.cwd.join(&self.path);
        match std::fs::read_to_string(&p) {
            Ok(content) if content.contains(&self.needle) => Grade::pass(),
            Ok(_) => Grade::fail(format!(
                "file {} exists but does not contain {:?}",
                self.path, self.needle
            )),
            Err(e) => Grade::fail(format!("could not read {}: {e}", self.path)),
        }
    }
}

/// Pass iff the file at `path` exists and its content exactly equals `expected`.
pub struct FileEquals {
    pub path: String,
    pub expected: String,
}
impl Grader for FileEquals {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let p = outcome.cwd.join(&self.path);
        match std::fs::read_to_string(&p) {
            Ok(content) if content == self.expected => Grade::pass(),
            Ok(content) => Grade::fail(format!(
                "file {} content mismatch ({} bytes, expected {})",
                self.path,
                content.len(),
                self.expected.len()
            )),
            Err(e) => Grade::fail(format!("could not read {}: {e}", self.path)),
        }
    }
}

/// Pass iff running `cmd` in the task cwd exits 0. Used to grade "does the project still
/// build / do its tests pass" tasks — the strongest orchestration gate, since it verifies
/// the agent's edits actually compile and behave. Runs via `sh -c` (unix) / `cmd /C`
/// (windows) so pipelines and redirects work.
pub struct CommandSucceeds(pub String);
impl Grader for CommandSucceeds {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let mut cmd = shell_cmd(&self.0);
        cmd.current_dir(&outcome.cwd);
        match cmd.output() {
            Ok(out) if out.status.success() => Grade::pass(),
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stderr_trim = stderr.trim();
                Grade::fail(format!("command {:?} exited {code}: {stderr_trim}", self.0))
            }
            Err(e) => Grade::fail(format!("could not run command {:?}: {e}", self.0)),
        }
    }
}

/// Build a shell-launched `Command` for the platform. Centralized so the grader and any
/// setup shell steps share one cross-platform entry point.
fn shell_cmd(line: &str) -> std::process::Command {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", line]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", line]);
        c
    };
    crate::util::no_window(&mut c);
    c
}

/// Always-pass sentinel (smoke tasks).
pub struct AlwaysPass;
impl Grader for AlwaysPass {
    fn grade(&self, _outcome: &TaskOutcome) -> Grade {
        Grade::pass()
    }
}

/// All sub-graders must pass. The aggregate reason lists every failing sub-grade so a
/// regression report names the specific check that broke.
pub struct All(pub Vec<Box<dyn Grader>>);
impl Grader for All {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let mut reasons: Vec<String> = Vec::new();
        for g in &self.0 {
            let r = g.grade(outcome);
            if !r.pass {
                reasons.push(r.reason);
            }
        }
        if reasons.is_empty() {
            Grade::pass()
        } else {
            Grade::fail(reasons.join("; "))
        }
    }
}

/// At least one sub-grader must pass.
pub struct Any(pub Vec<Box<dyn Grader>>);
impl Grader for Any {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        let mut last: Option<Grade> = None;
        for g in &self.0 {
            let r = g.grade(outcome);
            if r.pass {
                return Grade::pass();
            }
            last = Some(r);
        }
        match last {
            Some(r) => Grade::fail(format!("no sub-grader passed; last: {}", r.reason)),
            None => Grade::pass(),
        }
    }
}

/// A custom predicate closure. Use sparingly — prefer the concrete graders above so the
/// suite stays declarative and diffable.
pub struct Custom(pub Box<dyn Fn(&TaskOutcome) -> Grade + Send + Sync>);
impl Grader for Custom {
    fn grade(&self, outcome: &TaskOutcome) -> Grade {
        (self.0)(outcome)
    }
}

// ---- task + setup ----

/// Seed the task's working directory with starter files. Run before the agent; receives the
/// temp dir that will be the agent's cwd. A non-Ok result fails the task up front (the agent
/// is not run). The closure is `Fn` (not `FnOnce`) so it can be stored behind `&Task` and
/// invoked from `&self` inside the harness's async `run` — every bundled setup captures nothing,
/// so `Fn` is the right bound and keeps `Task: Send + Sync` so the harness future is `Send`.
pub type SetupFn = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// One coding task in the fixed suite.
pub struct Task {
    pub id: String,
    pub name: String,
    /// Which dotz subagent to drive (e.g. "worker"). The agent's `.pi/agents/<name>.md`
    /// frontmatter pins its tool set + model.
    pub agent: String,
    pub prompt: String,
    /// Optional model override (`provider/model-id`), overriding the agent's frontmatter model.
    pub model_override: Option<String>,
    /// Seed the task cwd with starter files before the agent runs.
    pub setup: Option<SetupFn>,
    pub grader: Box<dyn Grader>,
    /// Per-task wall-clock timeout. The harness aborts a hung task and records it as a fail.
    pub timeout_ms: u64,
}

impl Task {
    /// Convenience constructor with the common defaults (worker agent, 5m timeout, no setup,
    /// no model override).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        prompt: impl Into<String>,
        grader: Box<dyn Grader>,
    ) -> Self {
        Task {
            id: id.into(),
            name: name.into(),
            agent: "worker".into(),
            prompt: prompt.into(),
            model_override: None,
            setup: None,
            grader,
            timeout_ms: 5 * 60 * 1000,
        }
    }
}

// ---- runner ----

/// Drives one task through dotz and returns the observable outcome. Swapping the runner is
/// what makes the harness logic unit-testable without a live provider.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, task: &Task, cwd: &Path) -> TaskOutcome;
}

/// The production runner: drives the task through `subagent::run_single_agent_public` — the
/// same in-process agent loop the `subagent` tool and the workflow executor use. This is the
/// path that makes the self-eval a gate over the actual orchestration, not a parallel runtime.
pub struct SubagentRunner {
    /// Optional model override applied to every task (a task's own `model_override` wins).
    pub model_override: Option<String>,
}

#[async_trait]
impl Runner for SubagentRunner {
    async fn run(&self, task: &Task, cwd: &Path) -> TaskOutcome {
        let model = task
            .model_override
            .clone()
            .or_else(|| self.model_override.clone());
        let start = Instant::now();
        let result: SingleResult = subagent::run_single_agent_public(
            &task.agent,
            &task.prompt,
            model.as_deref(),
            &cwd.to_string_lossy(),
        )
        .await;
        let wall_ms = start.elapsed().as_millis() as u64;
        TaskOutcome {
            task_id: task.id.clone(),
            output: result.final_output(),
            exit_code: result.exit_code,
            stop_reason: result.stop_reason,
            error: result.error_message,
            cost: result.usage.cost,
            wall_ms,
            cwd: cwd.to_path_buf(),
        }
    }
}

// ---- report ----

/// One task's result row in the report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseRecord {
    pub task_id: String,
    pub name: String,
    pub pass: bool,
    pub grade_reason: String,
    pub cost: f64,
    pub wall_ms: u64,
    pub exit_code: i64,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

/// The full report for one run. Aggregates are computed from `cases` so they stay consistent
/// with the per-task rows (no separate counter to drift).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub release: String,
    pub created_at: i64,
    pub passed: usize,
    pub total: usize,
    pub pass_rate: f64,
    pub total_cost: f64,
    pub total_wall_ms: u64,
    pub cases: Vec<CaseRecord>,
}

impl Report {
    /// Build a report from its case rows. Pass-rate is `passed/total` (0.0 when total == 0).
    pub fn from_cases(release: impl Into<String>, cases: Vec<CaseRecord>) -> Self {
        let total = cases.len();
        let passed = cases.iter().filter(|c| c.pass).count();
        let pass_rate = if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        };
        let total_cost = cases.iter().map(|c| c.cost).sum();
        let total_wall_ms = cases.iter().map(|c| c.wall_ms).sum();
        Report {
            release: release.into(),
            created_at: now_ms(),
            passed,
            total,
            pass_rate,
            total_cost,
            total_wall_ms,
            cases,
        }
    }

    /// The gate predicate: pass-rate must meet or exceed `min`. A run with zero tasks fails
    /// (an empty suite is a config error, not a green gate).
    pub fn gate_ok(&self, min_pass_rate: f64) -> bool {
        self.total > 0 && self.pass_rate + 1e-9 >= min_pass_rate
    }

    /// A one-line JSON summary for `history.jsonl` — enough to diff two releases without
    /// reading the full report. The full report path is included so a diff can drill in.
    pub fn history_line(&self, report_path: &Path) -> Value {
        json!({
            "release": self.release,
            "createdAt": self.created_at,
            "passed": self.passed,
            "total": self.total,
            "passRate": self.pass_rate,
            "totalCost": self.total_cost,
            "totalWallMs": self.total_wall_ms,
            "report": report_path.to_string_lossy(),
            "failed": self.cases.iter().filter(|c| !c.pass).map(|c| c.task_id.clone()).collect::<Vec<_>>(),
        })
    }
}

// ---- harness ----

/// The harness owns the fixed suite + a runner and drives every task to a graded outcome.
pub struct Harness {
    pub suite: Vec<Task>,
    pub runner: Box<dyn Runner>,
    /// Release tag stamped onto the report (default: the crate version). Diff two releases
    /// by their tags to see which task drifted.
    pub release: String,
}

impl Harness {
    pub fn new(suite: Vec<Task>, runner: Box<dyn Runner>, release: impl Into<String>) -> Self {
        Harness {
            suite,
            runner,
            release: release.into(),
        }
    }

    /// Run every task in the suite, returning a graded report. Each task gets its own temp
    /// working directory; setup seeds it, the runner drives the agent, the grader scores the
    /// outcome, and a per-task timeout bounds a hung run. A task that fails setup or times out
    /// is recorded as a failing case — it never aborts the suite.
    pub async fn run(&self) -> Report {
        let mut cases: Vec<CaseRecord> = Vec::with_capacity(self.suite.len());
        for task in &self.suite {
            let case = self.run_task(task).await;
            cases.push(case);
        }
        Report::from_cases(&self.release, cases)
    }

    async fn run_task(&self, task: &Task) -> CaseRecord {
        // Per-task temp cwd so a task's files don't leak into the next task's grader. We avoid
        // the `tempfile` crate (not a dependency) and use a uuid-named dir under the platform
        // temp root, removed at scope end so a failed task never leaves litter behind.
        let cwd = std::env::temp_dir().join(format!("dotz-selfeval-task-{}", uuid::Uuid::new_v4()));
        if let Err(e) = std::fs::create_dir_all(&cwd) {
            return fail_case(task, format!("could not create temp cwd: {e}"));
        }
        let cwd_path = cwd.clone();

        // Setup: seed starter files. A failed setup fails the task up front (the agent never
        // runs) — this surfaces a broken suite fixture instead of blaming the agent. Setup is
        // `Fn` so it can be invoked through the `&Task` we hold here.
        if let Some(setup) = task.setup.as_ref() {
            if let Err(e) = setup(&cwd_path) {
                let _ = std::fs::remove_dir_all(&cwd);
                return fail_case(task, format!("task setup failed: {e}"));
            }
        }

        // Drive the runner with a per-task timeout. A timeout is a failing case (not a panic).
        let timeout = Duration::from_millis(task.timeout_ms);
        let outcome = match tokio::time::timeout(timeout, self.runner.run(task, &cwd_path)).await {
            Ok(o) => o,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&cwd);
                return CaseRecord {
                    task_id: task.id.clone(),
                    name: task.name.clone(),
                    pass: false,
                    grade_reason: format!("task timed out after {} ms", task.timeout_ms),
                    cost: 0.0,
                    wall_ms: task.timeout_ms,
                    exit_code: -1,
                    stop_reason: Some("timeout".into()),
                    error: Some(format!("harness timeout after {} ms", task.timeout_ms)),
                };
            }
        };

        // Grade the outcome. The grader reads files from the outcome's cwd (still alive —
        // `cwd` is removed below, after grading).
        let grade = task.grader.grade(&outcome);
        let case = CaseRecord {
            task_id: task.id.clone(),
            name: task.name.clone(),
            pass: grade.pass,
            grade_reason: grade.reason,
            cost: outcome.cost,
            wall_ms: outcome.wall_ms,
            exit_code: outcome.exit_code,
            stop_reason: outcome.stop_reason,
            error: outcome.error,
        };
        // Best-effort cleanup; a grader that left files behind must not poison the next task
        // (each task has its own dir anyway), but tidying keeps the temp root from growing.
        let _ = std::fs::remove_dir_all(&cwd);
        case
    }

    /// Persist a report to `<self_eval_dir()>/<release>-<timestamp>.json` and append a
    /// one-line summary to `history.jsonl`. Returns the report path. Failure-tolerant: a
    /// write error is returned, never panicked.
    pub fn persist(&self, report: &Report) -> Result<PathBuf, String> {
        let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = self_eval_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("could not create {}: {e}", dir.display()));
        }
        let path = dir.join(format!("{}-{}.json", report.release, report.created_at));
        let body =
            serde_json::to_string_pretty(report).map_err(|e| format!("serialize report: {e}"))?;
        std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;

        // Append the rolling summary so two releases can be diffed without reading every
        // full report. A corrupt history line must not block a successful report write.
        let history = dir.join("history.jsonl");
        let line = format!("{}\n", report.history_line(&path));
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
        {
            // The report itself is written; a failed history append degrades to "no diff line",
            // never to a lost report. Surface it but don't fail the whole persist.
            eprintln!("self_eval: could not append history line: {e}");
        }
        Ok(path)
    }
}

/// Build a case record for a task that failed before the agent ran (bad setup / no cwd).
fn fail_case(task: &Task, reason: String) -> CaseRecord {
    CaseRecord {
        task_id: task.id.clone(),
        name: task.name.clone(),
        pass: false,
        grade_reason: reason.clone(),
        cost: 0.0,
        wall_ms: 0,
        exit_code: -1,
        stop_reason: None,
        error: Some(reason),
    }
}

// ---- the fixed bundled suite ----

/// The fixed suite of real coding tasks the gate runs. Deterministic in shape (ids + graders
/// never change between releases); only the prompts may evolve with the codebase. A stable
/// suite is what makes pass-rate comparable over releases — changing the task ids would
/// silently reset the regression baseline.
pub fn bundled_suite() -> Vec<Task> {
    vec![
        // 1. Smoke: write a single known file. The cheapest end-to-end check that the agent
        //    loop + write tool actually land a file on disk. A regression here is a red alert.
        Task::new(
            "smoke-write-file",
            "Smoke: write answer.txt",
            "Create a file named `answer.txt` in the working directory containing exactly the text `42` (no trailing newline, no extra content).",
            Box::new(FileEquals {
                path: "answer.txt".into(),
                expected: "42".into(),
            }),
        ),
        // 2. Write a Rust source file with a public function. Graded by file existence +
        //    content (the function must be present and public). A real, tiny coding task.
        Task::new(
            "rust-add-fn",
            "Rust: add a public add() function to src/lib.rs",
            "Create the file `src/lib.rs` containing a public function `add(a: i64, b: i64) -> i64` that returns the sum of its two arguments. Use exactly that signature.",
            Box::new(All(vec![
                Box::new(FileExists("src/lib.rs".into())),
                Box::new(FileContains {
                    path: "src/lib.rs".into(),
                    needle: "pub fn add(a: i64, b: i64) -> i64".into(),
                }),
            ])),
        ),
        // 3. Two files in one turn. A regression where the agent stops after the first file
        //    write (a known tool-dispatch stall) must be caught here, not only in prod.
        Task::new(
            "two-files",
            "Write two files in one turn",
            "Create two files in the working directory: `a.txt` containing exactly `hello` and `b.txt` containing exactly `world`.",
            Box::new(All(vec![
                Box::new(FileEquals {
                    path: "a.txt".into(),
                    expected: "hello".into(),
                }),
                Box::new(FileEquals {
                    path: "b.txt".into(),
                    expected: "world".into(),
                }),
            ])),
        ),
        // 4. Fix a failing test. The strongest gate: a real cargo project is seeded, the agent
        //    must edit src/lib.rs so `cargo test` passes. Verifies the agent can read, edit, and
        //    verify — the actual orchestration loop, not just a single write.
        Task {
            id: "rust-fix-test".into(),
            name: "Rust: fix a failing cargo test".into(),
            agent: "worker".into(),
            prompt: "The file `src/lib.rs` in the working directory has a bug: `double(2)` returns 5 instead of 4. Read it, fix the bug so the existing test passes, and do NOT change the test. Run `cargo test` to confirm.".into(),
            model_override: None,
            setup: Some(setup_rust_fix_test()),
            grader: Box::new(CommandSucceeds("cargo test -q".into())),
            timeout_ms: 10 * 60 * 1000,
        },
    ]
}

/// Seed a minimal no-deps cargo library project with a deliberately-broken `double` and a
/// test that fails until the agent fixes it. No external crates so the grader's `cargo test`
/// runs offline (the gate must not depend on crates.io availability).
fn setup_rust_fix_test() -> SetupFn {
    Box::new(|cwd: &Path| {
        std::fs::create_dir_all(cwd.join("src")).map_err(|e| e.to_string())?;
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"dotz_eval_fix\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .map_err(|e| e.to_string())?;
        // Bug: returns n+1 instead of n*2. The test asserts double(2) == 4.
        std::fs::write(
            cwd.join("src/lib.rs"),
            "pub fn double(n: i64) -> i64 {\n    n + 1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::double;\n    #[test]\n    fn doubles_two() {\n        assert_eq!(double(2), 4);\n    }\n}\n",
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

// ---- helpers for the binary ----

/// Default release tag = the crate version. Overridable via `DOTZ_SELF_EVAL_RELEASE`.
pub fn release_tag() -> String {
    std::env::var("DOTZ_SELF_EVAL_RELEASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Parse a minimum pass-rate gate (0.0..=1.0) from `DOTZ_SELF_EVAL_MIN_PASS_RATE`; defaults
/// to 1.0 (the gate is green only when every task passes — a single regression fails the
/// release). Clamped to [0.0, 1.0].
pub fn min_pass_rate() -> f64 {
    const MIN: f64 = 0.0;
    const MAX: f64 = 1.0;
    std::env::var("DOTZ_SELF_EVAL_MIN_PASS_RATE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|r| r.clamp(MIN, MAX))
        .unwrap_or(1.0)
}

// ---- module tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Serialize these tests on the process-global env var they mutate
    /// (`DOTZ_SELF_EVAL_DIR`). Mirrors the `ENV_LOCK` pattern in `run_record.rs` — without it,
    /// parallel tests clobber each other's dir mid-run and a `persist` round-trip races
    /// against a sibling test that reset the dir.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[allow(dead_code)]
    struct TmpDir(PathBuf, std::sync::MutexGuard<'static, ()>);
    impl TmpDir {
        fn new() -> Self {
            let g = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let d = std::env::temp_dir().join(format!("dotz-selfeval-{}", Uuid::new_v4()));
            std::env::set_var("DOTZ_SELF_EVAL_DIR", d.to_string_lossy().to_string());
            Self(d, g)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            std::env::remove_var("DOTZ_SELF_EVAL_DIR");
        }
    }

    /// A runner that writes predetermined files and returns a synthetic outcome. Used to
    /// exercise the grading + aggregation + persistence logic without a live provider.
    struct FakeRunner {
        /// Files to write into the task cwd (relative path → content).
        files: Vec<(String, String)>,
        /// The final assistant text to report.
        output: String,
        /// Force an error outcome (None = clean run).
        error: Option<String>,
        /// Reported cost (so aggregation tests can assert a sum).
        cost: f64,
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn run(&self, _task: &Task, cwd: &Path) -> TaskOutcome {
            for (path, content) in &self.files {
                let p = cwd.join(path);
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(p, content);
            }
            TaskOutcome {
                task_id: String::new(),
                output: self.output.clone(),
                exit_code: 0,
                stop_reason: Some("stop".into()),
                error: self.error.clone(),
                cost: self.cost,
                wall_ms: 7,
                cwd: cwd.to_path_buf(),
            }
        }
    }

    fn task(id: &str, grader: Box<dyn Grader>) -> Task {
        Task::new(id, id, "do the thing", grader)
    }

    #[tokio::test]
    async fn file_equals_grader_passes_on_exact_match() {
        let runner = FakeRunner {
            files: vec![("answer.txt".into(), "42".into())],
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let harness = Harness::new(
            vec![task(
                "smoke",
                Box::new(FileEquals {
                    path: "answer.txt".into(),
                    expected: "42".into(),
                }),
            )],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 1);
        assert_eq!(report.total, 1);
        assert!((report.pass_rate - 1.0).abs() < 1e-9);
        assert!(report.gate_ok(1.0));
    }

    #[tokio::test]
    async fn file_equals_grader_fails_on_mismatch() {
        let runner = FakeRunner {
            files: vec![("answer.txt".into(), "43".into())],
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let harness = Harness::new(
            vec![task(
                "smoke",
                Box::new(FileEquals {
                    path: "answer.txt".into(),
                    expected: "42".into(),
                }),
            )],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 0, "mismatched content must fail");
        assert!(!report.gate_ok(1.0));
        assert!(
            report.cases[0].grade_reason.contains("mismatch"),
            "reason should explain the mismatch: {}",
            report.cases[0].grade_reason
        );
    }

    #[tokio::test]
    async fn file_grader_fails_when_file_missing() {
        let runner = FakeRunner {
            files: Vec::new(),
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let harness = Harness::new(
            vec![task("missing", Box::new(FileExists("nope.txt".into())))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 0);
        assert!(report.cases[0].grade_reason.contains("not found"));
    }

    #[tokio::test]
    async fn all_combinator_lists_every_failing_subgrade() {
        let runner = FakeRunner {
            files: vec![("a.txt".into(), "hello".into())], // b.txt missing
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let grader = All(vec![
            Box::new(FileEquals {
                path: "a.txt".into(),
                expected: "hello".into(),
            }),
            Box::new(FileEquals {
                path: "b.txt".into(),
                expected: "world".into(),
            }),
        ]);
        let harness = Harness::new(
            vec![task("two-files", Box::new(grader))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 0);
        // The aggregate reason must name the failing sub-grade (b.txt), not just "failed".
        assert!(
            report.cases[0].grade_reason.contains("b.txt"),
            "All must list the failing sub-grade: {}",
            report.cases[0].grade_reason
        );
    }

    #[tokio::test]
    async fn any_combinator_passes_when_one_passes() {
        let runner = FakeRunner {
            files: vec![("a.txt".into(), "hello".into())],
            output: "done".into(),
            error: None,
            cost: 0.0,
        };
        let grader = Any(vec![
            Box::new(FileExists("missing.txt".into())),
            Box::new(OutputContains("done".into())),
        ]);
        let harness = Harness::new(
            vec![task("any", Box::new(grader))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 1, "Any passes when one sub-grader passes");
    }

    #[tokio::test]
    async fn runner_error_fails_runner_ok_grader() {
        let runner = FakeRunner {
            files: Vec::new(),
            output: String::new(),
            error: Some("provider 500".into()),
            cost: 0.0,
        };
        let harness = Harness::new(
            vec![task("ok", Box::new(RunnerOk))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 0);
        assert!(report.cases[0].grade_reason.contains("provider 500"));
    }

    #[tokio::test]
    async fn command_succeeds_grader_runs_in_task_cwd() {
        // Write a marker file then grade with `test -f marker`. This exercises the real
        // subprocess path (sh -c) on the task's cwd without needing cargo.
        let runner = FakeRunner {
            files: vec![("marker".into(), "x".into())],
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let grader = if cfg!(windows) {
            CommandSucceeds("if exist marker (exit 0) else (exit 1)".into())
        } else {
            CommandSucceeds("test -f marker".into())
        };
        let harness = Harness::new(
            vec![task("cmd", Box::new(grader))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 1, "test -f marker should pass in cwd");
    }

    #[tokio::test]
    async fn command_succeeds_grader_fails_on_nonzero_exit() {
        let runner = FakeRunner {
            files: Vec::new(),
            output: String::new(),
            error: None,
            cost: 0.0,
        };
        let grader = if cfg!(windows) {
            CommandSucceeds("exit 7".into())
        } else {
            CommandSucceeds("false".into())
        };
        let harness = Harness::new(
            vec![task("cmd-fail", Box::new(grader))],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 0);
        assert!(
            report.cases[0].grade_reason.contains("exited"),
            "reason should mention the exit: {}",
            report.cases[0].grade_reason
        );
    }

    #[tokio::test]
    async fn aggregate_sums_cost_and_wall_time_across_tasks() {
        let runner = FakeRunner {
            files: vec![("answer.txt".into(), "42".into())],
            output: String::new(),
            error: None,
            cost: 0.5,
        };
        let harness = Harness::new(
            vec![
                task(
                    "t1",
                    Box::new(FileEquals {
                        path: "answer.txt".into(),
                        expected: "42".into(),
                    }),
                ),
                task(
                    "t2",
                    Box::new(FileEquals {
                        path: "answer.txt".into(),
                        expected: "42".into(),
                    }),
                ),
            ],
            Box::new(runner),
            "test",
        );
        let report = harness.run().await;
        assert_eq!(report.passed, 2);
        assert!((report.total_cost - 1.0).abs() < 1e-9, "total_cost sums");
        assert_eq!(report.total_wall_ms, 14, "wall_ms sums (7+7)");
    }

    #[tokio::test]
    async fn per_task_timeout_records_a_failing_case_without_panicking() {
        // A runner that sleeps past the task's 50ms timeout must be recorded as a fail, not a
        // panic. This is the regression-prevention guarantee: a single hung task can't kill the
        // whole gate.
        struct Slow;
        #[async_trait]
        impl Runner for Slow {
            async fn run(&self, _task: &Task, cwd: &Path) -> TaskOutcome {
                tokio::time::sleep(Duration::from_secs(5)).await;
                TaskOutcome {
                    task_id: String::new(),
                    output: String::new(),
                    exit_code: 0,
                    stop_reason: None,
                    error: None,
                    cost: 0.0,
                    wall_ms: 0,
                    cwd: cwd.to_path_buf(),
                }
            }
        }
        let mut t = task("slow", Box::new(AlwaysPass));
        t.timeout_ms = 50;
        let harness = Harness::new(vec![t], Box::new(Slow), "test");
        let report = harness.run().await;
        assert_eq!(report.passed, 0, "timed-out task fails the gate");
        assert_eq!(report.cases[0].stop_reason.as_deref(), Some("timeout"));
        assert!(report.cases[0].grade_reason.contains("timed out"));
    }

    #[tokio::test]
    async fn failed_setup_fails_the_task_without_running_the_agent() {
        // A setup that errors must surface as a failing case with the setup reason — the agent
        // is never run.
        struct Exploding;
        #[async_trait]
        impl Runner for Exploding {
            async fn run(&self, _task: &Task, _cwd: &Path) -> TaskOutcome {
                panic!("runner must not be called when setup fails");
            }
        }
        let mut t = task("bad-setup", Box::new(AlwaysPass));
        t.setup = Some(Box::new(|_cwd: &Path| {
            Err("boom: cannot seed fixture".into())
        }));
        let harness = Harness::new(vec![t], Box::new(Exploding), "test");
        let report = harness.run().await;
        assert_eq!(report.passed, 0);
        assert!(report.cases[0].grade_reason.contains("setup failed"));
        assert!(report.cases[0].grade_reason.contains("boom"));
    }

    #[tokio::test]
    async fn rust_fix_test_setup_seeds_a_compiling_project_with_a_failing_test() {
        // The bundled `rust-fix-test` fixture must seed a real cargo project whose test fails
        // before the agent runs. We exercise the setup directly and run `cargo test` to confirm
        // it FAILS (the agent hasn't fixed it yet) — proving the fixture is a real gate, not a
        // tautology.
        let cwd = std::env::temp_dir().join(format!("dotz-selfeval-setup-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let setup = setup_rust_fix_test();
        setup(&cwd).expect("setup must succeed");

        // Before the fix, the test must fail.
        let mut cmd = shell_cmd("cargo test -q");
        cmd.current_dir(&cwd);
        let out = cmd.output().expect("cargo test must run");
        assert!(
            !out.status.success(),
            "the seeded test must FAIL before the agent fixes it (it's a real gate)"
        );

        // After simulating the agent's fix (n + 1 -> n * 2), the test must pass — proving the
        // grader's `cargo test` command is the right gate for a fixed project.
        std::fs::write(
            cwd.join("src/lib.rs"),
            "pub fn double(n: i64) -> i64 {\n    n * 2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::double;\n    #[test]\n    fn doubles_two() {\n        assert_eq!(double(2), 4);\n    }\n}\n",
        )
        .unwrap();
        let mut cmd = shell_cmd("cargo test -q");
        cmd.current_dir(&cwd);
        let out = cmd.output().expect("cargo test must run");
        assert!(
            out.status.success(),
            "after the fix the test must pass — the grader's command is sound"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn persist_writes_report_and_appends_history_line() {
        let _tmp = TmpDir::new();
        let runner = FakeRunner {
            files: vec![("answer.txt".into(), "42".into())],
            output: String::new(),
            error: None,
            cost: 0.25,
        };
        let harness = Harness::new(
            vec![task(
                "smoke",
                Box::new(FileEquals {
                    path: "answer.txt".into(),
                    expected: "42".into(),
                }),
            )],
            Box::new(runner),
            "rel-1",
        );
        let report = harness.run().await;
        let path = harness.persist(&report).expect("persist succeeds");
        assert!(path.exists(), "report file must be written");
        assert!(path.to_string_lossy().contains("rel-1"));

        // The history file must have exactly one summary line whose passRate matches.
        let history = self_eval_dir().join("history.jsonl");
        let raw = std::fs::read_to_string(&history).expect("history exists");
        let lines: Vec<&str> = raw.trim_end().lines().collect();
        assert_eq!(lines.len(), 1, "one line per run");
        let v: Value = serde_json::from_str(lines[0]).expect("valid JSON");
        assert_eq!(v["release"], "rel-1");
        assert_eq!(v["passed"], 1);
        assert_eq!(v["total"], 1);
        assert!((v["passRate"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(v["totalCost"].as_f64().unwrap(), 0.25);

        // A second run appends a second line (rolling history, not overwrite).
        let harness2 = Harness::new(
            vec![task(
                "smoke2",
                Box::new(FileEquals {
                    path: "answer.txt".into(),
                    expected: "42".into(),
                }),
            )],
            Box::new(FakeRunner {
                files: vec![("answer.txt".into(), "42".into())],
                output: String::new(),
                error: None,
                cost: 0.5,
            }),
            "rel-2",
        );
        let r2 = harness2.run().await;
        harness2.persist(&r2).unwrap();
        let raw2 = std::fs::read_to_string(&history).unwrap();
        let lines2: Vec<&str> = raw2.trim_end().lines().collect();
        assert_eq!(lines2.len(), 2, "history appends, not overwrites");
        let last: Value = serde_json::from_str(lines2[1]).unwrap();
        assert_eq!(last["release"], "rel-2");
        assert_eq!(last["totalCost"].as_f64().unwrap(), 0.5);
    }

    #[test]
    fn report_gate_ok_requires_nonempty_suite() {
        let empty = Report::from_cases("rel", Vec::new());
        assert!(!empty.gate_ok(0.0), "empty suite must not pass the gate");
        let one_pass = Report::from_cases(
            "rel",
            vec![CaseRecord {
                task_id: "t".into(),
                name: "t".into(),
                pass: true,
                grade_reason: "ok".into(),
                cost: 0.0,
                wall_ms: 0,
                exit_code: 0,
                stop_reason: None,
                error: None,
            }],
        );
        assert!(one_pass.gate_ok(1.0));
    }

    #[test]
    fn min_pass_rate_clamps_and_defaults_to_one() {
        // Default is 1.0 (every task must pass — a single regression fails the release).
        std::env::remove_var("DOTZ_SELF_EVAL_MIN_PASS_RATE");
        assert!((min_pass_rate() - 1.0).abs() < 1e-9);

        // Out-of-range values clamp to [0.0, 1.0].
        std::env::set_var("DOTZ_SELF_EVAL_MIN_PASS_RATE", "1.5");
        assert!((min_pass_rate() - 1.0).abs() < 1e-9);
        std::env::set_var("DOTZ_SELF_EVAL_MIN_PASS_RATE", "-0.2");
        assert!((min_pass_rate() - 0.0).abs() < 1e-9);
        std::env::set_var("DOTZ_SELF_EVAL_MIN_PASS_RATE", "0.75");
        assert!((min_pass_rate() - 0.75).abs() < 1e-9);
        std::env::remove_var("DOTZ_SELF_EVAL_MIN_PASS_RATE");
    }

    #[test]
    fn bundled_suite_ids_are_stable() {
        // The suite's ids are the regression baseline — changing them silently resets the
        // diff. Lock them in a test so an accidental rename fails CI.
        let ids: Vec<String> = bundled_suite().iter().map(|t| t.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                "smoke-write-file",
                "rust-add-fn",
                "two-files",
                "rust-fix-test",
            ]
        );
        // Every task must have a non-empty prompt + a grader-bearing shape.
        for t in bundled_suite() {
            assert!(!t.prompt.is_empty(), "task {} has an empty prompt", t.id);
            assert!(!t.name.is_empty());
            assert!(t.timeout_ms > 0);
        }
    }

    #[test]
    fn self_eval_dir_honors_env_and_falls_back_to_dotz_dir() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("DOTZ_SELF_EVAL_DIR", "/tmp/dotz-eval-xyz");
        assert_eq!(self_eval_dir(), PathBuf::from("/tmp/dotz-eval-xyz"));
        std::env::set_var("DOTZ_SELF_EVAL_DIR", "");
        // Empty-but-set is treated as unset (don't point at cwd).
        assert_ne!(self_eval_dir(), PathBuf::from(""));
        std::env::remove_var("DOTZ_SELF_EVAL_DIR");
    }
}
