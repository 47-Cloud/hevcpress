# hevcpress

Lossless recompression of iPhone HEIC photos and Live Photo `.mov` files.

These files are mostly HEVC video data, which generic compressors (xz, zstd,
brotli) barely shrink (about 3.6% on the sample files). `hevcpress` parses the
HEVC bitstream down to its syntax elements, re-codes them with a much stronger
context-mixing model, and can rebuild the original file **bit for bit**.

## Usage

```
cargo build --release
hevcpress pack   [-j N] files...           # writes <file>.hvp for each, N files in parallel
hevcpress unpack [-j N] files.hvp...       # restores <file>, byte-identical
hevcpress compress   in.mov  in.mov.hvp     # single file
hevcpress decompress in.mov.hvp  out.mov
hevcpress verify     file...                # compress + check, print sizes
hevcpress inspect    file...                # NAL / slice-type byte breakdown
hevcpress syntax [-v] [--cold] file...      # bits per syntax element, CABAC vs model
```

`pack` and `unpack` never overwrite an existing file. Every compression checks
its own round trip before anything is written.

## Results

| file | original | hevcpress | saving | xz -9e |
|---|---|---|---|---|
| IMG_0029.HEIC | 2,201,957 | 2,023,212 | 8.1% | |
| IMG_0133.HEIC | 1,435,563 | 1,307,651 | 8.9% | |
| IMG_0283.HEIC | 1,556,848 | 1,424,972 | 8.5% | |
| IMG_9533.HEIC | 1,726,673 | 1,575,673 | 8.7% | 2.2% |
| IMG_0029.mov | 2,884,020 | 2,595,562 | 10.0% | |
| IMG_0133.mov | 3,989,550 | 3,461,379 | 13.2% | |
| IMG_0283.mov | 1,203,420 | 1,040,825 | 13.5% | |
| IMG_9533.mov | 4,288,293 | 3,809,629 | 11.2% | 1.6% |
| **total** | 19,286,324 | 17,238,903 | **10.6%** | |

xz -9e saves 3.6% on the first six files. IMG_9533 was added after the model
was tuned, so it is an out-of-sample check. Every file round-trips byte-exact;
`compress` checks this itself and falls back to plain xz (or stores the bytes)
if anything does not verify.

About 4% of each compressed HEIC is metadata the model leaves to xz. Most of it
is a binary plist with a grid of 16-bit floats (Apple tone-mapping data), which
is close to incompressible: a dedicated model would gain about 0.3%.

## Comparison with other compressors

Same eight files, each tool at its strongest practical setting, built from
current sources (2026-10). Savings are against the original files.

| tool | saving (all 8 files, 19.3 MB) |
|---|---|
| **hevcpress** | **10.62%** |
| zpaq -m5 | 3.38% |
| 7-Zip 23.01, LZMA2 -mx9 | 2.96% |
| xz -9e | 2.95% |
| brotli -q 11 -w 24 | 2.62% |
| zstd --ultra -22 --long=27 | 2.43% |
| 7-Zip 23.01, PPMd -mx9 | 0.42% |

The strongest general-purpose context mixers are too slow to run on the whole
set, so paq8px was measured on one file:

| IMG_0133.HEIC (1,435,563 bytes) | compressed | saving | time |
|---|---|---|---|
| **hevcpress** | 1,307,651 | **8.91%** | about 6.5 s |
| paq8px -8 | 1,378,634 | 3.97% | 590 s |
| zpaq -m5 | 1,394,045 | 2.89% | 8.6 s |
| xz -9e | 1,398,964 | 2.55% | 0.5 s |

paq8px is close to the ceiling of what generic compression can do, so the gap
comes from understanding the HEVC bitstream rather than from a stronger
general model. cmix was not run (it needs 32 GB of RAM); precomp was not run
because it has nothing to unpack in HEIC/MOV files and falls back to LZMA.

No public tool losslessly recompresses HEVC that we could find. Dropbox's
avrecode/losslessh264 did this for H.264 (deprecated, and limited to CAVLC
streams); Lepton and JPEG XL do it for JPEG, where Huffman coding leaves far
more slack than HEVC's CABAC.

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

On an 8-core i9 (2019 MacBook Pro): a 1.5 MB HEIC compresses in about 6.5 s
and decompresses in 2.5 s; a 4.3 MB Live Photo video in about 20 s and 7 s.
Compression includes a full verifying decompression. `pack` runs files in
parallel, so all eight sample files take about 24 s.
