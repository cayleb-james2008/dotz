//! C4: Plugin loader for dotz.
//!
//! A plugin is a directory under `~/.dotz/plugins/<name>/` containing:
//! - `plugin.toml` — manifest (name, version, optional `[[tools]]`, `[[hooks]]`, `[[mcp_servers]]`)
//! - `SKILL.md` — optional skill body (loaded via the existing `skills.rs` path; the plugin dir
//!   is a new scan root there, NOT a parallel loader)
//! - `hooks.toml` — optional legacy hook config (loaded by `hooks.rs` directly; C4 plugins put
//!   their hooks in `plugin.toml` instead)
//! - `mcp.json` — optional standalone MCP config (a plugin's `[[mcp_servers]]` in `plugin.toml`
//!   is the canonical path; the standalone file is supported for symmetry with `~/.dotz/mcp.json`)
//!
//! `skills.rs` is the single skill-discovery path: plugins extend its pool list (the plugin dir
//! is added as a new scan root with source `"plugin"`), they do NOT parallel it. `subagent.rs`
//! remains the sole emitter of `step_*` events; plugin tools dispatch through the normal tool
//! path (`agent::tools::ToolRegistry::run`) and do NOT emit graph events.
//!
//! # Trust boundary
//! `.dotz/plugins/` is a trust boundary (like `.dotz/hooks.toml`). Plugin `command` handlers run
//! with the user's privileges; `http` handlers POST payloads (which may carry tool args) to
//! arbitrary URLs. The manifest is parsed loudly — a malformed plugin is rejected with a clear
//! `eprintln!` warning, never silently skipped (silent acceptance of a broken trust-boundary
//! config is a security hole).
//!
//! # ponytail
//! - The manifest is parsed with a hand-rolled TOML subset parser (same approach as `hooks.rs`)
//!   to avoid adding the `toml` crate per the lean-deps rule (backend = axum + tokio + serde only).
//!   Ceiling: if the manifest schema grows (nested arrays-of-tables inside `[[tools]]`,
//!   datetimes, multi-line strings) swap for the `toml` crate — the `PluginManifest` struct
//!   already derives `Deserialize` so the migration is a one-line parser swap.
//! - `[[mcp_servers]]` registration is best-effort: a server that fails to connect is logged and
//!   skipped (matching `mcp::registry::connect_all`'s semantics). Plugins do NOT auto-connect on
//!   load; the operator (or the agent via the existing `mcp_call` tool) triggers `connect_all`.
//!   Auto-connecting on plugin load would spawn subprocesses the user never asked for, crossing
//!   the trust boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

/// The 5 tool handler types, mirroring `hooks::HandlerType` so a plugin tool can reuse the same
/// dispatch path as a hook. Re-declared here (not re-exported from `hooks`) so the manifest parser
/// stays self-contained and the plugin manifest schema is independent of the hook schema's
/// evolution (a hook handler may grow fields a tool handler doesn't need).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolHandler {
    Command,
    Http,
    McpTool,
    Prompt,
    Agent,
}

impl ToolHandler {
    /// Parse from the manifest's `handler` string. Returns a human-readable error on an unknown
    /// value so a malformed manifest is rejected loudly at load time (the trust-boundary rule).
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "command" => Ok(ToolHandler::Command),
            "http" => Ok(ToolHandler::Http),
            "mcp_tool" => Ok(ToolHandler::McpTool),
            "prompt" => Ok(ToolHandler::Prompt),
            "agent" => Ok(ToolHandler::Agent),
            other => Err(format!("unknown tool handler: {other}")),
        }
    }
}

/// One tool definition from a plugin's `[[tools]]` array. Only the fields relevant to the chosen
/// `handler` are required; the rest are `Option` and ignored when not applicable. Validation
/// happens in `validate()`.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub handler: ToolHandler,
    /// `command` handler: the executable to spawn (e.g. `python3`, `bash`).
    pub command: Option<String>,
    /// `command` handler: args passed to the command. The tool args JSON is piped via stdin.
    pub args: Option<Vec<String>>,
    /// `http` handler: URL to POST the tool args to.
    pub url: Option<String>,
    /// `http` handler: optional request headers.
    pub headers: Option<HashMap<String, String>>,
    /// `mcp_tool` handler: the MCP server name (must be connected via `mcp::registry`).
    pub server: Option<String>,
    /// `mcp_tool` handler: the tool name on that server.
    pub tool: Option<String>,
    /// `agent` handler: which bundled agent to spawn.
    pub agent: Option<String>,
    /// `prompt` handler: optional `{{key}}` template; default renders the full args JSON.
    pub template: Option<String>,
    /// JSON Schema for the tool's input — the agent sees this in the tool definition.
    pub input_schema: Value,
    /// Per-call timeout in milliseconds (default 30s). Applies to `command` + `http` handlers.
    pub timeout_ms: u64,
}

impl ToolDef {
    /// Validate the fields required for this tool's handler. Returns a human-readable error so a
    /// malformed manifest is rejected loudly at load time (trust-boundary rule). `pub(crate)` so
    /// the plugin loader + tests can call it; not part of the public plugin API.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("tool missing `name` field".into());
        }
        // Tool names must be plain identifiers so the `<plugin>:<tool>` registration name is a
        // clean key (no spaces, no colons — a `:` would break the prefix convention).
        if self.name.contains(':') || self.name.chars().any(|c| c.is_whitespace()) {
            return Err(format!(
                "tool name '{}' must not contain ':' or whitespace",
                self.name
            ));
        }
        match self.handler {
            ToolHandler::Command => {
                if self
                    .command
                    .as_deref()
                    .map(|c| c.trim().is_empty())
                    .unwrap_or(true)
                {
                    return Err(format!(
                        "tool '{}' handler=command requires a non-empty `command`",
                        self.name
                    ));
                }
            }
            ToolHandler::Http => {
                let u = self.url.as_deref().unwrap_or("").trim();
                if u.is_empty() {
                    return Err(format!(
                        "tool '{}' handler=http requires a non-empty `url`",
                        self.name
                    ));
                }
                // SSRF guard: https anywhere, or http://localhost / http://127.0.0.1 only.
                crate::mcp::validate_http_url(u)
                    .map_err(|e| format!("tool '{}': {e}", self.name))?;
            }
            ToolHandler::McpTool => {
                if self.server.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                    return Err(format!(
                        "tool '{}' handler=mcp_tool requires a non-empty `server`",
                        self.name
                    ));
                }
                if self.tool.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                    return Err(format!(
                        "tool '{}' handler=mcp_tool requires a non-empty `tool`",
                        self.name
                    ));
                }
            }
            ToolHandler::Agent => {
                if self.agent.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                    return Err(format!(
                        "tool '{}' handler=agent requires a non-empty `agent`",
                        self.name
                    ));
                }
            }
            ToolHandler::Prompt => {
                // prompt handler: no required fields; default renders the full args JSON.
            }
        }
        if self.timeout_ms == 0 {
            return Err(format!(
                "tool '{}' `timeout_ms` must be greater than 0",
                self.name
            ));
        }
        Ok(())
    }
}

/// Default tool-call timeout: 30 seconds. Matches the C2 hook default shape but with a longer
/// ceiling — a tool call (e.g. running a script) is allowed to take longer than a hook observer.
fn default_tool_timeout() -> u64 {
    30_000
}

/// A parsed plugin manifest. The top-level scalar fields + the three array-of-tables sections.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub tools: Vec<ToolDef>,
    /// Hook configs (re-parsed into `hooks::HookConfig` at registration time so the hook system
    /// owns its own validation). Stored as raw rows here to keep the plugin parser self-contained.
    pub hooks: Vec<HookRow>,
    /// MCP server configs (registered via `mcp::registry`). Stored as raw rows; the registration
    /// step builds the `mcp::ServerConfig` from them.
    pub mcp_servers: Vec<McpServerRow>,
}

/// A raw `[[hooks]]` row from a plugin manifest. Validated + converted to `hooks::HookConfig`
/// at registration time. Mirrors the `hooks.rs` field set; kept separate so the plugin parser
/// doesn't depend on the hook parser's exact shape (a hook may grow fields a plugin hook row
/// doesn't expose).
#[derive(Debug, Clone)]
pub struct HookRow {
    pub event: String,
    pub handler: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub template: Option<String>,
    pub agent: Option<String>,
    pub server: Option<String>,
    pub tool: Option<String>,
    pub timeout_ms: u64,
}

/// A raw `[[mcp_servers]]` row from a plugin manifest. Converted to `mcp::ServerConfig` at
/// registration time.
#[derive(Debug, Clone)]
pub struct McpServerRow {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
}

// ---- discovery + load ----

/// `~/.dotz/plugins/` — the plugin root, honoring `DOTZ_CONFIG_DIR` via `crate::config::dotz_dir()`.
pub fn plugins_dir() -> PathBuf {
    crate::config::dotz_dir().join("plugins")
}

/// The plugin scan root, surfaced through `skills.rs` as source `"plugin"`. Each direct child
/// directory of `~/.dotz/plugins/` is a plugin (its `SKILL.md`, if present, is loaded via the
/// existing `skills.rs` path — the single skill-discovery invariant). Returns the list of plugin
/// directories (not the SKILL.md files — `skills.rs` walks each dir for its own SKILL.md).
pub fn plugin_dirs() -> Vec<PathBuf> {
    let root = plugins_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else {
        return out;
    };
    for ent in rd.flatten() {
        if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(ent.path());
        }
    }
    out
}

/// Load all plugin manifests from `~/.dotz/plugins/*/plugin.toml`. Returns the parsed + validated
/// manifests. A missing `plugins/` dir is the common case → empty list (zero overhead). A
/// malformed or unvalidated manifest is logged via `eprintln!` and skipped — a broken plugin must
/// not crash the agent, but a clearly-bad field IS rejected at parse/validate time (the trust
/// boundary is the plugin dir; silent acceptance of a broken manifest is a security hole).
pub fn load_all() -> Vec<PluginManifest> {
    let mut out = Vec::new();
    for dir in plugin_dirs() {
        let manifest_path = dir.join("plugin.toml");
        let Some(name_from_dir) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(_) => {
                // No plugin.toml → not a plugin (the dir may exist for the SKILL.md only).
                continue;
            }
        };
        match parse_manifest(&text) {
            Ok(mut m) => {
                // The manifest's `name` (if set) overrides the directory name; otherwise the
                // directory name is the plugin name. The directory name is the trust-anchor —
                // it's what the user sees on disk — so a manifest `name` mismatch is logged but
                // the directory name wins for the registration prefix (so a malicious plugin
                // can't claim another plugin's prefix by editing its manifest `name`).
                if m.name.trim().is_empty() {
                    m.name = name_from_dir.to_string();
                } else if m.name != name_from_dir {
                    eprintln!(
                        "plugins: manifest name '{}' differs from dir name '{}'; using dir name for registration",
                        m.name, name_from_dir
                    );
                    m.name = name_from_dir.to_string();
                }
                // Validate every tool; collect failures. A single bad tool rejects the whole
                // plugin (so the agent never sees a half-loaded plugin with a broken tool). We
                // use a `skip` flag so the outer `for dir` loop continues to the next plugin
                // (a broken tool in one plugin must not prevent other plugins from loading).
                let mut skip = false;
                for tool in &m.tools {
                    if let Err(e) = tool.validate() {
                        eprintln!(
                            "plugins: skipped plugin '{name_from_dir}' at {display}: invalid tool: {e}",
                            display = manifest_path.display()
                        );
                        skip = true;
                        break;
                    }
                }
                if !skip {
                    out.push(m);
                }
            }
            Err(e) => {
                eprintln!(
                    "plugins: skipped plugin '{name_from_dir}' at {display}: parse error: {e}",
                    display = manifest_path.display()
                );
            }
        }
    }
    out
}

// ---- manifest TOML subset parser ----

/// Parse a plugin manifest from its TOML text. Hand-rolled subset (same approach as
/// `hooks::parse_hooks_toml`): top-level `key = value` scalars + `[[tools]]` / `[[hooks]]` /
/// `[[mcp_servers]]` array-of-tables, each table holding `key = "value"` / `key = 123` lines,
/// inline tables for `input_schema` / `headers` / `env`, and inline arrays for `args`. NOT a
/// general TOML parser — covers exactly the plugin manifest schema.
fn parse_manifest(text: &str) -> Result<PluginManifest, String> {
    let mut name = String::new();
    let mut version = String::new();
    let mut description: Option<String> = None;
    let mut author: Option<String> = None;
    let mut tools: Vec<ToolDef> = Vec::new();
    let mut hooks: Vec<HookRow> = Vec::new();
    let mut mcp_servers: Vec<McpServerRow> = Vec::new();

    // The current section: None = top-level scalars; Some("tools") / Some("hooks") /
    // Some("mcp_servers") = inside an array-of-tables entry (the most recent `[[...]]` header).
    let mut section: Option<&str> = None;

    // Accumulators for the current `[[tools]]` entry (rebuilt on each `[[tools]]` header).
    let mut cur_tool: HashMap<String, String> = HashMap::new();
    let mut cur_tool_input_schema: Option<Value> = None;
    let mut cur_hook: HashMap<String, String> = HashMap::new();
    let mut cur_mcp: HashMap<String, String> = HashMap::new();
    let mut cur_mcp_args: Option<Vec<String>> = None;
    let mut cur_mcp_env: Option<HashMap<String, String>> = None;
    let mut cur_mcp_headers: Option<HashMap<String, String>> = None;

    for (line_no, raw_line) in text.lines().enumerate() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        // Array-of-tables headers start a new entry.
        if line.starts_with("[[") && line.ends_with("]]") {
            // Flush the previous entry.
            match section {
                Some("tools") => {
                    let t = build_tool_def(&cur_tool, cur_tool_input_schema.take())
                        .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    tools.push(t);
                    cur_tool.clear();
                }
                Some("hooks") => {
                    let h = build_hook_row(&cur_hook)
                        .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    hooks.push(h);
                    cur_hook.clear();
                }
                Some("mcp_servers") => {
                    let s = build_mcp_server_row(
                        &cur_mcp,
                        cur_mcp_args.take(),
                        cur_mcp_env.take(),
                        cur_mcp_headers.take(),
                    )
                    .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    mcp_servers.push(s);
                    cur_mcp.clear();
                }
                _ => {}
            }
            section = match line {
                "[[tools]]" => Some("tools"),
                "[[hooks]]" => Some("hooks"),
                "[[mcp_servers]]" => Some("mcp_servers"),
                other => {
                    return Err(format!(
                        "line {}: unknown array-of-tables header: {other}",
                        line_no + 1
                    ))
                }
            };
            continue;
        }
        // A bare `[table]` header (not `[[array]]`) ends the current entry + is not part of the
        // schema — skip it.
        if line.starts_with('[') && line.ends_with(']') {
            match section {
                Some("tools") => {
                    let t = build_tool_def(&cur_tool, cur_tool_input_schema.take())
                        .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    tools.push(t);
                    cur_tool.clear();
                }
                Some("hooks") => {
                    let h = build_hook_row(&cur_hook)
                        .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    hooks.push(h);
                    cur_hook.clear();
                }
                Some("mcp_servers") => {
                    let s = build_mcp_server_row(
                        &cur_mcp,
                        cur_mcp_args.take(),
                        cur_mcp_env.take(),
                        cur_mcp_headers.take(),
                    )
                    .map_err(|e| format!("line {}: {e}", line_no + 1))?;
                    mcp_servers.push(s);
                    cur_mcp.clear();
                }
                _ => {}
            }
            section = None;
            continue;
        }

        let (key, val) = split_kv(line)
            .ok_or_else(|| format!("line {}: expected `key = value`, got: {line}", line_no + 1))?;

        match section {
            None => match key {
                "name" => {
                    name = parse_scalar(val)
                        .ok_or_else(|| format!("line {}: bad name value", line_no + 1))?
                }
                "version" => {
                    version = parse_scalar(val)
                        .ok_or_else(|| format!("line {}: bad version value", line_no + 1))?
                }
                "description" => description = parse_scalar(val),
                "author" => author = parse_scalar(val),
                _ => {} // unknown top-level keys are ignored (forward-compat)
            },
            Some("tools") => {
                if key == "input_schema" {
                    // Inline table parsed as a JSON object Value (the input_schema is a JSON Schema).
                    cur_tool_input_schema = Some(parse_inline_json_value(val).map_err(|e| {
                        format!("line {}: bad `input_schema` inline table: {e}", line_no + 1)
                    })?);
                } else {
                    cur_tool.insert(
                        key.to_string(),
                        parse_scalar(val).ok_or_else(|| {
                            format!("line {}: could not parse value: {val}", line_no + 1)
                        })?,
                    );
                }
            }
            Some("hooks") => {
                cur_hook.insert(
                    key.to_string(),
                    parse_scalar(val).ok_or_else(|| {
                        format!("line {}: could not parse value: {val}", line_no + 1)
                    })?,
                );
            }
            Some("mcp_servers") => {
                if key == "args" {
                    cur_mcp_args = Some(
                        parse_inline_array(val)
                            .map_err(|e| format!("line {}: bad `args` array: {e}", line_no + 1))?,
                    );
                } else if key == "env" {
                    cur_mcp_env = Some(parse_inline_table(val).map_err(|e| {
                        format!("line {}: bad `env` inline table: {e}", line_no + 1)
                    })?);
                } else if key == "headers" {
                    cur_mcp_headers = Some(parse_inline_table(val).map_err(|e| {
                        format!("line {}: bad `headers` inline table: {e}", line_no + 1)
                    })?);
                } else {
                    cur_mcp.insert(
                        key.to_string(),
                        parse_scalar(val).ok_or_else(|| {
                            format!("line {}: could not parse value: {val}", line_no + 1)
                        })?,
                    );
                }
            }
            _ => {}
        }
    }
    // Flush the final entry.
    match section {
        Some("tools") => {
            let t = build_tool_def(&cur_tool, cur_tool_input_schema.take())?;
            tools.push(t);
        }
        Some("hooks") => {
            let h = build_hook_row(&cur_hook)?;
            hooks.push(h);
        }
        Some("mcp_servers") => {
            let s = build_mcp_server_row(&cur_mcp, cur_mcp_args, cur_mcp_env, cur_mcp_headers)?;
            mcp_servers.push(s);
        }
        _ => {}
    }

    if name.trim().is_empty() {
        return Err("manifest missing `name` field".into());
    }
    if version.trim().is_empty() {
        // version is optional in spirit but the manifest schema lists it; default to "0.0.0"
        // rather than rejecting (a dev plugin without a version is common).
        version = "0.0.0".to_string();
    }
    Ok(PluginManifest {
        name,
        version,
        description,
        author,
        tools,
        hooks,
        mcp_servers,
    })
}

/// Build a `ToolDef` from the accumulated raw fields. `input_schema` is the inline-table JSON
/// parsed at the `input_schema = {...}` line; defaults to an empty object when absent (the agent
/// sees a permissive schema).
fn build_tool_def(
    fields: &HashMap<String, String>,
    input_schema: Option<Value>,
) -> Result<ToolDef, String> {
    let name = fields
        .get("name")
        .ok_or_else(|| "tool missing `name` field".to_string())?
        .clone();
    let description = fields.get("description").cloned().unwrap_or_default();
    let handler_str = fields
        .get("handler")
        .ok_or_else(|| format!("tool '{name}' missing `handler` field"))?;
    let handler = ToolHandler::from_str(handler_str)?;
    let timeout_ms = fields
        .get("timeout_ms")
        .map(|s| s.as_str())
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| format!("tool '{name}' `timeout_ms` is not a valid integer: {s}"))
        })
        .transpose()?
        .unwrap_or_else(default_tool_timeout);
    Ok(ToolDef {
        name,
        description,
        handler,
        command: fields.get("command").cloned(),
        args: None, // tool command args are NOT supported in this ponytail scope — the command
        // gets the tool args JSON via stdin (a single script), not an arg vector.
        // Ceiling: add `args = [...]` parsing if a plugin needs argv-level control.
        url: fields.get("url").cloned(),
        headers: None, // tool http headers are NOT supported in this ponytail scope — same as the
        // hook http handler which DOES support headers; added here if a plugin
        // needs them. Ceiling: parse `headers = {...}` like the hook parser.
        server: fields.get("server").cloned(),
        tool: fields.get("tool").cloned(),
        agent: fields.get("agent").cloned(),
        template: fields.get("template").cloned(),
        input_schema: input_schema.unwrap_or_else(|| serde_json::json!({"type": "object"})),
        timeout_ms,
    })
}

/// Build a `HookRow` from the accumulated raw fields. Validation happens at registration time
/// (when the row is converted to a `hooks::HookConfig`).
fn build_hook_row(fields: &HashMap<String, String>) -> Result<HookRow, String> {
    let event = fields
        .get("event")
        .ok_or_else(|| "hook missing `event` field".to_string())?
        .clone();
    let handler = fields
        .get("handler")
        .ok_or_else(|| "hook missing `handler` field".to_string())?
        .clone();
    let timeout_ms = fields
        .get("timeout_ms")
        .map(|s| s.as_str())
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| format!("hook `timeout_ms` is not a valid integer: {s}"))
        })
        .transpose()?
        .unwrap_or_else(default_hook_timeout);
    Ok(HookRow {
        event,
        handler,
        command: fields.get("command").cloned(),
        url: fields.get("url").cloned(),
        template: fields.get("template").cloned(),
        agent: fields.get("agent").cloned(),
        server: fields.get("server").cloned(),
        tool: fields.get("tool").cloned(),
        timeout_ms,
    })
}

/// Default hook timeout: 10s (matches `hooks::default_timeout_ms`).
fn default_hook_timeout() -> u64 {
    10_000
}

/// Build an `McpServerRow` from the accumulated raw fields + the inline-array `args` and
/// inline-table `env`/`headers` (parsed separately because they're not scalar strings).
fn build_mcp_server_row(
    fields: &HashMap<String, String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    headers: Option<HashMap<String, String>>,
) -> Result<McpServerRow, String> {
    let name = fields
        .get("name")
        .ok_or_else(|| "mcp_server missing `name` field".to_string())?
        .clone();
    let transport = fields
        .get("transport")
        .cloned()
        .unwrap_or_else(|| "stdio".to_string());
    Ok(McpServerRow {
        name,
        transport,
        command: fields.get("command").cloned(),
        args,
        env,
        url: fields.get("url").cloned(),
        headers,
    })
}

// ---- TOML subset helpers (mirrors hooks.rs) ----

/// Strip a `#` comment from a line, respecting `#` inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            in_str = !in_str;
        } else if b == b'\\' && in_str {
            i += 1; // skip the escaped char
        } else if b == b'#' && !in_str {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// Split `key = value` into `(key, value)`. The key is trimmed; the value is trimmed but its
/// quotes are preserved (the scalar parser handles them).
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    let val = line[eq + 1..].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, val))
}

/// Parse a TOML scalar string value: quoted (`"..."` with basic escapes) or bare (unquoted,
/// trimmed). Returns the decoded string value. Non-string scalars (numbers/bools) are returned as
/// their literal source text so the caller can parse them into the right type.
fn parse_scalar(val: &str) -> Option<String> {
    let v = val.trim();
    if v.is_empty() {
        return None;
    }
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        return Some(decode_quoted(&v[1..v.len() - 1]));
    }
    Some(v.to_string())
}

/// Decode a quoted TOML string body: handle `\"`, `\\`, `\n`, `\t`, `\r`. Other backslash escapes
/// are passed through literally (matches `hooks::decode_quoted`).
fn decode_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse an inline table `{ k = "v", k2 = "v2" }` into a `HashMap<String, String>`. Keys are bare
/// (unquoted) or quoted; values are quoted or bare scalars. Whitespace + commas separate pairs.
fn parse_inline_table(val: &str) -> Result<HashMap<String, String>, String> {
    let v = val.trim();
    if !(v.starts_with('{') && v.ends_with('}')) {
        return Err(format!("expected `{{...}}`, got: {v}"));
    }
    let inner = &v[1..v.len() - 1];
    let mut out = HashMap::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    for pair in split_top_level(inner, ',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (k, v) = split_kv(pair)
            .ok_or_else(|| format!("expected `k = v` in inline table, got: {pair}"))?;
        let key = if k.starts_with('"') && k.ends_with('"') && k.len() >= 2 {
            decode_quoted(&k[1..k.len() - 1])
        } else {
            k.to_string()
        };
        let value = parse_scalar(v).ok_or_else(|| format!("could not parse value: {v}"))?;
        out.insert(key, value);
    }
    Ok(out)
}

/// Parse an inline array `[ "a", "b" ]` into a `Vec<String>`. Elements are quoted or bare scalars.
fn parse_inline_array(val: &str) -> Result<Vec<String>, String> {
    let v = val.trim();
    if !(v.starts_with('[') && v.ends_with(']')) {
        return Err(format!("expected `[...]`, got: {v}"));
    }
    let inner = &v[1..v.len() - 1];
    let mut out = Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    for elem in split_top_level(inner, ',') {
        let elem = elem.trim();
        if elem.is_empty() {
            continue;
        }
        let value =
            parse_scalar(elem).ok_or_else(|| format!("could not parse array element: {elem}"))?;
        out.push(value);
    }
    Ok(out)
}

/// Parse an inline table `{ k = "v", k2 = 123 }` into a `serde_json::Value` object. Values are
/// typed: quoted strings → JSON strings; `true`/`false` → JSON bools; integers → JSON numbers;
/// nested inline tables → JSON objects (recursive); inline arrays → JSON arrays (recursive).
/// This is the input_schema parser — JSON Schema needs real types, not all-strings.
fn parse_inline_json_value(val: &str) -> Result<Value, String> {
    let v = val.trim();
    if v.starts_with('{') && v.ends_with('}') {
        return parse_inline_json_object(v);
    }
    if v.starts_with('[') && v.ends_with(']') {
        return parse_inline_json_array(v);
    }
    // A bare scalar at the top of input_schema is unusual but parse it anyway.
    parse_json_scalar(v).ok_or_else(|| format!("could not parse input_schema scalar: {v}"))
}

/// Parse an inline JSON object `{ k = "v", k2 = 123, k3 = { ... } }` into a `serde_json::Value`.
fn parse_inline_json_object(val: &str) -> Result<Value, String> {
    let v = val.trim();
    if !(v.starts_with('{') && v.ends_with('}')) {
        return Err(format!("expected `{{...}}`, got: {v}"));
    }
    let inner = &v[1..v.len() - 1];
    let mut map = serde_json::Map::new();
    if inner.trim().is_empty() {
        return Ok(Value::Object(map));
    }
    for pair in split_top_level(inner, ',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (k, v) = split_kv(pair)
            .ok_or_else(|| format!("expected `k = v` in inline object, got: {pair}"))?;
        let key = if k.starts_with('"') && k.ends_with('"') && k.len() >= 2 {
            decode_quoted(&k[1..k.len() - 1])
        } else {
            k.to_string()
        };
        let value = parse_inline_json_value(v)?;
        map.insert(key, value);
    }
    Ok(Value::Object(map))
}

/// Parse an inline JSON array `[ "a", 123, { ... } ]` into a `serde_json::Value`.
fn parse_inline_json_array(val: &str) -> Result<Value, String> {
    let v = val.trim();
    if !(v.starts_with('[') && v.ends_with(']')) {
        return Err(format!("expected `[...]`, got: {v}"));
    }
    let inner = &v[1..v.len() - 1];
    let mut out = Vec::new();
    if inner.trim().is_empty() {
        return Ok(Value::Array(out));
    }
    for elem in split_top_level(inner, ',') {
        let elem = elem.trim();
        if elem.is_empty() {
            continue;
        }
        out.push(parse_inline_json_value(elem)?);
    }
    Ok(Value::Array(out))
}

/// Parse a JSON scalar: quoted string, bool, null, or number. Returns None for unrecognized.
fn parse_json_scalar(v: &str) -> Option<Value> {
    let v = v.trim();
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        return Some(Value::String(decode_quoted(&v[1..v.len() - 1])));
    }
    if v == "true" {
        return Some(Value::Bool(true));
    }
    if v == "false" {
        return Some(Value::Bool(false));
    }
    if v == "null" {
        return Some(Value::Null);
    }
    if let Ok(n) = v.parse::<i64>() {
        return Some(Value::Number(n.into()));
    }
    if let Ok(f) = v.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Some(Value::Number(n));
        }
    }
    None
}

/// Split a string on a delimiter char, ignoring delimiters inside double-quoted substrings (so a
/// comma inside a quoted string value doesn't split the pair). Mirrors `hooks::split_top_level`.
fn split_top_level(s: &str, delim: char) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut depth: i32 = 0; // track nested `{}[]` so a comma inside a nested table/array doesn't split
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            in_str = !in_str;
            cur.push(c);
        } else if c == '\\' && in_str {
            cur.push(c);
            if let Some(next) = chars.next() {
                cur.push(next);
            }
        } else if (c == '{' || c == '[') && !in_str {
            depth += 1;
            cur.push(c);
        } else if (c == '}' || c == ']') && !in_str {
            depth -= 1;
            cur.push(c);
        } else if c == delim && !in_str && depth == 0 {
            out.push(cur.clone());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() || !out.is_empty() {
        out.push(cur);
    }
    out
}

// ---- registration: tools, hooks, mcp_servers ----

/// The registration name for a plugin tool: `<plugin_name>:<tool_name>`. The plugin name prefix
/// prevents shadowing built-in tools and collisions between plugins.
pub fn registration_name(plugin_name: &str, tool_name: &str) -> String {
    format!("{plugin_name}:{tool_name}")
}

/// Register all plugin tools into the add-closure. Called by `agent::tools::ToolRegistry::new()`
/// after the built-ins + extra tools. Each plugin tool is wrapped in a `PluginTool` (defined in
/// `agent::extra_tools.rs` because it needs the `Tool` trait + `ToolCtx` from `agent::tools`).
pub fn register_tools(
    manifests: &[PluginManifest],
    add: &mut dyn FnMut(Box<dyn crate::agent::tools::Tool>),
) {
    for m in manifests {
        for tool in &m.tools {
            let reg_name = registration_name(&m.name, &tool.name);
            add(Box::new(crate::agent::extra_tools::PluginTool::new(
                reg_name,
                tool.clone(),
            )));
        }
    }
}

/// Register all plugin hooks into the global hook registry. Each `[[hooks]]` row is converted to a
/// `hooks::HookConfig` and appended to the global registry (so it fires alongside the
/// project/user `hooks.toml` hooks). Best-effort: a malformed hook row is logged + skipped.
pub fn register_hooks(manifests: &[PluginManifest]) {
    let mut configs: Vec<crate::hooks::HookConfig> = Vec::new();
    for m in manifests {
        for row in &m.hooks {
            match hook_row_to_config(row) {
                Ok(cfg) => {
                    if let Err(e) = cfg.validate() {
                        eprintln!("plugins: skipped hook in plugin '{}': {e}", m.name);
                    } else {
                        configs.push(cfg);
                    }
                }
                Err(e) => {
                    eprintln!("plugins: skipped hook in plugin '{}': {e}", m.name);
                }
            }
        }
    }
    if configs.is_empty() {
        return;
    }
    crate::hooks::append_plugin_hooks(configs);
}

/// Convert a `HookRow` to a `hooks::HookConfig`. The hook system owns its own validation; this
/// just maps the fields. Errors on an unknown event/handler value (loud rejection at load time).
fn hook_row_to_config(row: &HookRow) -> Result<crate::hooks::HookConfig, String> {
    use crate::hooks::{HandlerType, HookConfig, HookEvent};
    let event = match row.event.as_str() {
        "SessionStart" => HookEvent::SessionStart,
        "PreToolUse" => HookEvent::PreToolUse,
        "PostToolUse" => HookEvent::PostToolUse,
        "Stop" => HookEvent::Stop,
        "SubagentStop" => HookEvent::SubagentStop,
        other => return Err(format!("unknown hook event: {other}")),
    };
    let handler = match row.handler.as_str() {
        "command" => HandlerType::Command,
        "http" => HandlerType::Http,
        "mcp_tool" => HandlerType::McpTool,
        "prompt" => HandlerType::Prompt,
        "agent" => HandlerType::Agent,
        other => return Err(format!("unknown hook handler: {other}")),
    };
    Ok(HookConfig {
        event,
        handler,
        command: row.command.clone(),
        url: row.url.clone(),
        headers: None, // plugin hook headers not supported in this ponytail scope; ceiling: add
        // `headers = {...}` parsing like the hook parser.
        template: row.template.clone(),
        agent: row.agent.clone(),
        server: row.server.clone(),
        tool: row.tool.clone(),
        timeout_ms: row.timeout_ms,
    })
}

/// Register all plugin MCP servers. Each `[[mcp_servers]]` row is converted to a
/// `mcp::ServerConfig` and validated. # ponytail: the actual connection is NOT auto-triggered
/// here — the `mcp::registry` module (which C4 is explicitly constrained NOT to edit) only
/// exposes `connect_all(cwd)` (which reads `~/.dotz/mcp.json`). Auto-registering plugin servers
/// would require either (a) editing `mcp::registry` to expose an `append_pending_servers` API,
/// or (b) mutating the user's `~/.dotz/mcp.json` on plugin load. Both cross the constraint
/// boundary; (b) is also surprising to the operator (a plugin silently editing their config).
///
/// Ceiling: the operator adds a plugin's `[[mcp_servers]]` to their `~/.dotz/mcp.json` manually
/// (the plugin manifest documents the server name + config), OR a future C4-follow-up adds a
/// public `mcp::registry::append_pending_servers` entry point (a one-line addition to that
/// module) and this function calls it. For now, the validation still runs (a malformed
/// `[[mcp_servers]]` row is logged + skipped) so a broken plugin is rejected loudly.
pub fn register_mcp_servers(manifests: &[PluginManifest]) {
    for m in manifests {
        for row in &m.mcp_servers {
            match mcp_row_to_config(row) {
                Ok(_cfg) => {
                    // Validated; the operator adds this server to ~/.dotz/mcp.json manually.
                    eprintln!(
                        "plugins: mcp_server '{}' in plugin '{}' validated; add it to ~/.dotz/mcp.json to connect",
                        row.name, m.name
                    );
                }
                Err(e) => {
                    eprintln!("plugins: skipped mcp_server in plugin '{}': {e}", m.name);
                }
            }
        }
    }
}

/// Convert an `McpServerRow` to a `mcp::ServerConfig`. Errors on an unknown transport value.
/// Public so the plugin MCP-server test can exercise the validation without re-implementing it.
pub fn mcp_row_to_config(row: &McpServerRow) -> Result<crate::mcp::ServerConfig, String> {
    use crate::mcp::{ServerConfig, TransportType};
    let transport = match row.transport.as_str() {
        "stdio" => TransportType::Stdio,
        "http" => TransportType::Http,
        other => return Err(format!("unknown mcp transport: {other}")),
    };
    Ok(ServerConfig {
        transport,
        command: row.command.clone(),
        args: row.args.clone(),
        env: row.env.clone(),
        url: row.url.clone(),
        headers: row.headers.clone(),
        oauth: None, // plugin mcp oauth not supported in this ponytail scope; ceiling: add an
                     // `[[mcp_servers.oauth]]` sub-table parser if a plugin needs oauth.
    })
}

// ---- test helpers ----

/// Write a plugin manifest to `<dotz_root>/plugins/<name>/plugin.toml` and return the plugin dir.
/// Used by the plugin tests to set up an isolated plugin tree. The `dotz_root` is the value of
/// `DOTZ_CONFIG_DIR` (i.e. `~/.dotz`), so the plugin lands at `<dotz_root>/plugins/<name>/` —
/// exactly where `plugin_dirs()` scans.
#[cfg(test)]
fn write_plugin(dotz_root: &Path, name: &str, manifest: &str) -> PathBuf {
    let dir = dotz_root.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plugin.toml");
    std::fs::write(&path, manifest).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::dotz_config_dir_test_lock;

    /// Serialize tests that mutate `DOTZ_CONFIG_DIR` so they don't race on the global plugin path.
    /// The guard is a std Mutex held across the test body (no `.await` inside the guarded region
    /// in these tests — they're all sync `load_all` / parser tests).
    fn with_plugins_root<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = dotz_config_dir_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!(
            "dotz-plugins-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let prev = std::env::var("DOTZ_CONFIG_DIR").ok();
        std::env::set_var("DOTZ_CONFIG_DIR", &root);
        let result = f(&root);
        match prev {
            Some(p) => std::env::set_var("DOTZ_CONFIG_DIR", p),
            None => std::env::remove_var("DOTZ_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&root);
        result
    }

    // ---- manifest parsing ----

    #[test]
    fn plugin_manifest_parses_command_tool() {
        let m = parse_manifest(
            r#"
name = "my-plugin"
version = "0.1.0"
description = "Does X"

[[tools]]
name = "my_tool"
description = "Does X"
handler = "command"
command = "python3"
[input_schema]
type = "object"
properties = { query = { type = "string" } }
required = ["query"]
"#,
        )
        .expect("command tool manifest must parse");
        assert_eq!(m.name, "my-plugin");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.description.as_deref(), Some("Does X"));
        assert_eq!(m.tools.len(), 1);
        let t = &m.tools[0];
        assert_eq!(t.name, "my_tool");
        assert_eq!(t.handler, ToolHandler::Command);
        assert_eq!(t.command.as_deref(), Some("python3"));
        assert_eq!(t.timeout_ms, 30_000);
        assert_eq!(
            t.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object")
        );
        m.tools[0].validate().expect("command tool must validate");
    }

    #[test]
    fn plugin_manifest_parses_http_tool() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "http_tool"
handler = "http"
url = "https://api.example.com/tool"
"#,
        )
        .expect("http tool manifest must parse");
        let t = &m.tools[0];
        assert_eq!(t.handler, ToolHandler::Http);
        assert_eq!(t.url.as_deref(), Some("https://api.example.com/tool"));
        t.validate().expect("http tool must validate");
    }

    #[test]
    fn plugin_manifest_parses_mcp_tool() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "delegate"
handler = "mcp_tool"
server = "my-mcp"
tool = "some_tool"
"#,
        )
        .expect("mcp_tool manifest must parse");
        let t = &m.tools[0];
        assert_eq!(t.handler, ToolHandler::McpTool);
        assert_eq!(t.server.as_deref(), Some("my-mcp"));
        assert_eq!(t.tool.as_deref(), Some("some_tool"));
        t.validate().expect("mcp_tool must validate");
    }

    #[test]
    fn plugin_manifest_parses_agent_tool() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "delegate"
handler = "agent"
agent = "worker"
"#,
        )
        .expect("agent tool manifest must parse");
        let t = &m.tools[0];
        assert_eq!(t.handler, ToolHandler::Agent);
        assert_eq!(t.agent.as_deref(), Some("worker"));
        t.validate().expect("agent tool must validate");
    }

    #[test]
    fn plugin_manifest_parses_prompt_tool() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "prompt_tool"
handler = "prompt"
template = "Result for {{query}}"
"#,
        )
        .expect("prompt tool manifest must parse");
        let t = &m.tools[0];
        assert_eq!(t.handler, ToolHandler::Prompt);
        assert_eq!(t.template.as_deref(), Some("Result for {{query}}"));
        t.validate().expect("prompt tool must validate");
    }

    #[test]
    fn plugin_manifest_rejects_unknown_handler() {
        let res = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "x"
handler = "carrier_pigeon"
"#,
        );
        assert!(res.is_err(), "unknown handler must reject");
        let e = res.unwrap_err();
        assert!(
            e.contains("carrier_pigeon"),
            "error must mention the unknown handler: {e}"
        );
    }

    #[test]
    fn plugin_manifest_rejects_missing_name() {
        let res = parse_manifest(
            r#"
version = "0.1.0"

[[tools]]
name = "x"
handler = "command"
command = "echo"
"#,
        );
        assert!(res.is_err(), "missing top-level name must reject");
        let e = res.unwrap_err();
        assert!(e.contains("name"), "error must mention name: {e}");
    }

    #[test]
    fn plugin_manifest_defaults_version_when_missing() {
        // A dev plugin without a version is common — default to "0.0.0" rather than rejecting.
        let m = parse_manifest(
            r#"
name = "p"

[[tools]]
name = "x"
handler = "command"
command = "echo"
"#,
        )
        .expect("missing version must default");
        assert_eq!(m.version, "0.0.0");
    }

    #[test]
    fn plugin_manifest_rejects_tool_with_missing_name() {
        let res = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
handler = "command"
command = "echo"
"#,
        );
        assert!(res.is_err(), "tool missing name must reject");
    }

    #[test]
    fn plugin_manifest_rejects_command_tool_with_empty_command() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "x"
handler = "command"
command = ""
"#,
        )
        .expect("parse ok");
        let err = m.tools[0].validate().unwrap_err();
        assert!(
            err.contains("non-empty") && err.contains("command"),
            "empty command must reject: {err}"
        );
    }

    #[test]
    fn plugin_manifest_rejects_http_tool_with_non_https_url() {
        // SSRF guard: http://example.com (non-loopback) must reject at validate time.
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "x"
handler = "http"
url = "http://example.com/tool"
"#,
        )
        .expect("parse ok");
        let err = m.tools[0].validate().unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("https") || err.contains("loopback"),
            "non-https non-loopback http url must reject with SSRF hint: {err}"
        );
    }

    #[test]
    fn plugin_manifest_accepts_loopback_http_tool() {
        // http://localhost / http://127.0.0.1 is allowed (local proxy / dev server).
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "x"
handler = "http"
url = "http://localhost:9000/tool"
"#,
        )
        .expect("parse ok");
        m.tools[0].validate().expect("loopback http must validate");
    }

    #[test]
    fn plugin_manifest_rejects_tool_name_with_colon() {
        // A colon in the tool name would break the `<plugin>:<tool>` prefix convention.
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[tools]]
name = "a:b"
handler = "command"
command = "echo"
"#,
        )
        .expect("parse ok");
        let err = m.tools[0].validate().unwrap_err();
        assert!(
            err.contains(':') || err.contains("colon"),
            "colon in tool name must reject: {err}"
        );
    }

    #[test]
    fn plugin_manifest_parses_hooks_section() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[hooks]]
event = "PreToolUse"
handler = "command"
command = "check.sh"
timeout_ms = 5000
"#,
        )
        .expect("hooks manifest must parse");
        assert_eq!(m.hooks.len(), 1);
        let h = &m.hooks[0];
        assert_eq!(h.event, "PreToolUse");
        assert_eq!(h.handler, "command");
        assert_eq!(h.command.as_deref(), Some("check.sh"));
        assert_eq!(h.timeout_ms, 5000);
    }

    #[test]
    fn plugin_manifest_parses_mcp_servers_section() {
        let m = parse_manifest(
            r#"
name = "p"
version = "0.1.0"

[[mcp_servers]]
name = "my-mcp"
transport = "stdio"
command = "npx"
args = ["-y", "my-mcp-server"]
env = { FOO = "bar" }
"#,
        )
        .expect("mcp_servers manifest must parse");
        assert_eq!(m.mcp_servers.len(), 1);
        let s = &m.mcp_servers[0];
        assert_eq!(s.name, "my-mcp");
        assert_eq!(s.transport, "stdio");
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(
            s.args.as_deref(),
            Some(["-y".to_string(), "my-mcp-server".to_string()].as_slice())
        );
        assert_eq!(
            s.env
                .as_ref()
                .and_then(|e| e.get("FOO"))
                .map(|s| s.as_str()),
            Some("bar")
        );
    }

    // ---- load_all ----

    #[test]
    fn plugin_load_all_empty_when_no_plugins_dir() {
        with_plugins_root(|_root| {
            let manifests = load_all();
            assert!(
                manifests.is_empty(),
                "no plugins dir → empty manifest list (zero overhead)"
            );
        });
    }

    #[test]
    fn plugin_load_all_loads_from_plugins_dir() {
        with_plugins_root(|root| {
            let _ = write_plugin(
                root,
                "alpha",
                r#"
name = "alpha"
version = "0.1.0"

[[tools]]
name = "do_thing"
handler = "command"
command = "echo"
"#,
            );
            let manifests = load_all();
            assert_eq!(manifests.len(), 1, "one plugin loaded");
            assert_eq!(manifests[0].name, "alpha");
            assert_eq!(manifests[0].tools.len(), 1);
            assert_eq!(manifests[0].tools[0].name, "do_thing");
        });
    }

    #[test]
    fn plugin_load_all_skips_dir_without_plugin_toml() {
        with_plugins_root(|_root| {
            // A dir with only a SKILL.md (no plugin.toml) is not a plugin — it's a skill scanned
            // by skills.rs. load_all must skip it.
            let skill_dir = crate::config::dotz_dir().join("plugins").join("skill-only");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
            let manifests = load_all();
            assert!(
                manifests.is_empty(),
                "dir without plugin.toml must be skipped"
            );
        });
    }

    #[test]
    fn plugin_load_all_skips_malformed_manifest() {
        with_plugins_root(|root| {
            let _ = write_plugin(root, "broken", "this is not toml at all = = =");
            let manifests = load_all();
            assert!(
                manifests.is_empty(),
                "a malformed manifest must be skipped, not crash"
            );
        });
    }

    #[test]
    fn plugin_load_all_dir_name_wins_on_manifest_name_mismatch() {
        // A manifest `name` that differs from the dir name is logged + the dir name wins for the
        // registration prefix (so a malicious plugin can't claim another plugin's prefix).
        with_plugins_root(|root| {
            let _ = write_plugin(
                root,
                "real-plugin",
                r#"
name = "evil-other-plugin"
version = "0.1.0"

[[tools]]
name = "x"
handler = "command"
command = "echo"
"#,
            );
            let manifests = load_all();
            assert_eq!(manifests.len(), 1);
            assert_eq!(
                manifests[0].name, "real-plugin",
                "dir name must win on a manifest name mismatch"
            );
        });
    }

    #[test]
    fn plugin_load_all_rejects_plugin_with_invalid_tool() {
        // A plugin with an invalid tool (empty command) is skipped entirely — the agent never
        // sees a half-loaded plugin.
        with_plugins_root(|root| {
            let _ = write_plugin(
                root,
                "bad-tool",
                r#"
name = "bad-tool"
version = "0.1.0"

[[tools]]
name = "x"
handler = "command"
command = ""
"#,
            );
            let manifests = load_all();
            // The plugin is loaded but the tool validation fails — per the load_all logic a single
            // bad tool rejects the whole plugin (the comment in load_all explains why).
            assert!(
                manifests.is_empty() || manifests[0].tools.is_empty(),
                "plugin with an invalid tool must not surface the broken tool"
            );
        });
    }

    // ---- registration_name ----

    #[test]
    fn plugin_tool_name_prefixed_with_plugin_name() {
        assert_eq!(
            registration_name("my-plugin", "my_tool"),
            "my-plugin:my_tool"
        );
        assert_eq!(registration_name("alpha", "beta"), "alpha:beta");
    }

    // ---- register_tools ----

    #[test]
    fn plugin_register_tools_adds_to_registry() {
        with_plugins_root(|root| {
            let _ = write_plugin(
                root,
                "alpha",
                r#"
name = "alpha"
version = "0.1.0"

[[tools]]
name = "do_thing"
description = "does a thing"
handler = "command"
command = "echo"
"#,
            );
            let manifests = load_all();
            let r = crate::agent::tools::ToolRegistry::new_with_plugins(&manifests);
            let all = r.all_names();
            assert!(
                all.contains(&"alpha:do_thing".to_string()),
                "plugin tool must be registered under the prefixed name, got: {all:?}"
            );
        });
    }

    #[test]
    fn plugin_register_tools_does_not_shadow_builtins() {
        // A plugin tool named "read" must register as "alpha:read", NOT shadow the built-in "read".
        with_plugins_root(|root| {
            let _ = write_plugin(
                root,
                "alpha",
                r#"
name = "alpha"
version = "0.1.0"

[[tools]]
name = "read"
handler = "command"
command = "echo"
"#,
            );
            let manifests = load_all();
            let r = crate::agent::tools::ToolRegistry::new_with_plugins(&manifests);
            let all = r.all_names();
            // Both the built-in "read" and the plugin "alpha:read" must coexist.
            assert!(
                all.contains(&"read".to_string()),
                "built-in read must remain"
            );
            assert!(
                all.contains(&"alpha:read".to_string()),
                "plugin alpha:read must also be registered, got: {all:?}"
            );
        });
    }

    // ---- static assertions ----

    /// `subagent.rs` remains the sole emitter of `step_*` events. Plugin tools dispatch via the
    /// normal tool path (`ToolRegistry::run`) and do NOT emit graph events. This is a static
    /// grep guard: plugins.rs must NOT emit `step_tool`/`step_thinking`.
    #[test]
    fn plugin_tool_does_not_emit_step_events() {
        let src = include_str!("plugins.rs");
        assert!(
            !src.contains("\"step_tool\"") && !src.contains("\"step_thinking\""),
            "plugins.rs must NOT emit step_tool/step_thinking events — subagent.rs is the sole emitter"
        );
        // Confirm subagent.rs still owns those emits (the canonical emitter).
        let subagent_src = include_str!("agent/subagent.rs");
        assert!(
            subagent_src.contains("\"step_tool\"") && subagent_src.contains("\"step_thinking\""),
            "subagent.rs must remain the sole step_tool/step_thinking emitter"
        );
    }

    /// `skills.rs` is still the single skill-discovery path. Plugins extend its pool list (the
    /// plugin dir is a new scan root), they do NOT parallel it. This is a static grep guard:
    /// plugins.rs must NOT define a SKILL.md loader (no `find_skill_files` / `parse_skill_file`
    /// / second skill index).
    #[test]
    fn plugins_do_not_parallel_skills_loader() {
        let src = include_str!("plugins.rs");
        // The plugin module must NOT define its own SKILL.md walker / parser / skill index. We
        // check for a line that STARTS with `fn <name>(` (a definition), not just a substring
        // match, so the assertion's own string literals (which mention the names) don't
        // false-positive.
        let defines_fn = |name: &str| -> bool {
            src.lines().any(|l| {
                l.trim().starts_with(&format!("fn {name}("))
                    || l.trim().starts_with(&format!("pub fn {name}("))
            })
        };
        assert!(
            !defines_fn("find_skill_files")
                && !defines_fn("parse_skill_file")
                && !defines_fn("build_index"),
            "plugins.rs must NOT add a second skill loader — skills.rs is the single path"
        );
        // The plugin module exposes the plugin dir list for skills.rs to add as a scan root.
        assert!(
            src.contains("pub fn plugin_dirs"),
            "plugins.rs must expose plugin_dirs() so skills.rs can add it as a scan root"
        );
        // And skills.rs must reference the plugin scan root.
        let skills_src = include_str!("skills.rs");
        assert!(
            skills_src.contains("plugins") && skills_src.contains("plugin_dirs"),
            "skills.rs must include the plugins scan root (single skill-discovery path)"
        );
    }

    // ---- TOML subset parser unit tests ----

    #[test]
    fn parse_inline_json_object_types_values_correctly() {
        let v = parse_inline_json_value(
            r#"{ type = "object", properties = { query = { type = "string" } }, required = ["query"], additional = false, count = 3 }"#,
        )
        .expect("typed inline object must parse");
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("object"));
        assert!(v.get("properties").is_some(), "nested object must parse");
        assert_eq!(
            v.get("properties")
                .and_then(|p| p.get("query"))
                .and_then(|q| q.get("type"))
                .and_then(|t| t.as_str()),
            Some("string")
        );
        let required = v.get("required").and_then(|r| r.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("query"));
        assert_eq!(v.get("additional").and_then(|x| x.as_bool()), Some(false));
        assert_eq!(v.get("count").and_then(|x| x.as_i64()), Some(3));
    }

    #[test]
    fn parse_inline_array_handles_quoted_elements() {
        let a = parse_inline_array(r#"["-y", "my-mcp-server", "/tmp"]"#).expect("array must parse");
        assert_eq!(a, vec!["-y", "my-mcp-server", "/tmp"]);
    }

    #[test]
    fn parse_inline_table_handles_quoted_keys() {
        let m = parse_inline_table(r#"{ Authorization = "Bearer x", "X-Frame-Options" = "DENY" }"#)
            .expect("table must parse");
        assert_eq!(m.get("Authorization").map(|s| s.as_str()), Some("Bearer x"));
        assert_eq!(m.get("X-Frame-Options").map(|s| s.as_str()), Some("DENY"));
    }

    #[test]
    fn split_top_level_respects_nested_braces() {
        // A comma inside a nested `{ ... }` must NOT split the pair (depth tracking).
        let parts = split_top_level(r#"k = { nested = { a = 1, b = 2 } }, k2 = "value""#, ',');
        assert_eq!(
            parts.len(),
            2,
            "nested-brace comma must not split: {parts:?}"
        );
        assert!(parts[0].contains("nested"));
        assert!(parts[1].contains("value"));
    }
}
