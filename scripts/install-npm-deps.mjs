#!/usr/bin/env node
import fs from "node:fs";
import { spawn } from "node:child_process";
import process from "node:process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const packageJson = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
const installOptions = process.argv.slice(2);

function overridesScriptPolicy(option) {
  const flag = option.split("=", 1)[0];
  return flag === "--ignore-scripts" || flag === "--no-ignore-scripts";
}

if (installOptions.some(overridesScriptPolicy)) {
  console.error("install:deps controls lifecycle policy; remove --ignore-scripts/--no-ignore-scripts overrides");
  process.exitCode = 2;
} else {
  const approvedSpecs = Object.entries(packageJson.allowScripts ?? {})
    .filter(([, approved]) => approved === true)
    .map(([spec]) => spec)
    .sort();
  const invalidSpecs = approvedSpecs.filter((spec) => {
    const at = spec.lastIndexOf("@");
    return at <= 0 || at === spec.length - 1 || /[\^~*|\s]/.test(spec.slice(at + 1));
  });

  if (invalidSpecs.length) {
    console.error(`install:deps requires exact-version allowScripts entries: ${invalidSpecs.join(", ")}`);
    process.exitCode = 2;
  } else {
    const npmExecPath = process.env.npm_execpath;
    const npmCommand = npmExecPath
      ? process.execPath
      : process.platform === "win32"
        ? "npm.cmd"
        : "npm";
    const env = { ...process.env, SHARP_IGNORE_GLOBAL_LIBVIPS: "1" };

    function runNpm(args, label) {
      const npmArgs = npmExecPath ? [npmExecPath, ...args] : args;
      return new Promise((resolve) => {
        let settled = false;
        const settle = (result) => {
          if (!settled) {
            settled = true;
            resolve(result);
          }
        };
        const child = spawn(npmCommand, npmArgs, {
          cwd: root,
          env,
          stdio: "inherit",
          shell: process.platform === "win32" && !npmExecPath,
        });
        child.once("error", (error) => {
          console.error(`npm ${label} could not start: ${error.message}`);
          settle({ code: 1 });
        });
        child.once("close", (code, signal) => {
          if (signal) {
            console.error(`npm ${label} terminated by ${signal}`);
            settle({ code: 1 });
          } else {
            settle({ code: code ?? 1 });
          }
        });
      });
    }

    const dryRun = installOptions.some((option) => option === "--dry-run" || option === "--dry-run=true");
    const install = await runNpm(["install", ...installOptions, "--ignore-scripts"], "dependency install");
    if (install.code !== 0) {
      process.exitCode = install.code;
    } else if (dryRun || approvedSpecs.length === 0) {
      process.exitCode = 0;
    } else {
      const presentSpecs = [];
      const missingSpecs = [];
      for (const spec of approvedSpecs) {
        const at = spec.lastIndexOf("@");
        const name = spec.slice(0, at);
        const approvedVersion = spec.slice(at + 1);
        const packageJsonPath = path.join(root, "node_modules", ...name.split("/"), "package.json");
        if (!fs.existsSync(packageJsonPath)) {
          missingSpecs.push(spec);
          continue;
        }
        const installed = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
        if (installed.version !== approvedVersion) {
          console.error(`install:deps refuses lifecycle for ${name}@${installed.version}; approval is pinned to ${approvedVersion}`);
          process.exitCode = 2;
          break;
        }
        presentSpecs.push(spec);
      }

      if (process.exitCode === undefined && presentSpecs.length > 0) {
        if (missingSpecs.length > 0) {
          console.log(`Skipping absent approved packages: ${missingSpecs.join(", ")}`);
        }
        console.log(`Running install hooks only for approved packages: ${presentSpecs.join(", ")}`);
        const rebuild = await runNpm(
          ["rebuild", "--no-audit", "--no-fund", "--ignore-scripts=false", ...presentSpecs],
          "approved lifecycle scripts",
        );
        process.exitCode = rebuild.code;
      } else if (process.exitCode === undefined) {
        if (missingSpecs.length > 0) {
          console.log(`No approved lifecycle packages are installed: ${missingSpecs.join(", ")}`);
        }
        process.exitCode = 0;
      }
    }
  }
}
