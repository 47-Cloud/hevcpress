# Contributing

Bug reports, test results and pull requests are all welcome.

## Building and testing

```
cargo build --release
cargo test --release
```

The unit tests don't need any media. To round-trip a folder of your own
photos and videos as well:

```
HEVCPRESS_SAMPLES=~/path/to/files cargo test --release --test samples -- --nocapture
```

CI runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`, so
it's worth running those before you push.

## Reporting a file that doesn't compress well

If `hevcpress verify` falls back to xz, or a file saves much less than usual,
the output of `hevcpress inspect file` and `hevcpress verify file` is usually
enough to see why. Please don't attach personal photos to an issue. If a file is
needed to reproduce a problem, say so and we'll work out a way to share it.

## Changing the model

Anything that changes what the model predicts (contexts, mixer, tables, the
order bins are coded in) changes the compressed format. If your change does:

- bump `FORMAT` in `src/codec.rs`, and
- check that `hevcpress verify` still round-trips the sample files.

The model must stay integer-only. Floating point, or anything else that could
differ between machines or compiler versions, would let the decompressor drift
from the compressor without any error until the checksum fails.

## Licence

Contributions are dual-licensed under MIT and Apache-2.0, the same as the
project, unless you say otherwise.
