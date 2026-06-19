/** Minimal preload — contextIsolation is on; the UI talks to the backend over HTTP/WS,
 *  so we expose only a tiny info bridge and updater controls. */
import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("dotz", {
  electron: true,
  version: process.env.npm_package_version || "0.2.0",
  // Source-rebuild updater controls exposed to the renderer.
  update: {
    check: () => ipcRenderer.send("dotz-update-check"),
    apply: () => ipcRenderer.send("dotz-update-apply"),
    onStatus: (cb: (status: string, data?: any) => void) => {
      const handler = (_event: any, status: string, data?: any) => cb(status, data);
      ipcRenderer.on("dotz-update-status", handler);
      return () => ipcRenderer.removeListener("dotz-update-status", handler);
    },
  },
});
