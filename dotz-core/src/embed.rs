//! ONNX sentence-embeddings (all-MiniLM-L6-v2, 384-dim) via ort — replaces transformers.js.
//! Mask-weighted mean-pool over the token dimension + L2-normalize, matching embedder.ts
//! ({pooling:"mean", normalize:true}). Empty string -> " " (matches `text || " "`).
use ort::session::Session;
use ort::value::Tensor;
use std::path::PathBuf;
use tokenizers::utils::truncation::TruncationParams;
use tokenizers::Tokenizer;

pub const EMBED_DIM: usize = 384;
/// all-MiniLM-L6-v2 max position embeddings. Truncate at the tokenizer so the ONNX session never
/// receives a sequence longer than the model can attend to, which would otherwise silently corrupt
/// embeddings or fail downstream memory search/recall.
const MAX_SEQ_LENGTH: usize = 512;

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
}

/// True when the bundled ONNX model files are present at the resolved models root. Used by the
/// health endpoint so operators can see at a glance whether memory / recall (which depend on the
/// `ort` embedder) are going to work.
pub fn model_files_present() -> bool {
    let base = models_root().join("Xenova").join("all-MiniLM-L6-v2");
    base.join("tokenizer.json").exists() && base.join("onnx").join("model.onnx").exists()
}

/// assets/models root: DOTZ_MODELS (the models dir) | DOTZ_ASSETS/models | <cwd>/assets/models |
/// <workspace>/assets/models derived from the crate manifest dir. The manifest fallback makes
/// tests and binaries runnable from the `dotz-core` crate dir as well as the workspace root.
fn models_root() -> PathBuf {
    if let Ok(d) = std::env::var("DOTZ_MODELS") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(d) = std::env::var("DOTZ_ASSETS") {
        if !d.is_empty() {
            return PathBuf::from(d).join("models");
        }
    }
    // Cargo sets CARGO_MANIFEST_DIR to the crate root (dotz-core). Fall back to the workspace
    // root (one level up) so tests/binaries work regardless of the current working directory.
    let manifest_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_root.parent().unwrap_or(&manifest_root);
    for candidate in [
        PathBuf::from("assets").join("models"),
        manifest_root.join("assets").join("models"),
        workspace_root.join("assets").join("models"),
    ] {
        if candidate
            .join("Xenova")
            .join("all-MiniLM-L6-v2")
            .join("tokenizer.json")
            .exists()
        {
            return candidate;
        }
    }
    // Default to cwd-relative when nothing matches so load() still reports the expected error.
    PathBuf::from("assets").join("models")
}

impl Embedder {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let base = models_root().join("Xenova").join("all-MiniLM-L6-v2");
        let mut tokenizer = Tokenizer::from_file(base.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer: {e}"))?;
        // Enforce the model's context limit regardless of the bundled tokenizer config. A
        // malformed or oversized memory text must not crash the embedder or the memory store.
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_SEQ_LENGTH,
                ..Default::default()
            }))
            .map_err(|e| format!("tokenizer truncation: {e}"))?;
        let session = Session::builder()?.commit_from_file(base.join("onnx").join("model.onnx"))?;
        Ok(Self { session, tokenizer })
    }

    /// Embed one string -> 384-dim L2-normalized vector.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let s = if text.is_empty() { " " } else { text };
        let enc = self
            .tokenizer
            .encode(s, true)
            .map_err(|e| format!("encode: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
        let seq = ids.len();
        let types = vec![0i64; seq];

        let outputs = self.session.run(ort::inputs![
            "input_ids" => Tensor::from_array(([1_usize, seq], ids))?,
            "attention_mask" => Tensor::from_array(([1_usize, seq], mask.clone()))?,
            "token_type_ids" => Tensor::from_array(([1_usize, seq], types))?,
        ])?;
        let (_shape, data) = outputs["last_hidden_state"].try_extract_tensor::<f32>()?;

        // mask-weighted mean-pool over the seq dimension; last_hidden_state is [1, seq, EMBED_DIM].
        let mut pooled = vec![0f32; EMBED_DIM];
        let mut msum = 0f32;
        for (sp, &mask_val) in mask.iter().enumerate().take(seq) {
            let m = mask_val as f32;
            msum += m;
            let off = sp * EMBED_DIM;
            for h in 0..EMBED_DIM {
                pooled[h] += data[off + h] * m;
            }
        }
        if msum > 0.0 {
            for v in pooled.iter_mut() {
                *v /= msum;
            }
        }
        let norm = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in pooled.iter_mut() {
                *v /= norm;
            }
        }
        Ok(pooled)
    }

    /// Embed many — one at a time (per-row results are identical to a padded batch; mask handles it).
    pub fn embed_batch(
        &mut self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    // Serialize tests that mutate the process-global `DOTZ_MODELS` env var so they do not race
    // with the shared embedder initialization or with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // The ONNX Session + Tokenizer are not cheap to create; share one instance across tests and
    // serialize on it so parallel test runners do not load the model multiple times.
    static EMBEDDER: OnceLock<Mutex<Embedder>> = OnceLock::new();

    fn shared() -> std::sync::MutexGuard<'static, Embedder> {
        EMBEDDER
            .get_or_init(|| {
                Mutex::new(
                    Embedder::load().expect("the bundled all-MiniLM-L6-v2 ONNX model should load"),
                )
            })
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cosine(a: &[f32], b: &[f32]) -> f64 {
        let n = a.len().min(b.len());
        let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
        for i in 0..n {
            d += a[i] as f64 * b[i] as f64;
            na += a[i] as f64 * a[i] as f64;
            nb += b[i] as f64 * b[i] as f64;
        }
        let den = na.sqrt() * nb.sqrt();
        if den == 0.0 {
            d
        } else {
            d / den
        }
    }

    fn l2_norm(v: &[f32]) -> f64 {
        v.iter().map(|x| *x as f64 * *x as f64).sum::<f64>().sqrt()
    }

    #[test]
    fn embedder_loads_and_produces_384_dim_normalized_vectors() {
        let mut e = shared();
        let v = e.embed("cargo test -p dotz-core").unwrap();
        assert_eq!(v.len(), EMBED_DIM, "expected {EMBED_DIM}-dim embeddings");
        let norm = l2_norm(&v);
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "embeddings must be L2-normalized, got norm {norm}"
        );
    }

    #[test]
    fn identical_texts_have_cosine_one() {
        let mut e = shared();
        let a = e.embed("dotz uses ort for local ONNX embeddings").unwrap();
        let b = e.embed("dotz uses ort for local ONNX embeddings").unwrap();
        let c = cosine(&a, &b);
        assert!(
            (c - 1.0).abs() < 1e-5,
            "identical texts should have cosine ~1, got {c}"
        );
    }

    #[test]
    fn different_texts_have_lower_similarity() {
        let mut e = shared();
        let a = e.embed("machine learning").unwrap();
        let b = e.embed("freshly baked sourdough bread").unwrap();
        let c = cosine(&a, &b);
        assert!(
            c < 0.95,
            "unrelated texts should not be near-identical, got cosine {c}"
        );
    }

    #[test]
    fn embed_batch_matches_individual_embeddings() {
        let mut e = shared();
        let texts = ["first sentence", "second sentence"];
        let batch = e.embed_batch(&texts).unwrap();
        assert_eq!(batch.len(), 2);
        for (i, text) in texts.iter().enumerate() {
            let single = e.embed(text).unwrap();
            assert_eq!(
                batch[i].len(),
                single.len(),
                "batch row {i} must have the same dimension as the single embedding"
            );
            let c = cosine(&batch[i], &single);
            assert!(
                (c - 1.0).abs() < 1e-5,
                "batch embedding {i} must match the individual embedding, got cosine {c}"
            );
        }
    }

    /// A memory text longer than the model's 512-token context limit must embed without error and
    /// still produce a 384-dim L2-normalized vector. Without explicit tokenizer truncation, an
    /// oversized input can silently corrupt embeddings or fail the ONNX inference.
    #[test]
    fn long_input_embeds_without_error_and_stays_normalized() {
        let mut e = shared();
        let long = "dotz ".repeat(2000);
        let v = e.embed(&long).unwrap();
        assert_eq!(v.len(), EMBED_DIM);
        let norm = l2_norm(&v);
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "long input embedding must stay L2-normalized, got norm {norm}"
        );
    }

    /// `model_files_present` must reflect whether the tokenizer + ONNX files exist at the resolved
    /// models root. When the bundled model is present (the normal case) it is true; with an
    /// intentionally bogus DOTZ_MODELS override it is false.
    #[test]
    fn model_files_present_matches_disk_state() {
        let _guard = ENV_LOCK.lock().unwrap();

        // Normal workspace/crate layout: the bundled model should be discoverable.
        assert!(
            model_files_present(),
            "bundled all-MiniLM-L6-v2 files should be present"
        );

        let prev = std::env::var("DOTZ_MODELS").ok();
        let tmp = std::env::temp_dir().join(format!("dotz-embed-missing-{}", uuid::Uuid::new_v4()));
        std::env::set_var("DOTZ_MODELS", &tmp);
        assert!(
            !model_files_present(),
            "bogus DOTZ_MODELS dir should report embedder not ready"
        );
        match prev {
            Some(p) => std::env::set_var("DOTZ_MODELS", p),
            None => std::env::remove_var("DOTZ_MODELS"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
