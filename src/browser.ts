/**
 * dotz in-app browser — embeds a Chromium instance inside the Electron window via
 * WebContentsView (the modern replacement for BrowserView). Uses a dedicated
 * --user-data-dir so it never conflicts with the user's personal Chrome profile.
 *
 * In browser-dev mode (npm run dev:server, no Electron), this module is a no-op stub
 * that returns "not available outside Electron" — the UI shows the placeholder.
 *
 * Control surface: navigate(url), back(), forward(), reload(), screenshot(), eval(js).
 * The agent drives it via the `browser_*` pi tools (registered in dotz-tools); the UI
 * panel calls the REST endpoints to navigate + view.
 */
import path from "node:path";
import os from "node:os";
import fs from "node:fs/promises";

const BROWSER_PROFILE_DIR = path.join(os.homedir(), ".dotz", "ai-agents", "browser-profile");

let view: import("electron").WebContentsView | null = null;
let hostWin: import("electron").BrowserWindow | null = null;
let currentUrl = "about:blank";

/** Whether we're running inside Electron (vs plain browser dev). */
export function isAvailable(): boolean {
  return !!process.versions.electron;
}

/** Attach the browser view to a BrowserWindow. Call after window creation. */
export async function attach(win: import("electron").BrowserWindow): Promise<void> {
  if (!isAvailable()) return;
  try {
    const { WebContentsView } = await import("electron");
    await fs.mkdir(BROWSER_PROFILE_DIR, { recursive: true });
    view = new WebContentsView({
      webPreferences: {
        preload: path.join(path.dirname(fileURLToPathSafe()), "preload.cjs"),
        contextIsolation: true,
        nodeIntegration: false,
      },
    });
    hostWin = win;
    // initially hidden (positioned off-screen); shown when the UI opens the browser panel
    view.setBounds({ x: -9999, y: 0, width: 1, height: 1 });
    win.contentView.addChildView(view);
    view.webContents.loadURL("about:blank");
  } catch (e) {
    console.error("dotz browser: attach failed", e);
  }
}

/** Show the browser view at a position/size within the host window. */
export function show(bounds: { x: number; y: number; width: number; height: number }): void {
  if (!view || !hostWin) return;
  view.setBounds(bounds);
}

/** Hide the browser view (move off-screen). */
export function hide(): void {
  if (!view) return;
  view.setBounds({ x: -9999, y: 0, width: 1, height: 1 });
}

/** Navigate to a URL. */
export async function navigate(url: string): Promise<{ ok: boolean; url: string; title?: string }> {
  if (!view) return { ok: false, url, title: "browser not available (dev mode)" };
  try {
    currentUrl = url;
    await view.webContents.loadURL(url);
    return { ok: true, url, title: view.webContents.getTitle() };
  } catch (e) {
    return { ok: false, url, title: (e as Error).message };
  }
}

/** Go back. */
export function back(): { ok: boolean } {
  if (!view) return { ok: false };
  try { view.webContents.goBack(); return { ok: true }; } catch { return { ok: false }; }
}

/** Go forward. */
export function forward(): { ok: boolean } {
  if (!view) return { ok: false };
  try { view.webContents.goForward(); return { ok: true }; } catch { return { ok: false }; }
}

/** Reload. */
export function reload(): { ok: boolean } {
  if (!view) return { ok: false };
  try { view.webContents.reload(); return { ok: true }; } catch { return { ok: false }; }
}

/** Get the current URL + title. */
export function state(): { url: string; title: string; available: boolean } {
  if (!view) return { url: currentUrl, title: "", available: false };
  return { url: view.webContents.getURL(), title: view.webContents.getTitle(), available: true };
}

/** Capture a screenshot (returns a data URL). */
export async function screenshot(): Promise<string> {
  if (!view) return "";
  try {
    const img = await view.webContents.capturePage();
    return img.toDataURL();
  } catch {
    return "";
  }
}

/** Evaluate JS in the browser page (returns the result as a string). */
export async function evalJs(js: string): Promise<string> {
  if (!view) return "browser not available";
  try {
    const result = await view.webContents.executeJavaScript(js, true);
    return typeof result === "string" ? result : JSON.stringify(result);
  } catch (e) {
    return `eval error: ${(e as Error).message}`;
  }
}

/** Detach + clean up on window close. */
export function detach(): void {
  if (view && hostWin) {
    try { hostWin.contentView.removeChildView(view); } catch {}
  }
  view = null;
  hostWin = null;
}

// helper to safely get the directory (avoids import.meta.url issues in CJS preload context)
function fileURLToPathSafe(): string {
  try {
    const { fileURLToPath } = require("node:url");
    return path.dirname(fileURLToPath(import.meta.url));
  } catch {
    return __dirname;
  }
}