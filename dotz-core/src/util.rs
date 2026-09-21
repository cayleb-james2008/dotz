//! Tiny shared helpers. Currently: a panic-free wall-clock millis-since-epoch.
//!
//! Several modules (`agent::session`, `agent::subagent`, `agent::provider_health`, `sandbox`,
//! `memory`) previously each defined their own `now_ms()` as
//! `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()`. That `.unwrap()` panics the entire
//! dotz-core server if the system clock is at or before the Unix epoch — a real failure mode in
//! misconfigured containers / CI runners / sandboxes (exactly the environments dotz targets).
//! Other modules (`living_docs`, `specs`, `projects`, `browser`, `checkpoint`, `self_eval`)
//! already used a safe `.unwrap_or(0)`-style idiom; this consolidates the safe idiom into one
//! place so the panic-prone copies could be deleted rather than re-fixed piecemeal.
use std::time::{SystemTime, UNIX_EPOCH};

/// Convert a `SystemTime` to milliseconds since the Unix epoch, returning `0` for a clock at or
/// before the epoch instead of panicking. Pure + injectable so the pre-epoch path is testable
/// without mocking the wall clock.
pub fn millis_since_epoch(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Current wall-clock time as milliseconds since the Unix epoch, panic-free. Returns `0` if the
/// system clock is before the epoch (better a zeroed timestamp than a dead server).
pub fn now_ms() -> i64 {
    millis_since_epoch(SystemTime::now())
}

/// House convention: every spawned console subprocess must be windowless on Windows, or the
/// packaged (`windows_subsystem = "windows"`) app flashes a conhost window over the dashboard
/// on every git/taskkill/gh/openspec spawn. Apply this to a `std::process::Command` before
/// spawning; no-op off Windows. Enforced by `dotz-core/tests/windowless_guard.rs`.
pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// `no_window` for `tokio::process::Command`, which exposes `creation_flags` inherently on
/// Windows (no `CommandExt` import needed). No-op off Windows.
pub fn no_window_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// THE process-wide serialization lock for tests that mutate process-global environment
/// variables (`DOTZ_CONFIG_DIR`, `DOTZ_PI_AGENT_DIR`, `DOTZ_MODELS`, `DOTZ_PI`,
/// `DOTZ_WORKFLOWS_FILE`, `DOTZ_SUBAGENT_TIMEOUT_MS`, timeout overrides, ...).
///
/// Process env vars are process-global: two tests mutating the SAME var under DIFFERENT locks
/// interleave (module A sets `DOTZ_CONFIG_DIR` to dir-a, module B sets it to dir-b, module A's
/// `load_config()` reads dir-b and sees an empty/wrong config), and on this toolchain any
/// concurrent `set_var` + `getenv` is unsafe even across different var names. Per-module locks
/// only serialize within their own module, so every test that calls `set_var`/`remove_var`
/// MUST hold this ONE lock for the whole test body. Async tests hold the std guard across
/// `.await` deliberately (the awaited tasks never acquire this lock, and each test runs on its
/// own runtime, so the deadlock the lint guards against cannot occur); mark those sites with
/// `#[allow(clippy::await_holding_lock)]` plus a one-line justification, matching the existing
/// `memory.rs` idiom. Always recover from poisoning with `unwrap_or_else(into_inner)` so one
/// panicking sibling cannot brick the rest of the suite with cascading `PoisonError`s.
#[cfg(test)]
pub fn env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Shared lock for tests that mutate the `DOTZ_CONFIG_DIR` env var. Both `config::tests` and
/// `telemetry::tests` (and any other module that flips `DOTZ_CONFIG_DIR` to a tmp dir) MUST hold
/// this lock for the whole test body — otherwise two modules' `with_tmp_dir` helpers can race:
/// module A sets `DOTZ_CONFIG_DIR` to dir-a, module B sets it to dir-b, module A's `load_config()`
/// reads from dir-b and sees an empty/wrong config. Observed as an intermittent flake in
/// `telemetry::tests::test_set_enabled_false_clears_endpoint` when a `config::tests::*` test runs
/// concurrently. A single process-wide mutex is the root-cause fix; per-module locks only
/// serialize within their own module.
///
/// This is an alias for [`env_test_lock`]: `DOTZ_CONFIG_DIR` is one of several raced vars, and
/// the tests that flip it also flip sibling vars (`DOTZ_SUBAGENT_MODEL`, `DOTZ_MODELS`, ...),
/// so one shared lock covers the whole family. Existing callers are unchanged.
#[cfg(test)]
pub fn dotz_config_dir_test_lock() -> &'static std::sync::Mutex<()> {
    env_test_lock()
}

/// RAII boot isolation for tests that start the dotz server (`serve_with_shutdown*`,
/// `start_server`). Holds the process-wide [`env_test_lock`] for the whole test body AND
/// points `DOTZ_CONFIG_DIR` + `DOTZ_WORKFLOWS_FILE` at fresh temp dirs (restored + removed
/// on drop), so the boot sequence cannot observe a sibling test's temp files.
///
/// Without this, two boot-time behaviors race the rest of the suite, both reading the LIVE
/// env: `config::load()` (bakes a foreign config into the test server's `AppState`) and
/// `workflows::startup_resume()` (scans `workflows.json` for non-terminal runs, reinserts
/// them into the shared in-memory store, marks steps interrupted, and spawns real executor
/// tasks — observed as a sibling `run_record` test's just-written step status reverting to
/// its pristine value mid-test). Booting with isolated temps also keeps tests from resuming
/// the operator's real runs. Struct-held guard: safe across `.await` (the awaited server
/// tasks never acquire the env lock), matching the existing `AuthDirGuard` idiom — no
/// `clippy::await_holding_lock` allow needed.
#[cfg(test)]
pub struct ServerBootGuard {
    _env: std::sync::MutexGuard<'static, ()>,
    prev_config_dir: Option<String>,
    prev_workflows_file: Option<String>,
    tmp: std::path::PathBuf,
}

/// Acquire boot isolation for a server-booting test. See [`ServerBootGuard`] for why.
#[cfg(test)]
pub fn server_boot_guard() -> ServerBootGuard {
    let env = env_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tmp = std::env::temp_dir().join(format!("dotz-server-boot-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&tmp);
    let prev_config_dir = std::env::var("DOTZ_CONFIG_DIR").ok();
    let prev_workflows_file = std::env::var("DOTZ_WORKFLOWS_FILE").ok();
    // SAFETY comments elsewhere in the suite apply: test-only, lock held.
    unsafe { std::env::set_var("DOTZ_CONFIG_DIR", &tmp) };
    let wf = tmp.join("workflows.json");
    unsafe { std::env::set_var("DOTZ_WORKFLOWS_FILE", &wf) };
    ServerBootGuard {
        _env: env,
        prev_config_dir,
        prev_workflows_file,
        tmp,
    }
}

#[cfg(test)]
impl Drop for ServerBootGuard {
    fn drop(&mut self) {
        match &self.prev_config_dir {
            Some(p) => unsafe { std::env::set_var("DOTZ_CONFIG_DIR", p) },
            None => unsafe { std::env::remove_var("DOTZ_CONFIG_DIR") },
        }
        match &self.prev_workflows_file {
            Some(p) => unsafe { std::env::set_var("DOTZ_WORKFLOWS_FILE", p) },
            None => unsafe { std::env::remove_var("DOTZ_WORKFLOWS_FILE") },
        }
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// RAII pin for `DOTZ_WORKFLOWS_FILE` at a fresh temp file (restored + removed on drop).
/// Does NOT take any lock: the caller must already hold [`env_test_lock()`] (it is
/// non-reentrant, so taking it again would deadlock). Used by server-test guards that
/// already own the lock (`AuthDirGuard`, `GatewayConfigDirGuard`, `FirstRunDirGuard`) so
/// every server boot scans an isolated file — never a sibling's temp file (whose foreign
/// runs would be reinserted into the shared store + executed) nor the operator's real one.
#[cfg(test)]
pub struct WorkflowsFilePin {
    prev: Option<String>,
    file: std::path::PathBuf,
}

/// Pin `DOTZ_WORKFLOWS_FILE` at a fresh temp file. Caller must hold [`env_test_lock()`].
#[cfg(test)]
pub fn pin_workflows_file() -> WorkflowsFilePin {
    let file = std::env::temp_dir().join(format!("dotz-wf-pin-{}.json", uuid::Uuid::new_v4()));
    let prev = std::env::var("DOTZ_WORKFLOWS_FILE").ok();
    unsafe { std::env::set_var("DOTZ_WORKFLOWS_FILE", &file) };
    WorkflowsFilePin { prev, file }
}

#[cfg(test)]
impl Drop for WorkflowsFilePin {
    fn drop(&mut self) {
        match &self.prev {
            Some(p) => unsafe { std::env::set_var("DOTZ_WORKFLOWS_FILE", p) },
            None => unsafe { std::env::remove_var("DOTZ_WORKFLOWS_FILE") },
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A system clock at/before the Unix epoch must yield `0`, not panic. This is the regression
    /// guard for the `.unwrap()` that used to live in five modules' `now_ms()`.
    #[test]
    fn millis_since_epoch_handles_pre_epoch_clock_without_panicking() {
        let pre = UNIX_EPOCH - Duration::from_secs(60);
        assert_eq!(
            millis_since_epoch(pre),
            0,
            "a pre-epoch clock must return 0, not panic"
        );
        assert_eq!(
            millis_since_epoch(UNIX_EPOCH),
            0,
            "the epoch itself must return 0"
        );
    }

    /// A known post-epoch time must yield its exact millis offset.
    #[test]
    fn millis_since_epoch_returns_exact_offset_for_post_epoch() {
        let t = UNIX_EPOCH + Duration::from_secs(60);
        assert_eq!(millis_since_epoch(t), 60_000);
        let t2 = UNIX_EPOCH + Duration::from_millis(1);
        assert_eq!(millis_since_epoch(t2), 1);
    }

    /// After applying `no_window`, a command must still spawn and complete normally on every
    /// platform (mirrors the connections.rs spawn-still-works test for CREATE_NO_WINDOW).
    #[test]
    fn no_window_command_still_spawns_and_completes() {
        let mut c = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "exit 0"]);
            c
        };
        let status = no_window(&mut c).status().expect("command should spawn");
        assert!(status.success(), "windowless command should exit 0");
    }

    /// Same for the tokio variant: `no_window_tokio` must not break spawning.
    #[tokio::test]
    async fn no_window_tokio_command_still_spawns_and_completes() {
        let mut c = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", "exit 0"]);
            c
        };
        let status = no_window_tokio(&mut c)
            .status()
            .await
            .expect("command should spawn");
        assert!(status.success(), "windowless command should exit 0");
    }

    /// `now_ms()` must be non-negative and broadly sane (a smoke test that the helper is wired
    /// up and never panics under the real wall clock).
    #[test]
    fn now_ms_is_non_negative_and_sane() {
        let t = now_ms();
        assert!(t >= 0, "now_ms must never be negative, got {t}");
        // After 2020-01-01 in millis — a loose floor so a wildly broken clock is still caught.
        assert!(
            t > 1_577_836_800_000,
            "now_ms should be a plausible recent timestamp, got {t}"
        );
    }

    /// `now_ms()` must be monotonic (non-decreasing) when called twice in sequence. This
    /// guards the consolidation of the local copies in `workflows.rs` and `self_eval.rs`
    /// into the shared helper: every call site — whether in workflow step-state transitions,
    /// run-record timestamps, or self-eval report creation — must see the same wall clock
    /// and produce timestamps that never go backward.
    #[test]
    fn now_ms_is_monotonic() {
        let t1 = now_ms();
        let t2 = now_ms();
        assert!(t2 >= t1, "now_ms must be monotonic: t1={t1}, t2={t2}");
    }
}
