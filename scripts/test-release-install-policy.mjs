import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const workflowPath = path.join(root, ".github", "workflows", "release.yml");
const workflow = fs.readFileSync(workflowPath, "utf8");
const step = workflow.match(
  /- name: fetch embed model\s+run: \|\s+([\s\S]*?)(?=\n\s+- uses:|\n\s+- name:|$)/,
);
if (!step) {
  throw new Error("release workflow must have a fetch embed model run block");
}
const commands = step[1]
  .split(/\r?\n/)
  .map((line) => line.replace(/#.*$/, "").trim())
  .filter(Boolean);
if (commands[0] !== "npm run install:deps") {
  throw new Error(
    `release workflow must install dependencies through npm run install:deps; found ${commands[0] ?? "no install command"}`,
  );
}
if (!commands.includes("npm run fetch-model")) {
  throw new Error("release workflow must continue to fetch the embedding model");
}
if (commands.some((command) => /^npm\s+(?:ci|install)(?:\s|$)/.test(command))) {
  throw new Error("release workflow bypasses lifecycle policy with raw npm ci/install");
}
console.log(JSON.stringify({ passed: true, workflow: ".github/workflows/release.yml", commands }, null, 2));
