---
description: Write tests for a module or feature - TDD, edge cases, coverage of the public surface. Use for "test X", "write tests for X", "add coverage to X".
---
Write tests for: $@

TDD where it fits; edge cases always; cover the public surface, not the implementation.

1. SURFACE — `read` the target module and list its public functions/types. Those are the contract; tests pin the contract, not the lines.
2. CASES — for each public symbol, enumerate: the happy path, the empty/null/zero input, the max-length/overflow input, the unauthorized/error path, and any concurrency boundary. Use `memory_search` for known-bad inputs that bit this symbol before.
3. WRITE — one test per case. Name the test after the behavior it pins (`embed_returns_384_dim_normalized_vectors`), not after the function (`embed_test1`).
4. VERIFY — run `cargo test -p dotz-core -- --test-threads=2` (or the project's native test command). Every new test must pass.
5. COVERAGE — if a branch has no test, add one or delete the branch. A branch with no test is a branch with no contract.

Rules:
- For TDD on new code: write the failing test first, watch it fail, then implement to make it pass.
- For existing code: characterize the current behavior first (a test that passes against today's code), then decide whether the behavior is correct or a bug.
- Never delete a test to make the build green. A deleted test is a regression you have already shipped.
- Tests that need the bundled ONNX model are gated (see `embed.rs` tests); do not ungate them.

Report: the files tested, the test count added, and fresh gate output.