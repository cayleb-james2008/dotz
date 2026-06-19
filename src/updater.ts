/**
 * dotz source-rebuild updater — keeps dotz a single PORTABLE exe but updates it by
 * git pull + rebuild + relaunch (NOT electron-updater, which cannot self-replace a
 * running portable exe).
 *
 * Model (operator's chosen update model — same as the other source-rebuild updaters):
 *   - The machine has the dotz source repo + node/npm present.
 *   - CHECK  = `git fetch` then count commits HEAD..origin/<branch> and detect a dirty
 *              tree (which would block an ff pull). Surfaced to the renderer as
 *              "update available" with behind-count + short sha.
 *   - APPLY  = spawn a DETACHED helper (.bat) that waits for this app to exit, runs
 *              `git pull --ff-only` + the portable rebuild npm script, then relaunches
 *              the freshly-built portable exe. The helper is detached so the rebuild +
 *              swap survive this process quitting (the exe being rebuilt may be the very
 *              file that is running).
 *
 * Dev builds (unpackaged) still run the git check so the UI is exercisable, but APPLY
 * relaunches `electron .` via the rebuild rather than a portable exe.
 *
 * The renderer UI (CHECK FOR UPDATES button + UPDATE AVAILABLE card) is unchanged; this
 * module only changes what each control does. IPC channel is the same: "dotz-update-status".
 */
import process from "node:process";
import path from "node:path";
import fs from "node:fs";
import os from "node:os";
import { spawn, execFile } from "node:child_process";
import { app, ipcMain, BrowserWindow } from "electron";
import { checkForUpdate, buildHelperScript, type GitRunner } from "./updater-core";

export { checkForUpdate, buildHelperScript } from "./updater-core";

const IS_DEV = !app.isPackaged;
/** Portable exe sets PORTABLE_EXECUTABLE_DIR; in dev there is no portable exe. */
const PORTABLE_DIR = process.env.PORTABLE_EXECUTABLE_DIR || null;
/** npm script that produces a fresh portable exe (build + portable-only electron-builder). */
const REBUILD_SCRIPT = "dist:portable";

let mainWindow: BrowserWindow | null = null;
let checking = false;
let applying = false;
/** Cached result of the last successful check, so APPLY knows there is something to do. */
let lastCheck: { behind: number; localSha: string; remoteSha: string; dirty: boolean } | null = null;

/** Current app version, falling back to package.json in dev. */
export function getAppVersion(): string {
  try {
    if (app.isPackaged) return app.getVersion();
    return JSON.parse(fs.readFileSync(path.resolve("./package.json"), "utf-8")).version || "0.0.0";
  } catch {
    return "0.0.0";
  }
}

/**
 * Resolve the dotz source repo dir (the git checkout to pull + rebuild).
 *   1. DOTZ_REPO_DIR env override.
 *   2. Walk up from the running exe / app dir until a `.git` entry is found
 *      (portable exe lives in <repo>/release/, dev runs from <repo>).
 */
export function resolveRepoDir(): string | null {
  const override = process.env.DOTZ_REPO_DIR;
  if (override && fs.existsSync(path.join(override, ".git"))) return override;
  const start = PORTABLE_DIR || path.dirname(process.execPath);
  const candidates = [start, app.getAppPath(), process.cwd()];
  for (const c of candidates) {
    let dir = c;
    for (let i = 0; i < 8 && dir; i++) {
      if (fs.existsSync(path.join(dir, ".git"))) return dir;
      const parent = path.dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  }
  return null;
}

function sendToRenderer(status: string, data?: any) {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("dotz-update-status", status, data);
  }
}

/** Build a repo-bound git runner via child_process; never throws — resolves { code, stdout, stderr }. */
function makeGit(repo: string): GitRunner {
  return (args: string[]) =>
    new Promise((resolve) => {
      execFile("git", args, { cwd: repo, windowsHide: true }, (err, stdout, stderr) => {
        const code = err && typeof (err as any).code === "number" ? (err as any).code : err ? 1 : 0;
        resolve({ code, stdout: stdout?.toString() ?? "", stderr: stderr?.toString() ?? "" });
      });
    });
}

/** Run a git check and surface the result to the renderer via the update status channel. */
export async function runUpdateCheck(): Promise<void> {
  if (checking) return;
  checking = true;
  try {
    sendToRenderer("checked", { phase: "checking", currentVersion: getAppVersion() });
    const repo = resolveRepoDir();
    if (!repo) {
      sendToRenderer("failed", { message: "dotz source repo not found (set DOTZ_REPO_DIR)." });
      return;
    }
    const res = await checkForUpdate(makeGit(repo));
    if (!res.ok) {
      sendToRenderer("failed", { message: res.error || "update check failed" });
      return;
    }
    lastCheck = { behind: res.behind, localSha: res.localSha, remoteSha: res.remoteSha, dirty: res.dirty };
    if (res.behind > 0) {
      sendToRenderer("available", {
        behind: res.behind,
        localSha: res.localSha,
        remoteSha: res.remoteSha,
        dirty: res.dirty,
        currentVersion: getAppVersion(),
      });
    } else {
      sendToRenderer("not-available", { currentVersion: getAppVersion(), localSha: res.localSha });
    }
  } finally {
    checking = false;
  }
}

/**
 * APPLY: write the detached helper, spawn it detached, then signal the renderer and quit.
 * The helper waits for this process to exit before pulling/rebuilding so the running exe
 * (which may be the file being overwritten) is free.
 */
export async function applyUpdate(): Promise<void> {
  if (applying) return;
  applying = true;
  const repo = resolveRepoDir();
  if (!repo) {
    sendToRenderer("failed", { message: "dotz source repo not found (set DOTZ_REPO_DIR)." });
    applying = false;
    return;
  }
  if (lastCheck?.dirty) {
    sendToRenderer("failed", {
      message: "local source tree has uncommitted changes — commit/stash them before updating.",
    });
    applying = false;
    return;
  }
  const helperPath = path.join(os.tmpdir(), `dotz-update-${Date.now()}.bat`);
  const script = buildHelperScript({
    repo,
    pid: process.pid,
    exePath: process.execPath,
    rebuildScript: REBUILD_SCRIPT,
    isPackaged: app.isPackaged,
  });
  try {
    fs.writeFileSync(helperPath, script, "utf-8");
  } catch (e) {
    sendToRenderer("failed", { message: `could not write updater helper: ${(e as Error).message}` });
    applying = false;
    return;
  }
  sendToRenderer("applying", { remoteSha: lastCheck?.remoteSha, behind: lastCheck?.behind });
  // Detach the helper so it outlives this app; a new console hosts the build output.
  const child = spawn("cmd.exe", ["/c", "start", '""', "cmd", "/c", helperPath], {
    cwd: repo,
    detached: true,
    stdio: "ignore",
    windowsHide: false, // user-facing build progress window
  });
  child.unref();
  // Give the helper a moment to start watching our pid, then quit so it can swap the exe.
  setTimeout(() => app.quit(), 400);
}

/** Start an update check on launch (background). Call once the main window exists. */
export async function checkForUpdatesOnLaunch(win: BrowserWindow): Promise<void> {
  mainWindow = win;
  await runUpdateCheck();
}

/** Wire IPC handlers for renderer-driven updater actions. */
export function wireUpdaterIpc() {
  ipcMain.on("dotz-update-check", () => {
    runUpdateCheck();
  });
  ipcMain.on("dotz-update-apply", () => {
    applyUpdate();
  });
}
