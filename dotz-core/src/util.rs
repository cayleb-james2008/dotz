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
}
