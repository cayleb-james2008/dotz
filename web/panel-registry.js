/* Pure panel metadata shared by state, graph, and the panel wirer modules.
 * Keep this module free of UI/wirer imports so ESM initialization does not cycle through panels.js.
 */
const PANEL_DEFINITIONS = [
  { name: "chat", icon: "▓", label: "CHAT" },
  { name: "graph", icon: "◐", label: "WORKFLOW GRAPH", color: "var(--cyan)", toolMap: [{ exact: "subagent" }] },
  { name: "brain", icon: "◆", label: "AGENT BRAIN", color: "var(--mauve)", toolMap: [{ exact: ["rsi_baseline", "rsi_compare"] }] },
  { name: "browser", icon: "▣", label: "BROWSER", color: "var(--cyan)", toolMap: [{ prefix: "browser_" }] },
  { name: "memory", icon: "▤", label: "MEMORY", color: "var(--mauve)", toolMap: [{ prefix: "memory_" }] },
  { name: "files", icon: "▥", label: "FILES", color: "var(--muted)", toolMap: [{ exact: ["edit", "write"] }] },
  { name: "sandbox", icon: "▩", label: "SANDBOX", color: "var(--peach)", toolMap: [{ prefix: "sandbox_" }] },
  { name: "skills", icon: "✦", label: "SKILLS", color: "var(--yellow)", toolMap: [{ exact: ["skill", "create_skill", "list_skills", "create_agent", "list_agents"] }] },
  { name: "templates", icon: "⬡", label: "TEMPLATES" },
  { name: "design", icon: "❖", label: "DESIGN", color: "var(--pink)", toolMap: [{ prefix: "design_" }] },
  { name: "spec", icon: "◇", label: "SPEC", color: "var(--peach)", toolMap: [{ prefix: "openspec_" }] },
  { name: "living-docs", icon: "◧", label: "LIVING DOCS", color: "var(--pink)", toolMap: [{ prefix: "living_docs_" }] },
  { name: "vcs", icon: "⌁", label: "VCS", color: "var(--green)", toolMap: [{ prefix: "vcs_" }] },
  { name: "connections", icon: "⊕", label: "CONNECTIONS" },
  { name: "doctrine", icon: "◈", label: "DOCTRINE", color: "var(--lav)", toolMap: [{ exact: "agents_md" }] },
  { name: "marketplace", icon: "⚑", label: "MARKETPLACE" },
  { name: "perf", icon: "⚡", label: "PERFORMANCE" },
];

const PANEL_NAMES = PANEL_DEFINITIONS.map((entry) => entry.name);

export { PANEL_DEFINITIONS, PANEL_NAMES };
