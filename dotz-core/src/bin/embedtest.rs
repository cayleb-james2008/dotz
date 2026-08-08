// Throwaway: verify the Rust ort embedder matches the transformers.js reference (js_embeddings.json).
use dotz_core::embed::Embedder;

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for i in 0..a.len().min(b.len()) {
        d += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    d / (na.sqrt() * nb.sqrt()).max(1e-9)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut e = Embedder::load()?;
    let raw = std::fs::read_to_string(
        std::env::var("DOTZ_EMBED_FIXTURE")
            .unwrap_or_else(|_| "fixtures/js_embeddings.json".to_string()),
    )?;
    let j: serde_json::Value = serde_json::from_str(&raw)?;
    let texts: Vec<String> = j["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let refs: Vec<Vec<f32>> = j["embeddings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap() as f32)
                .collect()
        })
        .collect();
    let mut minc = 1.0f32;
    for (i, t) in texts.iter().enumerate() {
        let v = e.embed(t)?;
        let c = cosine(&v, &refs[i]);
        if c < minc {
            minc = c;
        }
        println!("[{:.6}] {:?}", c, t.chars().take(42).collect::<String>());
    }
    println!(
        "MIN cosine vs transformers.js: {:.6}  ({})",
        minc,
        if minc > 0.999 { "PASS >0.999" } else { "FAIL" }
    );
    Ok(())
}
