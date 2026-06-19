/**
 * dotz auto-updater — user-controlled updates for the packaged Electron app.
 *
 * On launch:
 *   - checks the configured generic feed (DOTZ_UPDATE_URL, package.json, or electron-builder.yml)
 *   - if an update is available, sends an IPC event to the renderer so the UI can show a popup
 *     with three choices:
 *       1. "Download now" — download in background, then prompt to install once ready.
 *       2. "Later" — dismiss popup; the update will auto-install on the next launch.
 *       3. "Settings" — opens the app settings where a manual "Check for updates" button lives.
 *
 * Dev builds never check for updates. If no feed is configured, the app starts normally.
 */
import process from "node:process";
import path from "node:path";
import fs from "node:fs";
import { app, dialog, ipcMain, BrowserWindow } from "electron";
import pkg from "electron-updater";
import type { UpdateInfo } from "electron-updater";

const { autoUpdater } = pkg;

const UPDATE_FEED_ENV = process.env.DOTZ_UPDATE_URL || "";
const IS_DEV = !app.isPackaged;
const IS_PORTABLE = process.env.PORTABLE_EXECUTABLE_DIR != null;

let mainWindow: BrowserWindow | null = null;
let checking = false;
let downloadStarted = false;
let pendingUpdate: UpdateInfo | null = null;
let downloadPercent = 0;
let eventsAttached = false;
let errorEventFired = false;

/** Resolve the update server config from env/package.json. */
function resolveFeed(): string | undefined {
  if (UPDATE_FEED_ENV) return UPDATE_FEED_ENV;
  try {
    const pkgPath = process.resourcesPath
      ? path.join(process.resourcesPath, "app", "package.json")
      : path.resolve("./package.json");
    const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf-8"));
    const pub = pkg.build?.publish;
    if (typeof pub === "string") return pub;
    if (pub && pub.url) return pub.url;
    if (pub && pub.provider === "github") return "github";
    if (Array.isArray(pub)) {
      const first = pub.find((p: any) => p.url);
      if (first) return first.url;
      if (pub.some((p: any) => p.provider === "github")) return "github";
    }
  } catch {
    /* ignore */
  }
  return undefined;
}

/** Current app version, falling back to package.json in dev. */
export function getAppVersion(): string {
  try {
    if (app.isPackaged) return app.getVersion();
    const pkgPath = process.resourcesPath
      ? path.join(process.resourcesPath, "app", "package.json")
      : path.resolve("./package.json");
    return JSON.parse(fs.readFileSync(pkgPath, "utf-8")).version || "0.0.0";
  } catch {
    return "0.0.0";
  }
}

function sendToRenderer(status: string, data?: any) {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("dotz-update-status", status, data);
  }
}

function attachUpdaterEvents() {
  if (eventsAttached) return;
  eventsAttached = true;
  autoUpdater.on("checking-for-update", () => {
    sendToRenderer("checked", { phase: "checking", currentVersion: getAppVersion(), portable: IS_PORTABLE });
  });
  autoUpdater.on("update-available", (info) => {
    pendingUpdate = info;
    sendToRenderer("checked", { phase: "available", version: info.version, currentVersion: getAppVersion(), portable: IS_PORTABLE });
    sendToRenderer("available", { version: info.version, currentVersion: getAppVersion(), notes: info.releaseNotes,
      portable: IS_PORTABLE, manual: IS_PORTABLE });
  });

  autoUpdater.on("update-not-available", () => {
    sendToRenderer("checked", { phase: "current", currentVersion: getAppVersion(), portable: IS_PORTABLE });
    sendToRenderer("not-available", { currentVersion: getAppVersion(), portable: IS_PORTABLE });
  });

  autoUpdater.on("download-progress", (p) => {
    downloadPercent = p.percent;
    sendToRenderer("downloading", { percent: Math.round(p.percent), bytesPerSecond: p.bytesPerSecond });
  });

  autoUpdater.on("update-downloaded", (info) => {
    pendingUpdate = info;
    sendToRenderer("downloaded", { version: info.version });
    sendToRenderer("ready", { version: info.version });
  });

  autoUpdater.on("error", (err) => {
    errorEventFired = true;
    sendToRenderer("failed", { message: err.message });
  });
}

function configureUpdater(feed: string): void {
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = !IS_PORTABLE;
  if (feed !== "github") autoUpdater.setFeedURL({ url: feed, provider: "generic" as any });
}

/** Start an update check. Call once the main window exists. */
export async function checkForUpdatesOnLaunch(win: BrowserWindow): Promise<void> {
  mainWindow = win;
  if (IS_DEV) {
    console.log("dotz updater: skipped in dev build");
    return;
  }
  if (checking) return;
  const feed = resolveFeed();
  if (!feed) {
    console.log("dotz updater: no feed configured");
    return;
  }
  checking = true;
  downloadStarted = false;
  errorEventFired = false;
  try {
    attachUpdaterEvents();
    configureUpdater(feed);
    await autoUpdater.checkForUpdates();
  } catch (e) {
    // A thrown/failed check (network down, bad feed) must surface to the renderer so the UI can
    // show a stale/failed indicator instead of appearing to silently succeed — but only if the
    // autoUpdater "error" event didn't already report it (electron-updater commonly both emits
    // "error" AND rejects the promise for the same failure).
    if (!errorEventFired) sendToRenderer("failed", { message: String((e as Error)?.message ?? e) });
  } finally {
    checking = false;
  }
}

/** Download the discovered update. */
export async function downloadUpdate(): Promise<void> {
  if (IS_DEV || downloadStarted) return;
  if (IS_PORTABLE) {
    sendToRenderer("deferred", { portable: true, message: "Portable builds update manually from GitHub Releases." });
    return;
  }
  const feed = resolveFeed();
  if (!feed) return;
  downloadStarted = true;
  try {
    configureUpdater(feed);
    await autoUpdater.downloadUpdate();
  } catch (e) {
    downloadStarted = false;
    sendToRenderer("failed", { message: (e as Error).message });
  }
}

/** Install a downloaded update and restart. */
export function installUpdate(): void {
  if (IS_DEV || !pendingUpdate) return;
  if (IS_PORTABLE) {
    sendToRenderer("deferred", { portable: true, message: "Portable builds cannot install updates automatically." });
    return;
  }
  autoUpdater.quitAndInstall(false, true);
}

export function deferUpdate(): void {
  sendToRenderer("deferred", {
    version: pendingUpdate?.version,
    portable: IS_PORTABLE,
    message: IS_PORTABLE ? "Portable builds update manually from GitHub Releases." : "Update deferred until app exit.",
  });
}

/** Manual check invoked from settings. */
export async function manualUpdateCheck(parentWindow?: BrowserWindow): Promise<void> {
  if (IS_DEV) {
    dialog.showMessageBox(parentWindow || (mainWindow ?? undefined)!, {
      type: "info",
      title: "dotz updater",
      message: "Updates are not checked in development builds.",
    });
    return;
  }
  const feed = resolveFeed();
  if (!feed) {
    dialog.showMessageBox(parentWindow || (mainWindow ?? undefined)!, {
      type: "warning",
      title: "dotz updater",
      message: "No update feed is configured. Set DOTZ_UPDATE_URL or package.json build.publish.",
    });
    return;
  }
  try {
    attachUpdaterEvents();
    configureUpdater(feed);
    await autoUpdater.checkForUpdates();
  } catch (e) {
    dialog.showErrorBox("dotz updater", (e as Error).message);
  }
}

/** Wire IPC handlers for renderer-driven updater actions. */
export function wireUpdaterIpc() {
  ipcMain.on("dotz-update-download", () => {
    downloadUpdate();
  });
  ipcMain.on("dotz-update-check", () => {
    manualUpdateCheck(mainWindow ?? undefined);
  });
  ipcMain.on("dotz-update-install", () => {
    installUpdate();
  });
  ipcMain.on("dotz-update-defer", () => {
    deferUpdate();
  });
}

/** Called by main.ts to defer install to the next launch (silent fallback). */
export function enableInstallOnQuit(): void {
  if (IS_DEV || IS_PORTABLE) return;
  autoUpdater.autoInstallOnAppQuit = true;
}
