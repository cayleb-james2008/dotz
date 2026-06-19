/**
 * dotz auto-updater — checks a remote release feed on every app launch and, if a newer
 * version is available, downloads it in the background and installs the update before the
 * app fully starts. Designed for the portable Windows .exe produced by electron-builder.
 *
 * The updater reads `package.json` for the current version and the configured update feed.
 * The feed is a plain JSON file hosted at `UPDATE_FEED_URL` (override with DOTZ_UPDATE_URL)
 * with shape:
 *   {
 *     "version": "0.2.0",
 *     "url": "https://example.com/dotz-0.2.0.exe",
 *     "notes": "...",
 *     "mandatory": false
 *   }
 *
 * For electron-builder NSIS/portable targets, the built-in `electron-updater` module is the
 * most reliable path. We wrap it so that:
 *   - update checks happen on launch,
 *   - download + install is silent unless an error occurs,
 *   - a manual check is exposed via the tray / menu (not yet implemented),
 *   - dev builds never auto-install.
 */
import process from "node:process";
import path from "node:path";
import fs from "node:fs";
import { app, dialog } from "electron";
import { autoUpdater } from "electron-updater";

const UPDATE_FEED = process.env.DOTZ_UPDATE_URL || "";
const IS_DEV = !app.isPackaged;
const IS_PORTABLE = process.env.PORTABLE_EXECUTABLE_DIR != null;

let checking = false;

/** Resolve the update server config from env/package.json. */
function resolveFeed(): string | undefined {
  if (UPDATE_FEED) return UPDATE_FEED;
  try {
    // Load package.json from the packaged app root. Electron sets process.resourcesPath in prod.
    const pkgPath = process.resourcesPath
      ? path.join(process.resourcesPath, "app", "package.json")
      : path.resolve("./package.json");
    const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf-8"));
    const pub = pkg.build?.publish;
    if (typeof pub === "string") return pub;
    if (pub && pub.url) return pub.url;
    if (Array.isArray(pub)) {
      const first = pub.find((p: any) => p.url);
      if (first) return first.url;
    }
  } catch {
    /* package.json may not be resolvable in esbuild bundle */
  }
  return undefined;
}

/** Return the current app version, falling back to package.json in dev. */
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

/** Check for updates and, if found, download + install automatically. */
export async function checkForUpdatesOnLaunch(): Promise<void> {
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
  try {
    autoUpdater.setFeedURL({ url: feed, provider: "generic" as any });
    autoUpdater.autoDownload = true;
    autoUpdater.autoInstallOnAppQuit = true;

    autoUpdater.on("update-available", (info) => {
      console.log("dotz updater: update available", info.version);
    });
    autoUpdater.on("update-not-available", () => {
      console.log("dotz updater: up to date");
    });
    autoUpdater.on("download-progress", (p) => {
      console.log(`dotz updater: download ${Math.round(p.percent)}%`);
    });
    autoUpdater.on("update-downloaded", (info) => {
      console.log("dotz updater: downloaded", info.version);
      // For portable builds, quitAndInstall immediately so the next launch runs the new exe.
      // For installed builds, let it install on quit.
      if (IS_PORTABLE) {
        setImmediate(() => autoUpdater.quitAndInstall(true, true));
      }
    });
    autoUpdater.on("error", (err) => {
      console.error("dotz updater: error", err.message);
    });

    await autoUpdater.checkForUpdates();
  } finally {
    checking = false;
  }
}

/** Manual update check that shows UI feedback. */
export async function manualUpdateCheck(parentWindow?: Electron.BrowserWindow): Promise<void> {
  const opts: Partial<Electron.MessageBoxOptions> = {
    type: "info",
    title: "dotz updater",
  };
  const show = (message: string, type?: "info" | "warning") => {
    const box: Electron.MessageBoxOptions = { ...opts, type: type || "info", message };
    if (parentWindow) parentWindow.webContents.send("show-message-box", box);
    else dialog.showMessageBox(box);
  };
  if (IS_DEV) {
    show("Updates are not checked in development builds.");
    return;
  }
  const feed = resolveFeed();
  if (!feed) {
    show("No update feed is configured. Set DOTZ_UPDATE_URL or package.json build.publish.", "warning");
    return;
  }
  try {
    autoUpdater.setFeedURL({ url: feed, provider: "generic" as any });
    const result = await autoUpdater.checkForUpdates();
    if (!result || result.updateInfo.version === app.getVersion()) {
      show(`dotz ${app.getVersion()} is up to date.`);
    } else {
      show(`Update ${result.updateInfo.version} available. It will download and install automatically.`);
    }
  } catch (e) {
    dialog.showErrorBox("dotz updater", (e as Error).message);
  }
}
