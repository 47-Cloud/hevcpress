# hevcpress

Lossless recompression of iPhone HEIC photos and Live Photo `.mov` files.

These files are mostly HEVC video data, which generic compressors (xz, zstd,
brotli) barely shrink (about 3.6% on the sample files). `hevcpress` parses the
HEVC bitstream down to its syntax elements, re-codes them with a much stronger
context-mixing model, and can rebuild the original file **bit for bit**.

## Usage

```
cargo build --release
hevcpress compress   in.mov  in.mov.hvp     # verifies the round trip before writing
hevcpress decompress in.mov.hvp  out.mov    # byte-identical to in.mov
hevcpress verify     file...                # compress + check, print sizes
hevcpress inspect    file...                # NAL / slice-type byte breakdown
hevcpress syntax [-v] [--cold] file...      # bits per syntax element, CABAC vs model
```

## Results (six sample files, 13,271,358 bytes)

| file | original | hevcpress | saving |
|---|---|---|---|
| IMG_0029.HEIC | 2,201,957 | 2,023,212 | 8.1% |
| IMG_0133.HEIC | 1,435,563 | 1,307,651 | 8.9% |
| IMG_0283.HEIC | 1,556,848 | 1,424,972 | 8.5% |
| IMG_0029.mov | 2,884,020 | 2,595,562 | 10.0% |
| IMG_0133.mov | 3,989,550 | 3,461,379 | 13.2% |
| IMG_0283.mov | 1,203,420 | 1,040,825 | 13.5% |
| **total** | 13,271,358 | 11,853,601 | **10.7%** |

For comparison, xz -9e on the same files saves 3.6%. Every file round-trips
byte-exact; `compress` checks this itself and falls back to plain xz (or
stores the bytes) if anything does not verify.

## How it works

1. **Container** (`bmff.rs`): finds the HEVC samples in `.mov` / HEIF files, and
   the PCM audio track.
2. **Parser** (`ctu.rs`, `ps.rs`): a full HEVC slice-data parser (I/P/B slices,
   wavefront parallel processing, 4:2:0 and 4:0:0, 8/10 bit). It reads the
   CABAC-coded bins but never reconstructs pixels.
3. **Model** (`model.rs`): each bin is predicted by mixing several hashed
   contexts (neighbouring blocks, coefficient neighbourhoods, QP, intra modes,
   ...). Integer arithmetic only, with hard-coded tables (`tables.rs`), so the
   decoder reproduces probabilities exactly on any machine.
4. **Round trip** (`cabac_enc.rs`, `codec.rs`): the decompressor decodes bins
   from the model stream and re-encodes them with a CABAC encoder to regenerate
   the original slice bytes. During compression every slice is replayed through
   that encoder and compared with the original; a slice that does not
   reproduce exactly is simply left untouched in the file.
5. **Audio** (`audio.rs`): PCM audio gets a cascaded NLMS predictor, coded with
   the same model. It is about 15-27% smaller than FLAC -8 on the samples.
6. Everything else (container boxes, parameter sets, slice headers, metadata
   tracks) is stored in an xz block.

## Safety

- The model must never use floating point or anything platform dependent:
  a different probability in the decompressor corrupts the file silently.
- `compress` decompresses its own output and compares it with the input.
- Unsupported or malformed input (tiles, PCM coding units, 4:2:2/4:4:4,
  truncated files, non-HEVC data) falls back to xz.

## Not supported (falls back)

Tiles, PCM coding units, 4:2:2 / 4:4:4 chroma, extended-precision and
bypass-alignment profiles, cross-component prediction.

## Speed

Compression takes roughly 30-100 s per file (several parse passes plus the
verification decompress); decompression about a third of that.
