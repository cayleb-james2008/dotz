#!/usr/bin/env node
// Guard: every child_process spawn in first-party code must pass
// `windowsHide: true`, so the app never pops a console window on the desktop.
// Exits non-zero (failing the build/gate) on any violation. Run via `npm run check`.
//
// Portable: scans from the repo root (parent of this script's dir), skipping
// vendored/build trees. Precise child_process binding detection avoids false
// positives like `regex.exec(...)`.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SKIP = new Set([
  "node_modules", "dist", "release", ".git", "build", "out", "coverage",
]);
const EXTS = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];
const SPAWN = ["spawn", "spawnSync", "exec", "execSync", "execFile", "execFileSync", "fork"];

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    if (SKIP.has(name)) continue;
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) yield* walk(p);
    else if (EXTS.some((e) => p.endsWith(e))) yield p;
  }
}

function boundNames(src) {
  const bare = new Set();
  const ns = new Set();
  const cp = `['"](?:node:)?child_process['"]`;
  for (const m of src.matchAll(new RegExp(`import\\s*\\{([^}]*)\\}\\s*from\\s*${cp}`, "g")))
    for (const piece of m[1].split(",")) {
      const local = piece.split(" as ").pop().trim();
      if (local) bare.add(local);
    }
  for (const m of src.matchAll(new RegExp(`(?:const|let|var)\\s*\\{([^}]*)\\}\\s*=\\s*require\\(\\s*${cp}\\s*\\)`, "g")))
    for (const piece of m[1].split(",")) {
      const local = piece.split(":").pop().trim();
      if (local) bare.add(local);
    }
  for (const m of src.matchAll(new RegExp(`import\\s+(?:\\*\\s+as\\s+)?(\\w+)\\s+from\\s*${cp}`, "g")))
    ns.add(m[1]);
  for (const m of src.matchAll(new RegExp(`(?:const|let|var)\\s+(\\w+)\\s*=\\s*require\\(\\s*${cp}\\s*\\)`, "g")))
    ns.add(m[1]);
  return { bare, ns };
}

function argSpan(src, openParen) {
  let depth = 0;
  for (let i = openParen; i < src.length; i++) {
    if (src[i] === "(") depth++;
    else if (src[i] === ")" && --depth === 0) return src.slice(openParen, i + 1);
  }
  return src.slice(openParen);
}

const violations = [];
for (const file of walk(ROOT)) {
  const src = readFileSync(file, "utf8");
  if (!src.includes("child_process")) continue;
  const { bare, ns } = boundNames(src);
  const patterns = [];
  for (const fn of SPAWN) {
    if (bare.has(fn)) patterns.push(new RegExp(`(?<![.\\w])${fn}\\s*\\(`, "g"));
    for (const alias of ns) patterns.push(new RegExp(`\\b${alias}\\.${fn}\\s*\\(`, "g"));
  }
  for (const pat of patterns)
    for (const m of src.matchAll(pat)) {
      const open = src.indexOf("(", m.index);
      if (!argSpan(src, open).includes("windowsHide")) {
        const line = src.slice(0, m.index).split("\n").length;
        violations.push(`${file}:${line}: child_process ${m[0].replace(/\s*\($/, "")}() must pass windowsHide: true`);
      }
    }
}

if (violations.length) {
  console.error("✗ console-window guard failed:\n  " + violations.join("\n  "));
  console.error("\nPass { windowsHide: true } to every child_process spawn.");
  process.exit(1);
}
console.log("✓ no-console-window guard: all child_process spawns pass windowsHide: true");
