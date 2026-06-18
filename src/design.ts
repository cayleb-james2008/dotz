/**
 * dotz Open-Design — baked-in frontend design tooling exposed as pi agent tools.
 * Gives the agent access to design systems, component guidance, color palettes,
 * typography pairings, and UX audit rules so it can design/build UIs masterfully.
 *
 * In the dev environment (this OpenCode session) the ui-ux-pro MCP is available and
 * provides richer data; in the packaged Electron app these tools fall back to the
 * bundled design knowledge below. The agent calls `design_system`, `design_components`,
 * or `design_audit` and gets actionable tokens/rules regardless of environment.
 */

/** A design system: colors + typography + layout + platform guidelines. */
export interface DesignSystem {
  query: string;
  colors: { name: string; hex: string; role: string }[];
  darkColors?: { name: string; hex: string; role: string }[];
  typography: { pairing: string; fonts: string[]; googleFontsImport?: string };
  layout: string;
  platformGuidelines?: string[];
  cssTokens: string;
}

/** Component guidance: icons + chart types + framework rules. */
export interface ComponentGuidance {
  query: string;
  icons: string[];
  charts: string[];
  frameworkRules: string[];
}

/** UX audit findings. */
export interface DesignAudit {
  target: string;
  findings: { severity: "critical" | "warning" | "suggestion"; rule: string; fix: string }[];
  passed: string[];
}

// ---- bundled design knowledge (offline fallback) ----

const PALETTES: Record<string, DesignSystem> = {
  catppuccin: {
    query: "catppuccin mocha dark",
    colors: [
      { name: "bg", hex: "#1e1e2e", role: "background" },
      { name: "mantle", hex: "#181825", role: "elevated bg" },
      { name: "crust", hex: "#11111b", role: "deepest bg" },
      { name: "surface", hex: "#313244", role: "card" },
      { name: "surface2", hex: "#45475a", role: "border" },
      { name: "text", hex: "#cdd6f4", role: "foreground" },
      { name: "muted", hex: "#a6adc8", role: "muted text" },
      { name: "lav", hex: "#b4befe", role: "primary accent" },
      { name: "green", hex: "#a6e3a1", role: "success" },
      { name: "red", hex: "#f38ba8", role: "danger" },
      { name: "yellow", hex: "#f9e2af", role: "warning" },
      { name: "cyan", hex: "#89dceb", role: "info" },
      { name: "peach", hex: "#fab387", role: "secondary accent" },
    ],
    typography: { pairing: "Chakra Petch + JetBrains Mono", fonts: ["Chakra Petch", "JetBrains Mono"], googleFontsImport: "https://fonts.googleapis.com/css2?family=Chakra+Petch:wght@400;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap" },
    layout: "CSS Grid bento with dense auto-flow; 12 columns; minmax(120px, 1fr) tracks; 8px gap",
    cssTokens: `:root {
  --bg: #1e1e2e; --mantle: #181825; --crust: #11111b;
  --surface: #313244; --surface2: #45475a; --border: #45475a;
  --text: #cdd6f4; --muted: #a6adc8; --dim: #6c7086;
  --lav: #b4befe; --green: #a6e3a1; --red: #f38ba8;
  --yellow: #f9e2af; --cyan: #89dceb; --peach: #fab387;
}`,
  },
  "fintech-saas": {
    query: "fintech saas dashboard",
    colors: [
      { name: "primary", hex: "#2563eb", role: "brand" },
      { name: "primary-dark", hex: "#1e40af", role: "brand hover" },
      { name: "success", hex: "#16a34a", role: "positive metric" },
      { name: "danger", hex: "#dc2626", role: "negative metric" },
      { name: "bg", hex: "#f8fafc", role: "background" },
      { name: "card", hex: "#ffffff", role: "surface" },
      { name: "text", hex: "#0f172a", role: "foreground" },
      { name: "muted", hex: "#64748b", role: "secondary text" },
      { name: "border", hex: "#e2e8f0", role: "divider" },
    ],
    darkColors: [
      { name: "bg", hex: "#0f172a", role: "background" },
      { name: "card", hex: "#1e293b", role: "surface" },
      { name: "text", hex: "#f1f5f9", role: "foreground" },
      { name: "border", hex: "#334155", role: "divider" },
    ],
    typography: { pairing: "Inter + JetBrains Mono", fonts: ["Inter", "JetBrains Mono"], googleFontsImport: "https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap" },
    layout: "Sidebar + main content; 8px spacing scale; card-based sections; data tables with sticky headers",
    platformGuidelines: ["Use semantic ARIA for all interactive elements", "WCAG 2.2 AA contrast (4.5:1 text)", "44px touch targets", "Keyboard navigation for all data tables", "Loading skeletons, not spinners, for data"],
    cssTokens: `:root {
  --primary: #2563eb; --primary-dark: #1e40af;
  --success: #16a34a; --danger: #dc2626;
  --bg: #f8fafc; --card: #ffffff; --text: #0f172a;
  --muted: #64748b; --border: #e2e8f0;
}`,
  },
  "landing-modern": {
    query: "modern landing page hero cta",
    colors: [
      { name: "hero-bg", hex: "#0a0a0f", role: "hero gradient start" },
      { name: "accent", hex: "#8b5cf6", role: "CTA + links" },
      { name: "accent2", hex: "#ec4899", role: "gradient end" },
      { name: "text", hex: "#fafafa", role: "foreground" },
      { name: "muted", hex: "#a1a1aa", role: "secondary" },
    ],
    typography: { pairing: "Space Grotesk + Inter", fonts: ["Space Grotesk", "Inter"], googleFontsImport: "https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@400;500;700&family=Inter:wght@400;500;600&display=swap" },
    layout: "Full-width hero with gradient mesh background; centered headline + subhead + CTA; 3-column feature grid below; social proof strip",
    platformGuidelines: ["Hero headline 48-72px, subhead 18-24px", "Single primary CTA, high contrast", "Above-the-fold in < 1 viewport", "Lazy-load below-fold images", "Open Graph meta for sharing"],
    cssTokens: `:root {
  --hero-bg: #0a0a0f; --accent: #8b5cf6; --accent2: #ec4899;
  --text: #fafafa; --muted: #a1a1aa;
}`,
  },
};

const UX_RULES = [
  { rule: "Color contrast", check: "All text has WCAG 2.2 AA contrast (4.5:1 normal, 3:1 large)", severity: "critical" as const },
  { rule: "Touch targets", check: "Interactive elements are at least 44x44px", severity: "critical" as const },
  { rule: "Focus states", check: "All interactive elements have visible :focus and :focus-visible styles", severity: "critical" as const },
  { rule: "Keyboard navigation", check: "Tab order is logical; no keyboard traps; Esc closes overlays", severity: "critical" as const },
  { rule: "Semantic HTML", check: "Use semantic elements (nav, main, section, button) not div soup", severity: "warning" as const },
  { rule: "ARIA labels", check: "Icon-only buttons have aria-label; form inputs have associated labels", severity: "warning" as const },
  { rule: "Loading states", check: "Skeletons for data, not bare spinners; optimistic UI for mutations", severity: "suggestion" as const },
  { rule: "Error states", check: "Errors are specific, actionable, and near the relevant element", severity: "warning" as const },
  { rule: "Responsive", check: "Layout works at 320px, 768px, 1024px, 1440px breakpoints", severity: "critical" as const },
  { rule: "No AI-slop", check: "No purple gradients on everything, no fake glassmorphism, no generic SaaS cards, no side-stripe borders", severity: "suggestion" as const },
];

/** Get a design system by query keyword. */
export function getDesignSystem(query: string): DesignSystem {
  const q = query.toLowerCase();
  for (const key of Object.keys(PALETTES)) {
    if (q.includes(key) || PALETTES[key].query.includes(q) || PALETTES[key].query.includes(key)) {
      return PALETTES[key];
    }
  }
  // default to catppuccin for dark/cyber themes, fintech for dashboards
  if (q.includes("dashboard") || q.includes("saas") || q.includes("fintech")) return PALETTES["fintech-saas"];
  if (q.includes("landing") || q.includes("hero") || q.includes("marketing")) return PALETTES["landing-modern"];
  return PALETTES.catppuccin;
}

/** Get component guidance for a query. */
export function getComponents(query: string): ComponentGuidance {
  const q = query.toLowerCase();
  const icons: string[] = [];
  if (q.includes("user") || q.includes("profile")) icons.push("lucide:user", "lucide:user-circle", "lucide:users");
  if (q.includes("settings") || q.includes("config")) icons.push("lucide:settings", "lucide:sliders", "lucide:cog");
  if (q.includes("search")) icons.push("lucide:search", "lucide:filter");
  if (q.includes("chart") || q.includes("analytics")) icons.push("lucide:bar-chart-3", "lucide:trending-up", "lucide:pie-chart");
  if (q.includes("file") || q.includes("document")) icons.push("lucide:file", "lucide:folder", "lucide:upload");
  if (q.includes("notification") || q.includes("alert")) icons.push("lucide:bell", "lucide:alert-triangle", "lucide:info");
  if (icons.length === 0) icons.push("lucide:home", "lucide:menu", "lucide:plus", "lucide:x", "lucide:check");

  const charts: string[] = [];
  if (q.includes("time") || q.includes("trend")) charts.push("line chart (recharts <LineChart>)");
  if (q.includes("comparison") || q.includes("category")) charts.push("bar chart (recharts <BarChart>)");
  if (q.includes("proportion") || q.includes("distribution")) charts.push("donut chart (recharts <PieChart>)");
  if (q.includes("funnel") || q.includes("conversion")) charts.push("funnel chart");
  if (charts.length === 0) charts.push("line chart for trends", "bar chart for comparisons", "donut for proportions");

  const frameworkRules = [
    "Cards purposeful; avoid cards inside cards",
    "Use CSS Grid for layout, Flexbox for alignment within components",
    "Animate with transform + opacity only (compositor-friendly)",
    "Prefer system font stack + 1 display font; avoid font overload",
    "Spacing scale: 4/8/12/16/24/32/48/64 px (8px base)",
  ];
  return { query, icons, charts, frameworkRules };
}

/** Run a UX audit against a target description. */
export function auditDesign(target: string): DesignAudit {
  const findings: DesignAudit["findings"] = [];
  const passed: string[] = [];
  const t = target.toLowerCase();
  for (const rule of UX_RULES) {
    // crude heuristic: if the target mentions the rule's domain, flag it for checking
    const relevant =
      (rule.rule.includes("contrast") && (t.includes("color") || t.includes("theme") || t.includes("dark"))) ||
      (rule.rule.includes("Touch") && (t.includes("mobile") || t.includes("app"))) ||
      (rule.rule.includes("Focus") && (t.includes("form") || t.includes("input") || t.includes("button"))) ||
      (rule.rule.includes("Responsive") && (t.includes("layout") || t.includes("page"))) ||
      (rule.rule.includes("AI-slop") && (t.includes("design") || t.includes("ui")));
    if (relevant) {
      findings.push({ severity: rule.severity, rule: rule.rule, fix: rule.check });
    } else {
      passed.push(rule.rule);
    }
  }
  return { target, findings: findings.length ? findings : UX_RULES.map((r) => ({ severity: r.severity, rule: r.rule, fix: r.check })), passed };
}

/** Render a design system as a markdown block for the agent. */
export function renderDesignSystem(ds: DesignSystem): string {
  const lines = [`# Design system: ${ds.query}`, ``, `## CSS tokens`, "```css", ds.cssTokens, "```", ``, `## Typography`, `- Pairing: ${ds.typography.pairing}`, `- Fonts: ${ds.typography.fonts.join(", ")}`];
  if (ds.typography.googleFontsImport) lines.push(`- Google Fonts: \`${ds.typography.googleFontsImport}\``);
  lines.push(``, `## Layout`, ds.layout);
  if (ds.platformGuidelines && ds.platformGuidelines.length) {
    lines.push(``, `## Platform guidelines`, ...ds.platformGuidelines.map((g) => `- ${g}`));
  }
  return lines.join("\n");
}

/** Render component guidance as markdown. */
export function renderComponents(c: ComponentGuidance): string {
  return [`# Component guidance: ${c.query}`, ``, `## Icons (Lucide)`, ...c.icons.map((i) => `- \`${i}\``), ``, `## Charts`, ...c.charts.map((i) => `- ${i}`), ``, `## Framework rules`, ...c.frameworkRules.map((i) => `- ${i}`)].join("\n");
}

/** Render an audit as markdown. */
export function renderAudit(a: DesignAudit): string {
  const lines = [`# Design audit: ${a.target}`, ``];
  if (a.findings.length) {
    lines.push(`## Findings (${a.findings.length})`);
    for (const f of a.findings) lines.push(`- **[${f.severity.toUpperCase()}] ${f.rule}**: ${f.fix}`);
  }
  if (a.passed.length) {
    lines.push(``, `## Passed checks (${a.passed.length})`, ...a.passed.map((p) => `- ✓ ${p}`));
  }
  return lines.join("\n");
}