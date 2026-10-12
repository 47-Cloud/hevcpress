**What this changes**

**How it was tested**

- [ ] `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass
- [ ] `cargo test --release` passes
- [ ] If the model or format changed: `FORMAT` bumped in `src/codec.rs` and
      `hevcpress verify` round-trips the sample files
