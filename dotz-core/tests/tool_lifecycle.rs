#![cfg(target_os = "linux")]

use dotz_core::agent::tools::{ToolCtx, ToolRegistry};
use serde_json::json;
use std::{
    os::fd::AsRawFd,
    process::{Child, Command},
    time::Duration,
};

struct Control(Child);
impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("dotz-tool-lifecycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// SAFETY: this executable has one test; configure its environment before main/worker threads.
#[ctor::ctor]
fn configure_timeouts() {
    unsafe {
        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "1000");
        std::env::set_var("DOTZ_GATE_TIMEOUT_MS", "1000");
    }
}

/// A tool owns its background workers even if the shell exits while they hold output pipes.
#[tokio::test]
async fn bash_and_gate_teardown_root_exit_and_timeout_without_harming_control() {
    let mut control = Control(Command::new("sleep").arg("30").spawn().unwrap());
    let mut failures = Vec::new();
    for tool in ["bash", "rsi_baseline"] {
        for root_exits in [true, false] {
            let dir = TempDir::new();
            let ctx = ToolCtx {
                cwd: dir.path().to_path_buf(),
                tx: None,
                run_id: None,
            };
            let mut registry = ToolRegistry::new();
            registry.set_active(&[tool.into()]);
            let host_proc = std::fs::File::open("/proc").unwrap();
            // SAFETY: this is our fixture descriptor. Inherit the original proc directory
            // to capture host PIDs without changing the production namespace's proc view.
            assert_eq!(
                unsafe { libc::fcntl(host_proc.as_raw_fd(), libc::F_SETFD, 0) },
                0
            );
            let host_stat = format!("/proc/self/fd/{}/self/stat", host_proc.as_raw_fd());
            let command = format!(
                "(read host_pid rest < {host_stat}; printf '%s' \"$host_pid\" > pid; sleep 2; touch late) & while ! test -s pid; do sleep 0.01; done; echo '1 passed, 0 failed'; {}",
                if root_exits { "exit 0" } else { "wait" }
            );
            let started = std::time::Instant::now();
            let result = registry.run(tool, &json!({"command": command}), &ctx).await;
            let elapsed = started.elapsed();
            let text = result.clone().unwrap_or_else(|e| e);
            let pid: u32 = std::fs::read_to_string(dir.path().join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            tokio::time::sleep(Duration::from_millis(2300)).await;
            let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|s| {
                    !matches!(
                        s.rsplit_once(") ").unwrap().1.chars().next(),
                        Some('Z' | 'X')
                    )
                });
            let late = dir.path().join("late").exists();
            let unrelated_alive = control.0.try_wait().unwrap().is_none();
            let correct_result = if root_exits {
                result.is_ok() && !text.contains("[timeout]") && elapsed < Duration::from_secs(2)
            } else {
                text.contains("[timeout]")
            };
            println!(
                "tool={tool} root_exits={root_exits} elapsed={elapsed:?} alive={alive} late={late} unrelated_alive={unrelated_alive} result={text:?}"
            );
            if alive
                || late
                || !unrelated_alive
                || !correct_result
                || text.contains("cleanup incomplete")
            {
                failures.push(format!("{tool} root_exits={root_exits}: alive={alive}, late={late}, unrelated_alive={unrelated_alive}, result={text:?}"));
            }
        }
    }
    let dir = TempDir::new();
    let ctx = ToolCtx {
        cwd: dir.path().to_path_buf(),
        tx: None,
        run_id: None,
    };
    let mut registry = ToolRegistry::new();
    registry.set_active(&["bash".into()]);
    let result = registry.run("bash", &json!({"command": "read stat_pid rest < /proc/self/stat; test \"$stat_pid\" = \"$$\" && echo CONSISTENT_PROC"}), &ctx).await;
    if result.as_deref() != Ok("CONSISTENT_PROC\n") {
        failures.push(format!(
            "namespace PID and procfs view disagree: {result:?}"
        ));
    }
    // Diagnostic failures are asserted only after finite sentinel workers have finished and
    // the unrelated control has been explicitly reaped; no leaked probe persists on RED.
    drop(control);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
