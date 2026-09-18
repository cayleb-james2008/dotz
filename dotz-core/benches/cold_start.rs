//! Cold-start benchmark — measures `Embedder::load()`, the dominant dotz cold-start cost.
//!
//! The ONNX session + tokenizer bring-up is what gates memory/recall on first launch; everything
//! else (axum bind, route table) is sub-millisecond. A black-box server-spawn bench was considered
//! and rejected as ponytail overkill for CI: it would need `npm run fetch-model` in the gate and a
//! free-port poll, and it would not catch a regression this micro-bench misses.
//!
//! Run: `cargo bench --bench cold_start`.
//! Save a baseline on a clean main: `cargo bench --bench cold_start -- --save-baseline main`.
//! CI records the number (non-gating); a comparison gate will be wired up once
//! `docs/perf-baseline.json` carries a real `cold_start_ms`.
//!
//! Skips gracefully when the bundled ONNX model is absent (`model_files_present() == false`) so the
//! gate stays green on a fresh checkout that hasn't run `npm run fetch-model` — the bench then
//! reports nothing instead of failing. `cargo bench --bench cold_start -- --no-run` still compiles
//! and is the CI compile-check.
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dotz_core::embed::{Embedder, model_files_present};

/// One cold-start: load the embedder from disk (tokenizer parse + ONNX session commit). This is the
/// exact path the server runs on first memory/recall use; the `Embedder` is dropped at the end of
/// each sample so each iteration pays the full cold cost (no session reuse across samples).
fn embedder_load_bench(c: &mut Criterion) {
    if !model_files_present() {
        // ponytail: ceiling = a CI runner without the bundled model (fresh checkout, no
        // `npm run fetch-model`). Upgrade path: wire `npm run fetch-model` into the bench step
        // when the comparison gate is activated. Until then, skip instead of failing the gate.
        eprintln!(
            "cold_start: bundled ONNX model not present — skipping (run `npm run fetch-model`)."
        );
        return;
    }
    let mut group = c.benchmark_group("cold_start");
    group.sample_size(10); // ONNX load is ~100s of ms; 10 samples keeps the bench under a minute
    group.bench_function(BenchmarkId::new("embedder_load", "all-MiniLM-L6-v2"), |b| {
        b.iter(|| {
            // Load and immediately drop so every sample pays the full cold cost. A panic here is a
            // real regression (the embedder is the memory/recall gate) — let it surface.
            let _embedder = Embedder::load().expect("cold_start: Embedder::load() must succeed");
        });
    });
    group.finish();
}

criterion_group!(benches, embedder_load_bench);
criterion_main!(benches);
