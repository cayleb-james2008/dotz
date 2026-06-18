/**
 * dotz Electron main — boots the embedded pi-agent server in-process, then opens a native
 * window pointed at it. The window loads the same web/ dashboard served over localhost, so
 * the UI is identical in the browser (dev) and the packaged app.
 */
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { app, BrowserWindow, shell } from "electron";
import { buildServer } from "./server";
import { sandbox } from "./sandbox";
import * as browser from "./browser";

const HOST = "127.0.0.1";
const PORT = Number(process.env.DOTZ_PORT || 4317);
const DIR = path.dirname(fileURLToPath(import.meta.url));

let runtime: Awaited<ReturnType<typeof buildServer>> | null = null;

async function startServer() {
  // In the packaged app, run agent sessions against the user's real cwd; default to home.
  runtime = await buildServer();
  await runtime.app.listen({ host: HOST, port: PORT });
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
  win.loadURL(`http://${HOST}:${PORT}`);
  // External links open in the system browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:/.test(url)) shell.openExternal(url);
    return { action: "deny" };
  });
  // Attach the in-app browser view (hidden until the UI opens the browser panel).
  browser.attach(win).catch(() => {});
  win.on("closed", () => browser.detach());
}

app.whenReady().then(async () => {
  try {
    await startServer();
  } catch (e) {
    console.error("dotz: server failed to start", e);
  }
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  sandbox.disposeAll();
  browser.detach();
  if (runtime) runtime.pi.disposeAll();
  app.quit();
});
