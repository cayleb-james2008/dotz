//! ONNX sentence-embeddings (all-MiniLM-L6-v2, 384-dim) via ort — replaces transformers.js.
//! Mask-weighted mean-pool over the token dimension + L2-normalize, matching embedder.ts
//! ({pooling:"mean", normalize:true}). Empty string -> " " (matches `text || " "`).
use ort::session::Session;
use ort::value::Tensor;
use std::path::PathBuf;
use tokenizers::Tokenizer;

pub const EMBED_DIM: usize = 384;

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
}

/// assets/models root: DOTZ_MODELS (the models dir) | DOTZ_ASSETS/models | <cwd>/assets/models.
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
    PathBuf::from("assets").join("models")
}

impl Embedder {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let base = models_root().join("Xenova").join("all-MiniLM-L6-v2");
        let tokenizer = Tokenizer::from_file(base.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer: {e}"))?;
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
        for sp in 0..seq {
            let m = mask[sp] as f32;
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
