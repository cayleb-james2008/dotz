/** Bundle dotz's own TS (main/server/pi) into dist/, leaving node_modules external so the
 *  pi SDK (ESM + native deps + jiti .ts extension loading) resolves at runtime. */
import { build } from "esbuild";

const common = {
  bundle: true,
  platform: "node",
  target: "node20",
  packages: "external", // resolve fastify / pi-coding-agent / electron from node_modules at runtime
  sourcemap: true,
  logLevel: "info",
};

await build({ ...common, entryPoints: ["src/main.ts"], outfile: "dist/main.js", format: "esm" });
await build({ ...common, entryPoints: ["src/preload.ts"], outfile: "dist/preload.cjs", format: "cjs" });

console.log("dotz: esbuild done → dist/main.js, dist/preload.cjs");
