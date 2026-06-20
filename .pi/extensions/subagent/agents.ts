/**
 * Agent discovery and configuration
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { getAgentDir, parseFrontmatter } from "@earendil-works/pi-coding-agent";

export type AgentScope = "user" | "project" | "both";

export interface AgentConfig {
	name: string;
	description: string;
	tools?: string[];
	model?: string;
	systemPrompt: string;
	source: "user" | "project";
	filePath: string;
}

export interface AgentDiscoveryResult {
	agents: AgentConfig[];
	projectAgentsDir: string | null;
}

export interface CreateAgentInput {
	name: string;
	description: string;
	systemPrompt: string;
	tools?: string[];
	model?: string;
}

const RESOURCE_NAME = /^[a-z][a-z0-9-]{1,63}$/;

function requireResourceName(name: string): string {
	const value = name.trim();
	if (!RESOURCE_NAME.test(value)) {
		throw new Error("agent name must start with a lowercase letter and contain only lowercase letters, digits, and hyphens (2-64 chars)");
	}
	return value;
}

function yamlString(value: string): string {
	return JSON.stringify(value.replace(/\r?\n/g, " ").trim());
}

/** Persist a user agent in Pi's native discovery directory without overwriting an existing agent. */
export function createUserAgent(input: CreateAgentInput): AgentConfig {
	const name = requireResourceName(input.name);
	const description = input.description.trim();
	const systemPrompt = input.systemPrompt.trim();
	if (!description) throw new Error("agent description is required");
	if (!systemPrompt) throw new Error("agent systemPrompt is required");

	const tools = input.tools?.map((tool) => tool.trim()).filter(Boolean);
	if (tools?.some((tool) => !/^[a-zA-Z0-9_-]+$/.test(tool))) {
		throw new Error("agent tools may contain only letters, digits, underscores, and hyphens");
	}
	const model = input.model?.trim() || "ollama/minimax-m3";
	const dir = path.join(getAgentDir(), "agents");
	const filePath = path.join(dir, `${name}.md`);
	fs.mkdirSync(dir, { recursive: true });
	const lines = [
		"---",
		`name: ${yamlString(name)}`,
		`description: ${yamlString(description)}`,
		`model: ${yamlString(model)}`,
	];
	if (tools?.length) lines.push(`tools: ${yamlString(tools.join(", "))}`);
	lines.push("---", "", systemPrompt, "");
	try {
		fs.writeFileSync(filePath, lines.join("\n"), { encoding: "utf-8", flag: "wx" });
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "EEXIST") throw new Error(`agent \"${name}\" already exists`);
		throw error;
	}
	return { name, description, tools: tools?.length ? tools : undefined, model, systemPrompt, source: "user", filePath };
}

function loadAgentsFromDir(dir: string, source: "user" | "project"): AgentConfig[] {
	const agents: AgentConfig[] = [];

	if (!fs.existsSync(dir)) {
		return agents;
	}

	let entries: fs.Dirent[];
	try {
		entries = fs.readdirSync(dir, { withFileTypes: true });
	} catch {
		return agents;
	}

	for (const entry of entries) {
		if (!entry.name.endsWith(".md")) continue;
		if (!entry.isFile() && !entry.isSymbolicLink()) continue;

		const filePath = path.join(dir, entry.name);
		let content: string;
		try {
			content = fs.readFileSync(filePath, "utf-8");
		} catch {
			continue;
		}

		const { frontmatter, body } = parseFrontmatter<Record<string, string>>(content);

		if (!frontmatter.name || !frontmatter.description) {
			continue;
		}

		const tools = frontmatter.tools
			?.split(",")
			.map((t: string) => t.trim())
			.filter(Boolean);

		agents.push({
			name: frontmatter.name,
			description: frontmatter.description,
			tools: tools && tools.length > 0 ? tools : undefined,
			model: frontmatter.model,
			systemPrompt: body,
			source,
			filePath,
		});
	}

	return agents;
}

function isDirectory(p: string): boolean {
	try {
		return fs.statSync(p).isDirectory();
	} catch {
		return false;
	}
}

function findNearestProjectAgentsDir(cwd: string): string | null {
	let currentDir = cwd;
	while (true) {
		const candidate = path.join(currentDir, ".pi", "agents");
		if (isDirectory(candidate)) return candidate;

		const parentDir = path.dirname(currentDir);
		if (parentDir === currentDir) return null;
		currentDir = parentDir;
	}
}

// dotz ships its core workflow agents (scout/planner/reviewer/worker) next to this extension so the
// built-in workflows (/ultra-code-review, /implement, ...) work against ANY project cwd — not only
// when the session happens to run inside the dotz repo. Resolve them relative to this file:
//   <dotz>/.pi/extensions/subagent/agents.ts  ->  <dotz>/.pi/agents
function getBundledAgentsDir(): string | null {
	try {
		const here = path.dirname(fileURLToPath(import.meta.url));
		const dir = path.join(here, "..", "..", "agents");
		return isDirectory(dir) ? dir : null;
	} catch {
		return null;
	}
}

export function discoverAgents(cwd: string, scope: AgentScope): AgentDiscoveryResult {
	const userDir = path.join(getAgentDir(), "agents");
	const projectAgentsDir = findNearestProjectAgentsDir(cwd);
	const bundledDir = getBundledAgentsDir();

	// Bundled dotz agents are ALWAYS available (treated as global/user scope) regardless of cwd or the
	// requested scope, so the flagship workflows never fail with "Available agents: none" on an
	// external project. Without this, discovery only finds agents in ~/.pi/agent/agents or an ancestor
	// .pi/agents of the cwd — neither of which contains dotz's bundled agents for a non-dotz project.
	const bundledAgents = bundledDir ? loadAgentsFromDir(bundledDir, "user") : [];
	const userAgents = scope === "project" ? [] : loadAgentsFromDir(userDir, "user");
	const projectAgents = scope === "user" || !projectAgentsDir ? [] : loadAgentsFromDir(projectAgentsDir, "project");

	const agentMap = new Map<string, AgentConfig>();
	// Precedence (low -> high): bundled defaults, then user agents, then project-local overrides.
	for (const agent of bundledAgents) agentMap.set(agent.name, agent);
	if (scope !== "project") for (const agent of userAgents) agentMap.set(agent.name, agent);
	if (scope !== "user") for (const agent of projectAgents) agentMap.set(agent.name, agent);

	return { agents: Array.from(agentMap.values()), projectAgentsDir };
}

export function formatAgentList(agents: AgentConfig[], maxItems: number): { text: string; remaining: number } {
	if (agents.length === 0) return { text: "none", remaining: 0 };
	const listed = agents.slice(0, maxItems);
	const remaining = agents.length - listed.length;
	return {
		text: listed.map((a) => `${a.name} (${a.source}): ${a.description}`).join("; "),
		remaining,
	};
}
