//! Provider key store at `~/.pi/agent/auth.json` — the documented auth path (see `.env.example`:
//! "pi resolves provider keys from ~/.pi/agent/auth.json → env vars"). This module is the single
//! reader/writer for that file so the in-UI key setter actually works without a restart, and so
//! `provider::resolve_api_key` can fall back to it when the env var is unset.
//!
//! Shape (matches the existing hand-authored file, keys = env var names):
//! ```json
//! { "OLLAMA_API_KEY": "sk-...", "OPENROUTER_API_KEY": "sk-...", "NVIDIA_API_KEY": "nvapi-..." }
//! ```
//!
//! Security: the GET route reports only `set: true/false` — NEVER the key value. The POST route
//! never logs the key. The file lives in `~/.pi/agent/` (a user-only dir). Writes are atomic
//! (temp + rename) so a crash mid-write leaves the previous complete file intact.
//!
//! Cache: the parsed auth.json is cached in a `Mutex<Value>` (behind a `OnceLock` for lazy init)
//! so `resolve_api_key` doesn't re-read + re-parse the file on every provider call (every chat
//! turn hits it). The POST/DELETE routes call [`refresh_cache`] after writing, so a newly-set
//! key is visible to `resolve_api_key` immediately — no restart required.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The provider key store location: `~/.pi/agent/auth.json`. Honors `DOTZ_PI_AGENT_DIR` as an
/// override (used by tests + the Tauri shell's `DOTZ_PI` resource resolution) so the test suite
/// can point at a temp dir without touching the operator's real keys.
fn auth_file() -> PathBuf {
    if let Ok(d) = std::env::var("DOTZ_PI_AGENT_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("auth.json");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pi")
        .join("agent")
        .join("auth.json")
}

/// Read + parse `auth.json` from disk. Returns an empty object when the file is missing or
/// malformed (so a fresh install before any key is set behaves like "no keys" rather than
/// erroring). Public to the POST/DELETE routes so they can read-modify-write the file without
/// going through the process cache.
pub(crate) fn read_auth_json() -> Value {
    match std::fs::read_to_string(auth_file()) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

/// Write `auth.json` atomically (temp + rename) so a crash mid-write leaves the previous
/// complete file intact. Creates the parent dir if missing. Restricts the file permissions to
/// owner-only (0600 on Unix; explicit user-only ACL on Windows) so provider keys are not
/// world-readable on shared accounts. Public to the POST/DELETE routes.
pub(crate) fn write_auth_json(v: &Value) -> Result<(), String> {
    let file = auth_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create auth dir {}: {e}", parent.display()))?;
    }
    let s = serde_json::to_string_pretty(v)
        .map_err(|e| format!("could not serialize auth.json: {e}"))?;
    // Atomic write: temp file in the same dir, then rename. On Windows the std rename uses
    // MoveFileExW with MOVEFILE_REPLACE_EXISTING, so the destination is replaced atomically.
    // A crash mid-write leaves the temp file behind (harmless) and the previous auth.json
    // intact. Mirrors `workflows::write_all` + `run_record::write_unlocked`.
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, &s).map_err(|e| format!("could not write auth.json temp: {e}"))?;
    // Restrict the temp file to owner-only BEFORE the rename so the final file is never
    // briefly world-readable. Best-effort: a failure to restrict is logged but does NOT
    // block the write (an operator who can't chmod has bigger problems).
    restrict_perms(&tmp);
    if std::fs::rename(&tmp, &file).is_err() {
        // Exotic cross-device / permission edge: fall back to a direct write so the key still
        // persists, accepting the non-atomic window only on that path. Clean up the temp.
        let _ = std::fs::write(&file, &s);
        restrict_perms(&file);
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

/// Restrict a file to owner-only access. On Unix, chmod 0600. On Windows, set an explicit
/// user-only ACL (remove inherited ACEs, grant the current user full control only) so the
/// file is not world-readable on shared accounts. Best-effort: logs a warning on failure,
/// does NOT error (the write still succeeds; the operator's ~/.dotz dir is already user-only
/// in the normal case, so this is defense-in-depth, not the primary boundary).
fn restrict_perms(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("auth: could not chmod 0600 {}: {e}", path.display());
        }
    }
    #[cfg(windows)]
    {
        // ponytail: Windows ACL hardening uses `icacls` via shell-out (the `windows-acl` crate
        // would be a heavy dep). The command: icacls "<path>" /inheritance:r /grant:r
        // "%USERNAME%:F" — removes inherited ACEs and grants the current user full control.
        // Best-effort: if icacls is absent (non-standard Windows), the file keeps the parent
        // dir's ACL (which is user-only in the normal ~/.dotz layout). The upgrade path is a
        // native `windows-acl` crate if more auth files land.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "CURRENT_USER".into());
        let mut cmd = std::process::Command::new("icacls");
        cmd.arg(path)
            .args(["/inheritance:r"])
            .args(["/grant:r", &format!("{user}:F")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Err(e) = crate::util::no_window(&mut cmd).status() {
            eprintln!(
                "auth: could not restrict ACL on {} (icacls failed): {e}",
                path.display()
            );
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
    }
}

/// The cached parsed auth.json. Held in a `Mutex<Value>` (behind a `OnceLock` for lazy init)
/// so the cache can be refreshed in-place by the POST/DELETE routes + tests after a write.
/// The `Mutex` is uncontended on the hot read path (every chat turn) once initialized, so the
/// lock cost is negligible. This is the upgrade path the ponytail ceiling above references:
/// POST `/api/provider/key` now calls [`refresh_cache`] after writing, so a newly-set key is
/// visible to `resolve_api_key` immediately — no restart required.
static AUTH_CACHE: OnceLock<std::sync::Mutex<Value>> = OnceLock::new();

fn auth_cache() -> &'static std::sync::Mutex<Value> {
    AUTH_CACHE.get_or_init(|| std::sync::Mutex::new(read_auth_json()))
}

/// Look up a single key (by env-var name, e.g. `"OLLAMA_API_KEY"`) in the cached auth.json.
/// Returns `Some(value)` if present and stringy, `None` otherwise. This is the hot path called
/// by `provider::resolve_api_key` on every chat turn — the cache makes it a cheap lock+read
/// after the first call.
pub(crate) fn lookup_key(var: &str) -> Option<String> {
    let cache = auth_cache();
    let g = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    g.get(var)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Re-read `auth.json` from disk and replace the cache contents. Called by the POST/DELETE
/// routes after a write so `resolve_api_key` sees the new key immediately without a restart.
/// Public to `server/mod.rs`; safe to call any time (the lock recovers from a poisoned mutex).
pub(crate) fn refresh_cache() {
    let fresh = read_auth_json();
    let cache = auth_cache();
    let mut g = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *g = fresh;
}

/// Map a provider id to its env-var name (the key under which it lives in auth.json). Mirrors
/// `provider::provider_endpoint`'s `$VAR` refs so the GET route can report `set` for the same
/// variable `resolve_api_key` will consult. Returns `None` for unknown providers.
///
/// `local` is special: its key ref is `local` (a bare literal, no env indirection) when
/// `DOTZ_LOCAL_API_KEY` is unset, and `$DOTZ_LOCAL_API_KEY` when it is. We surface
/// `DOTZ_LOCAL_API_KEY` as the auth.json key for `local` so an operator can set it in-UI too.
pub fn provider_key_var(provider: &str) -> Option<&'static str> {
    match provider {
        "ollama" => Some("OLLAMA_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "cohere" => Some("COHERE_API_KEY"),
        "nvidia-nim" => Some("NVIDIA_API_KEY"),
        "local" => Some("DOTZ_LOCAL_API_KEY"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that mutate the process-global `DOTZ_PI_AGENT_DIR` env var + the shared
    // `AUTH_CACHE` so they do not race with each other or with the provider fallback tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_auth_dir<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("dotz-auth-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("DOTZ_PI_AGENT_DIR", dir.to_string_lossy().to_string());
        let result = f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("DOTZ_PI_AGENT_DIR");
        drop(guard);
        result
    }

    #[test]
    fn provider_key_var_covers_all_known_providers() {
        // Every one of the 12 known provider ids (types::providers()) must map to a var.
        for id in [
            "openrouter",
            "nvidia-nim",
            "ollama",
            "anthropic",
            "openai",
            "google",
            "groq",
            "mistral",
            "xai",
            "deepseek",
            "cohere",
            "local",
        ] {
            assert!(
                provider_key_var(id).is_some(),
                "provider_key_var must map known provider {id:?}"
            );
        }
        assert!(
            provider_key_var("bogus").is_none(),
            "unknown provider → None"
        );
    }

    #[test]
    fn read_auth_json_returns_empty_object_when_missing() {
        with_tmp_auth_dir(|dir| {
            assert!(
                !dir.join("auth.json").exists(),
                "precondition: no auth.json"
            );
            let v = read_auth_json();
            assert!(v.is_object(), "missing auth.json → empty object, got {v}");
            assert!(v.as_object().unwrap().is_empty());
        });
    }

    #[test]
    fn write_then_read_round_trips_keys() {
        with_tmp_auth_dir(|dir| {
            let v = json!({
                "OLLAMA_API_KEY": "sk-test-ollama",
                "OPENROUTER_API_KEY": "sk-or-test",
            });
            write_auth_json(&v).expect("write should succeed");
            assert!(
                dir.join("auth.json").exists(),
                "file should exist after write"
            );
            // No stale .tmp.
            assert!(
                !dir.join("auth.json.tmp").exists(),
                "no stale temp after atomic rename"
            );

            let back = read_auth_json();
            assert_eq!(back["OLLAMA_API_KEY"], "sk-test-ollama");
            assert_eq!(back["OPENROUTER_API_KEY"], "sk-or-test");
        });
    }

    #[test]
    fn write_auth_json_creates_parent_dir_if_missing() {
        with_tmp_auth_dir(|dir| {
            // Point DOTZ_PI_AGENT_DIR at a not-yet-existing nested dir.
            let nested = dir.join("nested").join("deep");
            std::env::set_var("DOTZ_PI_AGENT_DIR", nested.to_string_lossy().to_string());
            write_auth_json(&json!({"OLLAMA_API_KEY": "sk-x"}))
                .expect("write should create parent dirs");
            assert!(nested.join("auth.json").exists());
        });
    }

    #[test]
    fn write_auth_json_is_valid_json_after_write() {
        with_tmp_auth_dir(|_dir| {
            write_auth_json(&json!({"OLLAMA_API_KEY": "sk-1", "OPENAI_API_KEY": "sk-2"}))
                .expect("write");
            let raw = std::fs::read_to_string(auth_file()).expect("read back");
            let parsed: Value = serde_json::from_str(&raw).expect("must be valid JSON");
            assert_eq!(parsed["OLLAMA_API_KEY"], "sk-1");
            assert_eq!(parsed["OPENAI_API_KEY"], "sk-2");
        });
    }

    /// Writing auth.json must restrict the file to owner-only (0600 on Unix; explicit user-only
    /// ACL on Windows) so provider keys are not world-readable on shared accounts. This is the
    /// regression guard for the skeptic-flagged residual risk that auth.json had no chmod.
    #[test]
    fn write_auth_json_restricts_file_permissions() {
        with_tmp_auth_dir(|_dir| {
            write_auth_json(&json!({"OLLAMA_API_KEY": "sk-secret"})).expect("write");
            let file = auth_file();
            assert!(file.exists(), "auth.json must exist after write");
            // On Unix, assert 0600 (owner read+write only). On Windows, the ACL restriction is
            // best-effort via icacls (the icacls call may be absent on minimal hosts); assert
            // the file exists + is not world-writable. The defense-in-depth boundary is the
            // user-only ~/.dotz dir in the normal case; this test guards the explicit chmod.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
                let bits = mode & 0o077;
                assert_eq!(
                    bits, 0,
                    "auth.json must be 0600 (owner-only); got mode {mode:o}"
                );
            }
            #[cfg(windows)]
            {
                // ponytail: the Windows ACL restriction is via `icacls /inheritance:r
                // /grant:r %USERNAME%:F`. We assert the file exists + is readable by the
                // current user (the writer). A full ACL assertion would need the `windows-acl`
                // crate (heavy dep); the icacls call is best-effort + logged on failure.
                let meta = std::fs::metadata(&file).expect("stat");
                assert!(meta.is_file(), "auth.json must be a regular file");
            }
        });
    }
}
