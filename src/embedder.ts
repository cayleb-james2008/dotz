/**
 * dotz local embedder — bundled transformers.js (ONNX) sentence-embeddings, so mem0's vector
 * memory works fully ON-DEVICE with no embeddings API. Model: all-MiniLM-L6-v2 (384-dim).
 *
 * This is injected directly into mem0's `Memory` in-process (`memory.embedder = localEmbedder`),
 * so there is NO embeddings HTTP route, no port coupling, and no second process — the leanest
 * way to give mem0 embeddings (mem0-ts has no custom-embedder provider, only an in-process
 * field override). Implements mem0's `Embedder` interface ({ embed, embedBatch }).
 *
 * Offline-first: if a bundled model directory exists (assets/models/, shipped in the exe) it is
 * used with remote fetches disabled; otherwise (dev / first run) downloads are cached under
 * ~/.dotz/models-cache so the packaged app can reuse them.
 */
import path from "node:path";
import os from "node:os";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

/** The bundled embedding model + its vector dimension. mem0's vector store is sized to this. */
export const EMBED_MODEL = "Xenova/all-MiniLM-L6-v2";
export const EMBED_DIM = 384;

/** mem0's Embedder contract (kept local so we don't depend on mem0's types at module load). */
export interface Embedder {
  embed(text: string): Promise<number[]>;
  embedBatch(texts: string[]): Promise<number[][]>;
}

type FeatureTensor = { tolist(): number[][]; data: ArrayLike<number> };
type FeaturePipe = (input: string | string[], opts: { pooling: "mean"; normalize: boolean }) => Promise<FeatureTensor>;

// transformers.js pulls onnxruntime (heavy) — load it lazily on first embed.
let pipePromise: Promise<FeaturePipe> | null = null;
// Serialize inference — a single ONNX session is safest run sequentially.
let chain: Promise<unknown> = Promise.resolve();

function modelsRoot(): string {
  // assets/ ships alongside dist/ in the packaged app (electron-builder files), and resolves to
  // the repo root in dev.
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "assets", "models");
}

async function getPipe(): Promise<FeaturePipe> {
  if (!pipePromise) {
    pipePromise = (async () => {
      const tf = (await import("@huggingface/transformers")) as unknown as {
        pipeline: (task: string, model: string) => Promise<FeaturePipe>;
        env: Record<string, unknown>;
      };
      const bundled = modelsRoot();
      if (existsSync(path.join(bundled, ...EMBED_MODEL.split("/")))) {
        // Packaged: load the bundled ONNX model and forbid any network fetch.
        tf.env.localModelPath = bundled;
        tf.env.allowRemoteModels = false;
      } else {
        // Dev / first run: cache downloads under ~/.dotz so the exe build can pick them up.
        tf.env.cacheDir = path.join(process.env.DOTZ_CONFIG_DIR || path.join(os.homedir(), ".dotz"), "models-cache");
      }
      return tf.pipeline("feature-extraction", EMBED_MODEL);
    })();
  }
  return pipePromise;
}

function run<T>(fn: () => Promise<T>): Promise<T> {
  const next = chain.then(fn, fn);
  chain = next.then(() => undefined, () => undefined);
  return next;
}

export const localEmbedder: Embedder = {
  async embed(text: string): Promise<number[]> {
    return run(async () => {
      const pipe = await getPipe();
      const out = await pipe(text || " ", { pooling: "mean", normalize: true });
      return Array.from(out.data as ArrayLike<number>);
    });
  },
  async embedBatch(texts: string[]): Promise<number[][]> {
    if (texts.length === 0) return [];
    return run(async () => {
      const pipe = await getPipe();
      const out = await pipe(texts.map((t) => t || " "), { pooling: "mean", normalize: true });
      return out.tolist();
    });
  },
};
