/** Minimal preload — contextIsolation is on; the UI talks to the backend over HTTP/WS,
 *  so we expose only a tiny info bridge. */
import { contextBridge } from "electron";

contextBridge.exposeInMainWorld("dotz", {
  electron: true,
  version: process.env.npm_package_version || "0.1.0",
});
