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