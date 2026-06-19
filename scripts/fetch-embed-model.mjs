/**
 * Build step: download the bundled embedding model into assets/models/ so the packaged exe runs
 * the local embedder fully OFFLINE. The runtime embedder (src/embedder.ts) loads from this dir with
 * remote fetches disabled when it exists. cacheDir layout (<dir>/<owner>/<model>/...) matches the
 * localModelPath layout the runtime expects, so this is a straight download-into-place.
 *
 *   node scripts/fetch-embed-model.mjs   (run by `npm run fetch-model`, chained into `npm run dist`)
 */
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const MODEL = "Xenova/all-MiniLM-L6-v2";
const dest = path.join(root, "assets", "models");

const { pipeline, env } = await import("@huggingface/transformers");
env.cacheDir = dest;
env.allowRemoteModels = true;

console.log(`[fetch-embed-model] downloading ${MODEL} → ${dest}`);
const pipe = await pipeline("feature-extraction", MODEL);
const out = await pipe("warm up the embedder", { pooling: "mean", normalize: true });
if (out.data.length !== 384) { console.error(`[fetch-embed-model] unexpected dim ${out.data.length}`); process.exit(1); }
console.log(`[fetch-embed-model] OK — ${MODEL} bundled (dim ${out.data.length}).`);
