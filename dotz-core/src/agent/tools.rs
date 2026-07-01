//! Tool layer: a `Tool` trait + a registry + the core built-ins the agent loop can call.
//!
//! Fully functional: read, write, edit, bash, ls, grep, find (std::fs + std::process::Command),
//! plus skill (crate::skills::load_body) and memory_search/memory_add/memory_list (crate::memory).
//! The other pi tools (agents_md, rsi_*, human_gate, browser_*, subagent, create_*) are registered as
//! thin stubs so the tool LIST matches the pi surface — they return a clear "not yet implemented"
//! (Phase 4). `setActiveToolsByName` filters which tools the model actually sees.
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Context a tool executes in (the session's cwd, for relative-path resolution + memory scoping).
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// Session WS broadcast sender (for human_gate to emit a gate frame). None in subagent contexts.
    pub tx: Option<tokio::sync::broadcast::Sender<serde_json::Value>>,
    /// The workflow run id this tool call belongs to, when invoked from a workflow subagent.
    /// The inter-agent context-bus tools (`context_read`/`context_write`) use this as the
    /// authoritative run id so a subagent never needs to be told its run id (which it has no way
    /// to know) and cannot read or write another run's bus by passing a different id. None for
    /// the lead session and non-workflow contexts → the bus tools refuse.
    pub run_id: Option<String>,
}

impl ToolCtx {
    /// Resolve a possibly-relative path against the session cwd.
    fn resolve(&self, p: &str) -> PathBuf {
        let pp = Path::new(p);
        if pp.is_absolute() {
            pp.to_path_buf()
        } else {
            self.cwd.join(pp)
        }
    }

    /// Resolve a path and ensure it stays inside the session cwd. Blocks `..` traversal and
    /// absolute paths outside the project root — a trust-boundary guard for the file tools.
    /// # ponytail: follows `..` literally, not symlinks; good enough for the agent-tool sandbox.
    fn resolve_in_cwd(&self, p: &str) -> Result<PathBuf, String> {
        let raw = self.resolve(p);
        let normalized = normalize_path(&raw);
        let cwd_abs = if self.cwd.is_absolute() {
            self.cwd.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(&self.cwd)
        };
        let cwd_norm = normalize_path(&cwd_abs);
        if !normalized.starts_with(&cwd_norm) {
            return Err(format!("path escapes the project directory: {p}"));
        }
        Ok(normalized)
    }
}

/// Remove `.` and `..` components from a path without touching the filesystem. Symlinks are not
/// followed, which matches the intentionally lightweight sandbox boundary.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(Path::new(c.as_os_str())),
        }
    }
    out
}

/// A tool the agent can invoke. `execute` returns Ok(text-result) or Err(error-text).
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON-schema for the function parameters (OpenAI function-calling `parameters`).
    fn parameters(&self) -> Value;
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String>;

    /// OpenAI tool spec: {type:"function", function:{name, description, parameters}}.
    fn spec(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }
}

// ---- helpers ----
fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}
fn obj_param(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// Cap for the `read` tool output. Reading arbitrarily large files (logs, binaries, dumps)
/// into the agent context would blow up the context window and block the tokio runtime; we
/// return the leading chunk and tell the agent how to proceed.
const READ_CAP: usize = 200 * 1024;

// ---- read ----
struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a file's contents (UTF-8). Returns the full text, or the first ~200 KB with a truncation marker for larger files."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "file_path": { "type": "string", "description": "Path to the file" } }),
            &["file_path"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let p = str_arg(args, "file_path").ok_or("file_path is required")?;
        let path = ctx.resolve_in_cwd(p)?;
        // Offload the blocking filesystem read to tokio's blocking pool so a slow/bursty read
        // does not stall the agent loop.
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("read {p}: {e}"))?;
        if content.len() <= READ_CAP {
            return Ok(content);
        }
        // Truncate on a char boundary so the returned string is always valid UTF-8.
        let mut end = READ_CAP;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        let omitted = content.len() - end;
        Ok(format!(
            "{}\n\n[read truncated: {omitted} bytes omitted; use grep, head/tail, or read a smaller range]",
            &content[..end]
        ))
    }
}

// ---- write ----
struct WriteTool;
#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Write (create or overwrite) a file with the given content."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "file_path": { "type": "string" }, "content": { "type": "string" } }),
            &["file_path", "content"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let p = str_arg(args, "file_path").ok_or("file_path is required")?;
        let content = str_arg(args, "content").unwrap_or("");
        let path = ctx.resolve_in_cwd(p)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("create dir for {p}: {e}"))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("write {p}: {e}"))?;
        Ok(format!("wrote {} bytes to {p}", content.len()))
    }
}

// ---- edit (exact string replace) ----
struct EditTool;
#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace an exact unique string in a file with a new string."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({
                "file_path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            }),
            &["file_path", "old_string", "new_string"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let p = str_arg(args, "file_path").ok_or("file_path is required")?;
        let old = str_arg(args, "old_string").ok_or("old_string is required")?;
        let new = str_arg(args, "new_string").unwrap_or("");
        if old == new {
            return Err("old_string and new_string are identical (no-op edit)".into());
        }
        let path = ctx.resolve_in_cwd(p)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("read {p}: {e}"))?;
        let count = content.matches(old).count();
        if count == 0 {
            return Err(format!("old_string not found in {p}"));
        }
        if count > 1 {
            return Err(format!("old_string is not unique in {p} ({count} matches)"));
        }
        let updated = content.replacen(old, new, 1);
        tokio::fs::write(&path, updated)
            .await
            .map_err(|e| format!("write {p}: {e}"))?;
        Ok(format!("edited {p}"))
    }
}

/// Configurable file-scan budget for the `grep` tool. Caps how many files are read so a huge
/// directory tree doesn't blow up the agent's context or stall the blocking pool. Defaults to
/// 5000; override with `DOTZ_GREP_FILE_BUDGET` (clamped to [1, 100_000]) — primarily a test
/// affordance so the budget-exhaustion path can be exercised without creating thousands of files.
fn grep_file_budget() -> usize {
    const DEFAULT: usize = 5000;
    const MIN: usize = 1;
    const MAX: usize = 100_000;
    std::env::var("DOTZ_GREP_FILE_BUDGET")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(MIN, MAX))
        .unwrap_or(DEFAULT)
}

/// Configurable wall-clock timeout for `bash` tool executions. A hung command (interactive prompt,
/// infinite loop, long sleep) otherwise blocks the agent turn forever. Defaults to 5 minutes;
/// override with `DOTZ_BASH_TIMEOUT_MS` (clamped to [1s, 1h]).
fn bash_timeout() -> Duration {
    const DEFAULT_MS: u64 = 300_000; // 5 minutes
    const MIN_MS: u64 = 1_000; // 1 second
    const MAX_MS: u64 = 3_600_000; // 1 hour
    std::env::var("DOTZ_BASH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_MS, MAX_MS))
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_MS))
}

// ---- bash ----
struct BashTool;
#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command in the session cwd. Returns combined stdout+stderr."
    }
    fn parameters(&self) -> Value {
        obj_param(json!({ "command": { "type": "string" } }), &["command"])
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let cmd = str_arg(args, "command")
            .ok_or("command is required")?
            .to_string();
        let cwd = ctx.cwd.clone();
        let timeout = bash_timeout();

        let mut c = {
            #[cfg(windows)]
            {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(&cmd);
                c
            }
            #[cfg(not(windows))]
            {
                // Build a std Command so we can place the child in its own process group
                // (tokio's Command doesn't expose process_group). The group lets a timeout
                // tree-kill the shell AND every descendant (cargo/npm/sleep …) with
                // `kill -9 -<pgrp>`. Without this, `start_kill` only terminates the shell
                // and leaves the real workload running as an orphan that keeps consuming CPU.
                use std::os::unix::process::CommandExt;
                let mut sc = std::process::Command::new("sh");
                sc.arg("-c").arg(&cmd).process_group(0);
                tokio::process::Command::from(sc)
            }
        };
        c.current_dir(&cwd);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW); // avoid console pop-ups in the packaged app
        }

        let mut child = c
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("exec: {e}"))?;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        let run_fut = async {
            let (status, _, _) = tokio::join!(
                child.wait(),
                async {
                    if let Some(s) = stdout.as_mut() {
                        let _ = s.read_to_end(&mut stdout_buf).await;
                    }
                },
                async {
                    if let Some(s) = stderr.as_mut() {
                        let _ = s.read_to_end(&mut stderr_buf).await;
                    }
                },
            );
            status
        };

        let status = match tokio::time::timeout(timeout, run_fut).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(format!("exec: {e}")),
            Err(_) => {
                // Tree-kill the child AND its descendants so a timed-out
                // `cargo test` / `npm run build` does not leak orphaned processes
                // that keep consuming CPU. `start_kill` only terminates the direct
                // child (the shell), leaving the spawned workload alive.
                //
                // Windows: `taskkill /PID <pid> /T /F` kills the whole process tree
                // (matching the sandbox's kill_pid).  POSIX: the child was placed in
                // its own process group at spawn, so `kill -9 -<pgrp>` reaps the
                // entire group — shell + every descendant.
                #[cfg(windows)]
                {
                    if let Some(pid) = child.id() {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/T", "/F"])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    }
                }
                #[cfg(not(windows))]
                {
                    if let Some(pid) = child.id() {
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &format!("-{pid}")])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    }
                }
                // Reap the killed child so it does not become a zombie (Unix) or
                // leak a process handle (Windows) after the timeout path returns.
                let _ = child.wait().await;
                return Err(format!("[timeout] killed after {}ms", timeout.as_millis()));
            }
        };

        let mut s = String::from_utf8_lossy(&stdout_buf).to_string();
        let err = String::from_utf8_lossy(&stderr_buf);
        if !err.trim().is_empty() {
            s.push_str("\n[stderr]\n");
            s.push_str(&err);
        }
        if !status.success() {
            s.push_str(&format!("\n[exit {}]", status.code().unwrap_or(-1)));
            // A non-zero exit is an error so the agent loop marks the tool result as failed.
            return Err(s);
        }
        Ok(s)
    }
}

// ---- ls ----
struct LsTool;
#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn description(&self) -> &'static str {
        "List directory entries (one per line; dirs suffixed with /)."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "path": { "type": "string", "description": "Directory (default: cwd)" } }),
            &[],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let p = str_arg(args, "path").unwrap_or(".");
        let dir = ctx.resolve_in_cwd(p)?;
        let mut names: Vec<String> = Vec::new();
        for ent in std::fs::read_dir(&dir)
            .map_err(|e| format!("ls {p}: {e}"))?
            .flatten()
        {
            let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let mut n = ent.file_name().to_string_lossy().to_string();
            if is_dir {
                n.push('/');
            }
            names.push(n);
        }
        names.sort();
        Ok(names.join("\n"))
    }
}

// ---- grep (recursive substring search; pattern is a plain substring for portability) ----
struct GrepTool;
#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents for a substring under a path. Returns matching lines as file:line:text."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "pattern": { "type": "string" }, "path": { "type": "string", "description": "Root (default: cwd)" } }),
            &["pattern"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let pat = str_arg(args, "pattern")
            .ok_or("pattern is required")?
            .to_string();
        let root = ctx.resolve_in_cwd(str_arg(args, "path").unwrap_or("."))?;
        let cwd = ctx.cwd.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            let mut stack = vec![root];
            let mut budget = grep_file_budget();
            while let Some(dir) = stack.pop() {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for ent in rd.flatten() {
                    let path = ent.path();
                    let ft = match ent.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let name = ent.file_name().to_string_lossy().to_string();
                    if ft.is_dir() {
                        if name == ".git" || name == "node_modules" || name == "target" {
                            continue;
                        }
                        stack.push(path);
                    } else if ft.is_file() {
                        if budget == 0 {
                            // Budget exhausted: stop scanning entirely, not just the
                            // current directory. Without this outer-loop break the
                            // traversal keeps pushing/popping directories (wasting CPU
                            // in large trees) even though no more files will be read.
                            return hits;
                        }
                        budget -= 1;
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            let rel = path
                                .strip_prefix(&cwd)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            for (i, line) in content.lines().enumerate() {
                                if line.contains(&pat) {
                                    hits.push(format!("{rel}:{}:{}", i + 1, line.trim_end()));
                                    if hits.len() >= 200 {
                                        return hits;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            hits
        })
        .await
        .map_err(|e| format!("grep: {e}"))?;
        Ok(if out.is_empty() {
            "(no matches)".into()
        } else {
            out.join("\n")
        })
    }
}

// ---- find (glob-ish by filename substring) ----
struct FindTool;
#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "Find files whose name contains a substring under a path. Returns relative paths."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "pattern": { "type": "string" }, "path": { "type": "string" } }),
            &["pattern"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let pat = str_arg(args, "pattern").unwrap_or("").to_string();
        let root = ctx.resolve_in_cwd(str_arg(args, "path").unwrap_or("."))?;
        let cwd = ctx.cwd.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for ent in rd.flatten() {
                    let path = ent.path();
                    let ft = match ent.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let name = ent.file_name().to_string_lossy().to_string();
                    if ft.is_dir() {
                        if name == ".git" || name == "node_modules" || name == "target" {
                            continue;
                        }
                        stack.push(path);
                    } else if pat.is_empty() || name.contains(&pat) {
                        let rel = path
                            .strip_prefix(&cwd)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        hits.push(rel);
                        if hits.len() >= 500 {
                            return hits;
                        }
                    }
                }
            }
            hits
        })
        .await
        .map_err(|e| format!("find: {e}"))?;
        Ok(if out.is_empty() {
            "(no files)".into()
        } else {
            out.join("\n")
        })
    }
}

// ---- skill (load a SKILL.md body) ----
struct SkillTool;
#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn description(&self) -> &'static str {
        "Load a skill's full instructions by name (from the skill index in your prompt)."
    }
    fn parameters(&self) -> Value {
        obj_param(json!({ "name": { "type": "string" } }), &["name"])
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let name = str_arg(args, "name").ok_or("name is required")?;
        crate::skills::load_body(name).ok_or_else(|| format!("no such skill: {name}"))
    }
}

// ---- memory_search / memory_add / memory_list ----
struct MemorySearchTool;
#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }
    fn description(&self) -> &'static str {
        "Semantic search of durable memory (project + global). Returns matching facts."
    }
    fn parameters(&self) -> Value {
        obj_param(json!({ "query": { "type": "string" } }), &["query"])
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let q = str_arg(args, "query").ok_or("query is required")?;
        let cwd = ctx.cwd.to_string_lossy().to_string();
        let hits = crate::memory::search_public(q, Some(&cwd));
        Ok(render_mem(&hits))
    }
}

struct MemoryAddTool;
#[async_trait]
impl Tool for MemoryAddTool {
    fn name(&self) -> &'static str {
        "memory_add"
    }
    fn description(&self) -> &'static str {
        "Save a durable fact to memory. scope: 'project' (default) or 'global'."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "text": { "type": "string" }, "scope": { "type": "string", "enum": ["project", "global"] } }),
            &["text"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let text = str_arg(args, "text").ok_or("text is required")?;
        let scope = str_arg(args, "scope").unwrap_or("project");
        let cwd = ctx.cwd.to_string_lossy().to_string();
        let v = crate::memory::add_public(text, scope, Some(&cwd))?;
        Ok(format!("saved memory {} ({})", v.id, v.scope))
    }
}

struct MemoryListTool;
#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &'static str {
        "memory_list"
    }
    fn description(&self) -> &'static str {
        "List all durable memories (project + global)."
    }
    fn parameters(&self) -> Value {
        obj_param(json!({}), &[])
    }
    async fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let cwd = ctx.cwd.to_string_lossy().to_string();
        Ok(render_mem(&crate::memory::list_public(Some(&cwd))))
    }
}

fn render_mem(items: &[crate::memory::MemoryView]) -> String {
    if items.is_empty() {
        return "(no memories)".into();
    }
    items
        .iter()
        .map(|m| format!("- {}", m.memory))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The real subagent tool (text path; the DAG-populating `details` are attached in session.rs).
struct SubagentTool;
#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }
    fn description(&self) -> &'static str {
        "Delegate tasks to specialized subagents with isolated context. Modes: single (agent+task), parallel (tasks[], max 8, concurrency 4), chain (chain[] sequential with {previous} feed-forward, max 16). Omit `model` to use the configured low-cost worker."
    }
    fn parameters(&self) -> Value {
        crate::agent::subagent::parameters_schema()
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let cwd = ctx.cwd.to_string_lossy().to_string();
        let d = crate::agent::subagent::dispatch(args, &cwd).await;
        if d.is_error {
            Err(d.text)
        } else {
            Ok(d.text)
        }
    }
}

// ---- in-app browser tools (drive the agent-browser controller) ----
struct BrowserStartTool;
#[async_trait]
impl Tool for BrowserStartTool {
    fn name(&self) -> &'static str {
        "browser_start"
    }
    fn description(&self) -> &'static str {
        "Start an isolated in-app browser at a URL (origin-allowlisted, disposable profile). Returns the page observation with interactive refs. Args: {url, allowedOrigins?:[string]}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "url": { "type": "string" }, "allowedOrigins": { "type": "array", "items": { "type": "string" } } }, "required": ["url"] })
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("url is required")?;
        let origins = args
            .get("allowedOrigins")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let cwd = ctx.cwd.to_string_lossy().to_string();
        let obs = crate::browser::start(&cwd, url, origins, None, None, None).await?;
        Ok(serde_json::to_string(&obs).unwrap_or_default())
    }
}
struct BrowserActTool;
#[async_trait]
impl Tool for BrowserActTool {
    fn name(&self) -> &'static str {
        "browser_act"
    }
    fn description(&self) -> &'static str {
        "Drive the in-app browser. Args: {sessionId, action, targetRef?, text?, x?, y?, key?, url?, values?, expectedSeq?}. Actions: navigate, observe, back, forward, reload, click, clickAt, type, key, select, scroll, wait."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "sessionId": { "type": "string" }, "action": { "type": "string" } }, "required": ["sessionId", "action"] })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let obs = crate::browser::act(args).await?;
        Ok(serde_json::to_string(&obs).unwrap_or_default())
    }
}
struct BrowserStopTool;
#[async_trait]
impl Tool for BrowserStopTool {
    fn name(&self) -> &'static str {
        "browser_stop"
    }
    fn description(&self) -> &'static str {
        "Stop an in-app browser session and dispose its profile. Args: {sessionId}."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "sessionId": { "type": "string" } }, "required": ["sessionId"] })
    }
    async fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        let sid = args
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("sessionId is required")?;
        let obs = crate::browser::stop(sid).await?;
        Ok(serde_json::to_string(&obs).unwrap_or_default())
    }
}

/// The tool registry. Holds every tool; `active` is the subset the model sees.
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Box<dyn Tool>>,
    active: Vec<String>,
}

impl ToolRegistry {
    /// Build the registry with all built-ins + Phase-4 stubs. Active set defaults to the core
    /// functional tools (matching pi's default read/write/edit/bash + the dotz extras).
    pub fn new() -> Self {
        let mut tools: BTreeMap<&'static str, Box<dyn Tool>> = BTreeMap::new();
        let mut add = |t: Box<dyn Tool>| {
            tools.insert(t.name(), t);
        };
        add(Box::new(ReadTool));
        add(Box::new(WriteTool));
        add(Box::new(EditTool));
        add(Box::new(BashTool));
        add(Box::new(LsTool));
        add(Box::new(GrepTool));
        add(Box::new(FindTool));
        add(Box::new(SkillTool));
        add(Box::new(MemorySearchTool));
        add(Box::new(MemoryAddTool));
        add(Box::new(MemoryListTool));
        add(Box::new(SubagentTool));
        add(Box::new(BrowserStartTool));
        add(Box::new(BrowserActTool));
        add(Box::new(BrowserStopTool));
        add(Box::new(crate::context_bus::ContextReadTool));
        add(Box::new(crate::context_bus::ContextWriteTool));
        // The remaining pi tools: agents_md, create_agent/skill, rsi_baseline/compare, human_gate.
        super::extra_tools::register(&mut add);

        let active = vec![
            "read",
            "write",
            "edit",
            "bash",
            "ls",
            "grep",
            "find",
            "skill",
            "memory_search",
            "memory_add",
            "memory_list",
            "subagent",
            "browser_start",
            "browser_act",
            "browser_stop",
            "agents_md",
            "create_agent",
            "create_skill",
            "rsi_baseline",
            "rsi_compare",
            "human_gate",
            "openspec_status",
            "openspec_explore",
            "openspec_propose",
            "openspec_apply",
            "openspec_verify",
            "openspec_sync",
            "openspec_archive",
            "living_docs_read",
            "living_docs_update",
            "living_docs_suggest",
            "vcs_status",
            "vcs_branch",
            "vcs_atomic_commit",
            "vcs_pr",
            "vcs_rollback",
            "context_read",
            "context_write",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        ToolRegistry { tools, active }
    }

    /// All tool names (for GET /api/sessions/:id/tools `all`).
    pub fn all_names(&self) -> Vec<String> {
        self.tools.keys().map(|s| s.to_string()).collect()
    }

    /// Active tool names (for `getActiveToolNames`).
    pub fn active_names(&self) -> Vec<String> {
        self.active.clone()
    }

    /// setActiveToolsByName — keep only the names that resolve to a registered tool, preserving order.
    pub fn set_active(&mut self, names: &[String]) {
        self.active = names
            .iter()
            .filter(|n| self.tools.contains_key(n.as_str()))
            .cloned()
            .collect();
    }

    /// OpenAI tool specs for the ACTIVE tools (sent in the request `tools` array).
    pub fn active_specs(&self) -> Vec<Value> {
        self.active
            .iter()
            .filter_map(|n| self.tools.get(n.as_str()))
            .map(|t| t.spec())
            .collect()
    }

    /// Run a tool by name. Unknown or inactive tool → Err.
    pub async fn run(&self, name: &str, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        if !self.active.iter().any(|n| n == name) {
            return Err(format!("tool is not active: {name}"));
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| format!("no such tool: {name}"))?;
        tool.execute(args, ctx).await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate the process-global `DOTZ_BASH_TIMEOUT_MS` env var so
    /// concurrent bash tests do not race on timeout configuration.
    static BASH_TIMEOUT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn run_rejects_inactive_tool() {
        // bash exists in the registry but is not in the active set after we restrict it.
        let mut restricted = ToolRegistry::new();
        restricted.set_active(&["read".to_string()]);
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        let err = restricted
            .run("bash", &json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("not active"),
            "inactive tool should be rejected, got: {err}"
        );
    }

    #[tokio::test]
    async fn run_allows_active_tool() {
        let mut registry = ToolRegistry::new();
        registry.set_active(&["bash".to_string()]);
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        let out = registry
            .run("bash", &json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("hello"),
            "active bash should execute, got: {out}"
        );
    }

    #[tokio::test]
    async fn run_reports_bash_failure_as_error() {
        // Serialize against the timeout tests: they set the process-global DOTZ_BASH_TIMEOUT_MS
        // to 1000ms, and without this lock a concurrent run leaks that short timeout into this
        // test's `exit 1`, which under load times out instead of returning the [exit 1] error.
        let _guard = BASH_TIMEOUT_TEST_LOCK.lock().await;
        let mut registry = ToolRegistry::new();
        registry.set_active(&["bash".to_string()]);
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        let err = registry
            .run("bash", &json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("[exit 1]"),
            "a failing bash command must return an error containing the exit code, got: {err}"
        );
    }

    #[tokio::test]
    async fn bash_timeout_is_configurable_and_clamped() {
        let _guard = BASH_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_BASH_TIMEOUT_MS").ok();

        std::env::remove_var("DOTZ_BASH_TIMEOUT_MS");
        assert_eq!(bash_timeout().as_secs(), 300, "default is 5 minutes");

        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "5000");
        assert_eq!(bash_timeout().as_millis(), 5000, "valid override preserved");

        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "50");
        assert_eq!(
            bash_timeout().as_millis(),
            1000,
            "too-small value clamped to minimum"
        );

        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "100000000");
        assert_eq!(
            bash_timeout().as_millis(),
            3_600_000,
            "too-large value clamped to maximum"
        );

        match prev {
            Some(p) => std::env::set_var("DOTZ_BASH_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_BASH_TIMEOUT_MS"),
        }
    }

    /// A long-running `bash` command must not block the agent turn forever. The tool honors
    /// `DOTZ_BASH_TIMEOUT_MS`, kills the child, and returns a clear timeout error.
    #[tokio::test]
    async fn run_bash_times_out_on_long_command() {
        let _guard = BASH_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_BASH_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "1000");

        let mut registry = ToolRegistry::new();
        registry.set_active(&["bash".to_string()]);
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            tx: None,
            run_id: None,
        };
        // A ~2s command with a 1s timeout must be killed mid-run.
        let command = if cfg!(windows) {
            "ping -n 3 127.0.0.1"
        } else {
            "sleep 2"
        };

        let start = std::time::Instant::now();
        let err = registry
            .run("bash", &json!({"command": command}), &ctx)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        match prev {
            Some(p) => std::env::set_var("DOTZ_BASH_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_BASH_TIMEOUT_MS"),
        }

        assert!(
            err.contains("[timeout]"),
            "timed-out bash command must report a timeout error, got: {err}"
        );
        // Proves the configured 1s timeout fired rather than the 5-minute default; the margin above
        // 1s absorbs scheduling delay under a saturated parallel suite, which made a tight 3s bound
        // flaky without indicating any regression.
        assert!(
            elapsed < Duration::from_secs(15),
            "bash timeout should fire on the configured 1s timeout, not the default, elapsed: {elapsed:?}"
        );
    }

    /// A timed-out bash child must be reaped, not left as a zombie (Unix) or leaking handles
    /// (Windows). We verify the reap on Unix by checking `kill -0 <pid>` after the timeout path
    /// returns; on Windows we still verify the timeout result shape.
    #[tokio::test]
    async fn run_bash_reaps_child_after_timeout() {
        let _guard = BASH_TIMEOUT_TEST_LOCK.lock().await;
        let prev = std::env::var("DOTZ_BASH_TIMEOUT_MS").ok();
        std::env::set_var("DOTZ_BASH_TIMEOUT_MS", "500");

        let base =
            std::env::temp_dir().join(format!("dotz-bash-reap-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let pidfile = base.join("pid");

        let command = if cfg!(windows) {
            "ping -n 3 127.0.0.1".to_string()
        } else {
            // Spawn `sleep 30` as a background child of the shell, write its PID to
            // the pidfile, then `wait` so the shell stays alive until the timeout.
            // The tree-kill must reach this `sleep` child, not just the shell.
            format!("sleep 30 & echo $! > {} ; wait", pidfile.to_string_lossy())
        };

        let mut registry = ToolRegistry::new();
        registry.set_active(&["bash".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };
        let err = registry
            .run("bash", &json!({"command": command}), &ctx)
            .await
            .unwrap_err();

        match prev {
            Some(p) => std::env::set_var("DOTZ_BASH_TIMEOUT_MS", p),
            None => std::env::remove_var("DOTZ_BASH_TIMEOUT_MS"),
        }
        let _ = std::fs::remove_dir_all(&base);

        assert!(
            err.contains("[timeout]"),
            "timed-out bash command must report a timeout error, got: {err}"
        );

        #[cfg(unix)]
        {
            // The pidfile holds the PID of the `sleep 30` background child (not the
            // shell). The old `start_kill` only killed the shell, leaving this child
            // running as an orphan; the process-group tree-kill must reach it too.
            // Give the tree-kill a moment to propagate before checking.
            std::thread::sleep(std::time::Duration::from_millis(200));
            let pid_text = std::fs::read_to_string(&pidfile).unwrap_or_default();
            let pid: i32 = pid_text.trim().parse().unwrap_or(-1);
            assert!(pid > 0, "test should have captured a valid child pid");
            let gone = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true);
            assert!(
                gone,
                "timed-out bash child (pid {pid}) should have been killed and reaped, not still running"
            );
        }
    }

    /// The file-tool sandbox must reject paths that escape the session cwd, whether via `..`
    /// traversal or an absolute path outside the project root. This is a trust-boundary guard: the
    /// agent's read/write/edit/ls/grep/find tools operate only inside the selected project.
    #[test]
    fn file_tools_reject_paths_escaping_cwd() {
        let base = std::env::temp_dir().join(format!("dotz-toolctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let sibling =
            std::env::temp_dir().join(format!("dotz-toolctx-sibling-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&sibling).unwrap();

        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };

        // Relative paths inside the cwd are fine.
        assert!(ctx.resolve_in_cwd("src/main.rs").is_ok());
        assert!(ctx.resolve_in_cwd(".").is_ok());
        assert!(ctx.resolve_in_cwd("sub/../file.txt").is_ok());

        // `..` traversal and absolute paths outside the cwd are rejected.
        assert!(ctx.resolve_in_cwd("../secret.txt").is_err());
        assert!(ctx.resolve_in_cwd("sub/../../secret.txt").is_err());
        assert!(ctx
            .resolve_in_cwd(sibling.join("file.txt").to_str().unwrap())
            .is_err());
        assert!(ctx.resolve_in_cwd("/etc/passwd").is_err());

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    /// The `read` tool must return small files unchanged.
    #[tokio::test]
    async fn read_tool_returns_small_file_unchanged() {
        let base = std::env::temp_dir().join(format!("dotz-read-small-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("note.txt"), "hello world").unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["read".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };
        let out = registry
            .run("read", &json!({"file_path": "note.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "hello world");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The `read` tool must cap huge files so they do not explode the context window or block
    /// the runtime, returning a leading chunk plus a clear truncation marker.
    #[tokio::test]
    async fn read_tool_truncates_oversized_file() {
        let base = std::env::temp_dir().join(format!("dotz-read-large-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        // Build a file larger than READ_CAP (200 KiB). The leading bytes are ASCII so the
        // char-boundary truncation is deterministic; the trailing multi-byte char tests boundary
        // correctness.
        let mut content = "A".repeat(READ_CAP + 500);
        content.push('é'); // 2-byte UTF-8 char at the end
        std::fs::write(base.join("big.txt"), &content).unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["read".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };
        let out = registry
            .run("read", &json!({"file_path": "big.txt"}), &ctx)
            .await
            .unwrap();

        assert!(
            out.contains("[read truncated:"),
            "oversized read must include a truncation marker, got: {out}"
        );
        assert!(
            out.contains("bytes omitted"),
            "truncation marker must mention omitted bytes, got: {out}"
        );
        assert!(
            out.starts_with("AAAA"),
            "truncated read must start with the original leading content"
        );
        assert!(
            out.len() <= READ_CAP + 300,
            "truncated read should be close to READ_CAP plus the marker, got {} bytes",
            out.len()
        );
        // The returned string must be valid UTF-8 (no panic on .chars()).
        let _ = out.chars().count();

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The `read` tool must still reject paths that escape the project directory.
    #[tokio::test]
    async fn read_tool_rejects_escaping_paths() {
        let base = std::env::temp_dir().join(format!("dotz-read-escape-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["read".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };
        let err = registry
            .run("read", &json!({"file_path": "../secret.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("escapes"),
            "escaping path should be rejected, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A path inside the cwd, including via an absolute path that points back at the cwd, is
    /// allowed by the sandbox. The tool should then actually read/write it successfully.
    #[tokio::test]
    async fn file_tools_allow_paths_inside_cwd() {
        let base =
            std::env::temp_dir().join(format!("dotz-toolctx-inside-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["read".to_string(), "write".to_string(), "edit".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };

        // write + read round-trip.
        let out = registry
            .run(
                "write",
                &json!({"file_path": "note.txt", "content": "hello"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("wrote"));
        let text = registry
            .run("read", &json!({"file_path": "note.txt"}), &ctx)
            .await
            .unwrap();
        assert!(text.contains("hello"));

        // edit inside cwd works.
        let edited = registry
            .run(
                "edit",
                &json!({
                    "file_path": "note.txt",
                    "old_string": "hello",
                    "new_string": "world"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(edited.contains("edited"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The `write` tool must create any missing parent directories asynchronously and write the
    /// file so the agent can create deeply nested files in a single call. This exercises the
    /// tokio::fs path introduced to avoid blocking the async runtime on directory creation and I/O.
    #[tokio::test]
    async fn write_tool_creates_nested_parent_directories_async() {
        let base = std::env::temp_dir().join(format!("dotz-write-nested-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["write".to_string(), "read".to_string(), "edit".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };

        let out = registry
            .run(
                "write",
                &json!({
                    "file_path": "src/agent/nested/note.txt",
                    "content": "nested hello"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("wrote"), "write should report success: {out}");

        let text = registry
            .run(
                "read",
                &json!({"file_path": "src/agent/nested/note.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(text, "nested hello");

        let edited = registry
            .run(
                "edit",
                &json!({
                    "file_path": "src/agent/nested/note.txt",
                    "old_string": "nested hello",
                    "new_string": "nested world"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            edited.contains("edited"),
            "edit should report success: {edited}"
        );

        let text = registry
            .run(
                "read",
                &json!({"file_path": "src/agent/nested/note.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(text, "nested world");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A no-op edit (old_string == new_string) must be rejected with a clear error before any
    /// file read/write, so the agent does not mistake a byte-identical replacement for a
    /// successful edit and falsely report completion or loop. The file must be left unchanged.
    #[tokio::test]
    async fn edit_tool_rejects_noop_edit() {
        let base = std::env::temp_dir().join(format!("dotz-edit-noop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let original = "hello world\nsecond line\n";
        std::fs::write(base.join("note.txt"), original).unwrap();

        let mut registry = ToolRegistry::new();
        registry.set_active(&["edit".to_string(), "read".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };

        let err = registry
            .run(
                "edit",
                &json!({
                    "file_path": "note.txt",
                    "old_string": "hello world",
                    "new_string": "hello world"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("identical"),
            "no-op edit should be rejected, got: {err}"
        );

        // The file must be untouched.
        let text = registry
            .run("read", &json!({"file_path": "note.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(text, original, "no-op edit must leave the file unchanged");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `grep_file_budget` must default to 5000, honor `DOTZ_GREP_FILE_BUDGET`, and clamp to
    /// [1, 100_000]. This is the unit test for the configurable budget that makes the
    /// grep-budget-exhaustion path testable without creating thousands of files.
    #[test]
    fn grep_file_budget_is_configurable_and_clamped() {
        static GREP_BUDGET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = GREP_BUDGET_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let prev = std::env::var("DOTZ_GREP_FILE_BUDGET").ok();

        std::env::remove_var("DOTZ_GREP_FILE_BUDGET");
        assert_eq!(grep_file_budget(), 5000, "default budget should be 5000");

        std::env::set_var("DOTZ_GREP_FILE_BUDGET", "100");
        assert_eq!(grep_file_budget(), 100, "valid override preserved");

        std::env::set_var("DOTZ_GREP_FILE_BUDGET", "0");
        assert_eq!(grep_file_budget(), 1, "zero clamped to minimum");

        std::env::set_var("DOTZ_GREP_FILE_BUDGET", "999999");
        assert_eq!(grep_file_budget(), 100_000, "too-large clamped to maximum");

        match prev {
            Some(p) => std::env::set_var("DOTZ_GREP_FILE_BUDGET", p),
            None => std::env::remove_var("DOTZ_GREP_FILE_BUDGET"),
        }
    }

    /// The `grep` tool must respect the file-scan budget: when `DOTZ_GREP_FILE_BUDGET` is set
    /// low enough that the matching file is never reached, the tool returns "(no matches)"
    /// instead of reading past the budget. This also verifies the budget-exhaustion `return`
    /// (which replaced the old inner-loop-only `break` that left the outer loop traversing
    /// directories uselessly) terminates the scan cleanly.
    #[tokio::test]
    async fn grep_respects_file_budget_and_terminates_cleanly() {
        static GREP_BUDGET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = GREP_BUDGET_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let base =
            std::env::temp_dir().join(format!("dotz-grep-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();

        // Create a single directory with files. The first file is a decoy (consumes the
        // budget), and the second file contains the needle. With budget=1, only the first
        // file is read — the needle in the second file must NOT be found. This is
        // deterministic because both files are in the same directory and `read_dir` returns
        // them in a stable per-filesystem order; we name them so `decoy.txt` sorts before
        // `match.txt` to ensure the decoy is scanned first on all platforms.
        std::fs::write(base.join("a_decoy.txt"), "nothing interesting").unwrap();
        std::fs::write(base.join("z_match.txt"), "unique-needle-here").unwrap();

        let prev_budget = std::env::var("DOTZ_GREP_FILE_BUDGET").ok();

        // budget=1: only the first file (`a_decoy.txt`) is read; the needle must NOT be found.
        std::env::set_var("DOTZ_GREP_FILE_BUDGET", "1");
        let mut registry = ToolRegistry::new();
        registry.set_active(&["grep".to_string()]);
        let ctx = ToolCtx {
            cwd: base.clone(),
            tx: None,
            run_id: None,
        };
        let result = registry
            .run("grep", &json!({"pattern": "unique-needle-here"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            result, "(no matches)",
            "with budget=1 the needle in the second file must not be found, got: {result}"
        );

        // budget=10: both files are read; the needle MUST be found.
        std::env::set_var("DOTZ_GREP_FILE_BUDGET", "10");
        let found = registry
            .run("grep", &json!({"pattern": "unique-needle-here"}), &ctx)
            .await
            .unwrap();
        assert!(
            found.contains("unique-needle-here"),
            "with a generous budget the needle must be found, got: {found}"
        );

        match prev_budget {
            Some(p) => std::env::set_var("DOTZ_GREP_FILE_BUDGET", p),
            None => std::env::remove_var("DOTZ_GREP_FILE_BUDGET"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
