/**
 * dotz Electron main — boots the embedded pi-agent server in-process, then opens a native
 * window pointed at it. The window loads the same web/ dashboard served over localhost, so
 * the UI is identical in the browser (dev) and the packaged app.
 */
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { app, BrowserWindow, shell, ipcMain, dialog } from "electron";
import { buildServer } from "./server";
import { sandbox } from "./sandbox";
import { browserController } from "./browser";
import { checkForUpdatesOnLaunch, wireUpdaterIpc } from "./updater";

const HOST = "127.0.0.1";
const PORT = Number(process.env.DOTZ_PORT || 4317);
const DIR = path.dirname(fileURLToPath(import.meta.url));

let runtime: Awaited<ReturnType<typeof buildServer>> | null = null;

// Harden: an unhandled promise rejection in the Electron main process (e.g. from the pi SDK,
// sandbox, or browser controller) would crash the app by default (Node v15+). Log prominently so
// bugs are visible but don't take down the desktop app.
process.on("unhandledRejection", (reason) => {
  console.error("dotz: unhandled rejection:", reason);
});
// Same for synchronous uncaught exceptions — a throw in any callback (EventEmitter, child_process,
// IPC handler) would crash the app. Log but don't exit so a single bad callback doesn't take down
// the entire desktop app.
process.on("uncaughtException", (err) => {
  console.error("dotz: uncaught exception:", err);
});

async function startServer() {
  // In the packaged app, run agent sessions against the user's real cwd; default to home.
  runtime = await buildServer();
  try {
    await runtime.app.listen({ host: HOST, port: PORT });
  } catch (err) {
    const code = (err as { code?: string }).code;
    if (code === "EADDRINUSE") {
      console.error(`dotz: port ${PORT} is already in use — another dotz instance may be running. Set DOTZ_PORT to use a different port.`);
    }
    throw err;
  }
}

function createWindow() {
  const win = new BrowserWindow({
    width: 1480,
    height: 920,
    minWidth: 1024,
    minHeight: 640,
    backgroundColor: "#1e1e2e",
    title: "dotz",
    autoHideMenuBar: true,
    webPreferences: {
      preload: path.join(DIR, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  win.removeMenu();
  win.webContents.on("did-fail-load", (_e, code, desc, url) => {
    if (code === -3) return; // ABORTED — ignore benign in-page navigations
    win.loadURL("data:text/html," + encodeURIComponent(
      `<body style="background:#1e1e2e;color:#f38ba8;font-family:monospace;padding:40px">` +
      `<h2>dotz could not reach its backend</h2>` +
      `<p>${desc} (${url}). The local agent server may not be running. Relaunch dotz.</p></body>`));
  });
  win.loadURL(`http://${HOST}:${PORT}`);
  // External links open in the system browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:/.test(url)) shell.openExternal(url);
    return { action: "deny" };
  });
  // In-window navigation is locked to the app origin; anything else is blocked, and
  // http(s) targets are handed to the system browser instead of navigating in-app.
  win.webContents.on("will-navigate", (e, url) => {
    if (!url.startsWith(`http://${HOST}:${PORT}`)) {
      e.preventDefault();
      if (/^https?:/.test(url)) shell.openExternal(url);
    }
  });
  return win;
}

// Single-instance lock: a second launch of dotz must not spawn a duplicate window or a
// second server that collides on the port. Hand focus to the existing window instead.
const gotTheLock = app.requestSingleInstanceLock();
if (!gotTheLock) {
  app.quit();
} else {
  app.on("second-instance", () => {
    const existing = BrowserWindow.getAllWindows()[0];
    if (existing) {
      if (existing.isMinimized()) existing.restore();
      existing.focus();
    }
  });
}

if (gotTheLock) app.whenReady().then(async () => {
  wireUpdaterIpc();
  // Native directory picker for the new-project form (preload exposes window.dotz.pickDirectory).
  // Returns the chosen absolute path or null — so the user selects a real workspace via File
  // Explorer instead of typing a path that could be relative or nonexistent.
  ipcMain.handle("dotz:pick-directory", async () => {
    const win = BrowserWindow.getFocusedWindow() ?? BrowserWindow.getAllWindows()[0] ?? null;
    const opts = { properties: ["openDirectory" as const], title: "Select project workspace" };
    const res = win ? await dialog.showOpenDialog(win, opts) : await dialog.showOpenDialog(opts);
    return res.canceled || res.filePaths.length === 0 ? null : res.filePaths[0];
  });
  // Start the server before opening the window so the renderer has a live backend.
  let serverOk = true;
  let serverErr: unknown = null;
  try {
    await startServer();
  } catch (e) {
    serverOk = false;
    serverErr = e;
    console.error("dotz: server failed to start", e);
  }
  const win = createWindow();
  if (!serverOk) {
    const isPortInUse = serverErr && (serverErr as { code?: string }).code === "EADDRINUSE";
    const portMsg = isPortInUse
      ? `Port ${PORT} is already in use by another process (likely another dotz instance). Close it, or set DOTZ_PORT to use a different port.`
      : `The local agent server could not start (port ${PORT} may be in use by another process).`;
    win.loadURL("data:text/html," + encodeURIComponent(
      `<body style="background:#1e1e2e;color:#f38ba8;font-family:monospace;padding:40px">` +
      `<h2>dotz failed to start its backend</h2>` +
      `<p>${portMsg} Close any other instance using that port and relaunch dotz.</p>`));
  }
  // Background git check after the window is ready. If the local checkout is behind
  // origin, the renderer shows the UPDATE AVAILABLE card; the user chooses UPDATE & RESTART
  // (git pull + portable rebuild + relaunch) via the source-rebuild updater.
  checkForUpdatesOnLaunch(win).catch((e) => console.error("dotz: update check failed", e));
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

// Quit cleanly: browserController.disposeAll() is async (spawns `agent-browser close` and rm's the
// throwaway mkdtemp profile dirs), so gate the quit on before-quit and await it before exiting —
// a bare app.quit() tears down the event loop first and leaks those profile dirs in %TEMP%.
let quitting = false;
app.on("before-quit", (e) => {
  if (quitting) return;
  e.preventDefault();
  quitting = true;
  sandbox.disposeAll();
  if (runtime) runtime.pi.disposeAll();
  // Close the Fastify server so its onClose hook runs — that disposes connectionsController
  // (active CLI login subprocesses: gh/vercel/neonctl) AND browserController (browser sessions
  // + stray-process reaping) AND unsubscribes memory/browser broadcast listeners. Without
  // app.close(), connectionsController.disposeAll() was NEVER called, leaking login processes.
  // browserController.disposeAll() is idempotent (second call is a no-op after sessions map empties).
  // Timeout backstop: if app.close() hangs (e.g. an agent-browser `close` command stalls), force
  // exit after 8s so the app never wedges in a non-quit state — the OS reaps any orphaned children.
  const serverClosed = runtime ? runtime.app.close().catch(() => undefined) : Promise.resolve();
  const forceExit = setTimeout(() => app.exit(0), 8_000);
  serverClosed.finally(() => { clearTimeout(forceExit); app.exit(0); });
});
app.on("window-all-closed", () => app.quit());
