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

/// Context a tool executes in (the session's cwd, for relative-path resolution + memory scoping).
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// Session WS broadcast sender (for human_gate to emit a gate frame). None in subagent contexts.
    pub tx: Option<tokio::sync::broadcast::Sender<serde_json::Value>>,
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

// ---- read ----
struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a file's contents (UTF-8). Returns the full text."
    }
    fn parameters(&self) -> Value {
        obj_param(
            json!({ "file_path": { "type": "string", "description": "Path to the file" } }),
            &["file_path"],
        )
    }
    async fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let p = str_arg(args, "file_path").ok_or("file_path is required")?;
        std::fs::read_to_string(ctx.resolve(p)).map_err(|e| format!("read {p}: {e}"))
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
        let path = ctx.resolve(p);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, content).map_err(|e| format!("write {p}: {e}"))?;
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
        let path = ctx.resolve(p);
        let content = std::fs::read_to_string(&path).map_err(|e| format!("read {p}: {e}"))?;
        let count = content.matches(old).count();
        if count == 0 {
            return Err(format!("old_string not found in {p}"));
        }
        if count > 1 {
            return Err(format!("old_string is not unique in {p} ({count} matches)"));
        }
        let updated = content.replacen(old, new, 1);
        std::fs::write(&path, updated).map_err(|e| format!("write {p}: {e}"))?;
        Ok(format!("edited {p}"))
    }
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
        // Block on the OS process off the async runtime worker.
        let out = tokio::task::spawn_blocking(move || {
            // Windows: cmd /C; otherwise sh -c. (The host here is Windows; keep both for portability.)
            let mut c = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.arg("/C").arg(&cmd);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(&cmd);
                c
            };
            c.current_dir(&cwd).output()
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
        .map_err(|e| format!("exec: {e}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            s.push_str("\n[stderr]\n");
            s.push_str(&err);
        }
        if !out.status.success() {
            s.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
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
        let dir = ctx.resolve(p);
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
        let root = ctx.resolve(str_arg(args, "path").unwrap_or("."));
        let cwd = ctx.cwd.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            let mut stack = vec![root];
            let mut budget = 5000usize; // cap files scanned
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
                            break;
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
        let root = ctx.resolve(str_arg(args, "path").unwrap_or("."));
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

    /// Run a tool by name. Unknown/inactive tool → Err.
    pub async fn run(&self, name: &str, args: &Value, ctx: &ToolCtx) -> Result<String, String> {
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
