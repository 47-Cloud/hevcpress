# hevcpress

Lossless recompression for iPhone photos (HEIC) and Live Photo videos (MOV).
Files come out about 10% smaller, and decompressing gives back the original
file bit for bit.

General-purpose compressors get very little out of these files (xz manages
around 3%) because almost everything in them is HEVC data that is already
entropy coded. hevcpress parses the HEVC bitstream down to the individual
CABAC decisions, codes those with a stronger context-mixing model, and on the
way back runs a CABAC encoder to regenerate the original bitstream exactly.
It never decodes the image itself.

There's a longer write-up of how it works, and what didn't, at
<https://47-cloud.github.io/hevcpress/>.

## Install

Needs a recent Rust toolchain (1.85 or newer).

```
git clone https://github.com/47-Cloud/hevcpress
cd hevcpress
cargo build --release
```

The binary ends up in `target/release/hevcpress`.

## Usage

```
hevcpress pack   [-j N] files...      compress each file to <file>.hvp, N at a time
hevcpress unpack [-j N] files.hvp...  restore the originals
hevcpress view   files.hvp...         restore a temporary copy and open it
hevcpress verify files...             compress, check the round trip, print sizes
```

There are also `compress` / `decompress` for single files, and two tools I used
while developing it: `inspect` (byte breakdown by NAL unit and slice type) and
`syntax` (bits per syntax element, CABAC against the model).

A `.hvp` file is an archive, not an image: the bytes are no longer valid HEVC,
so nothing can display it until it is unpacked. That is the price of the
saving. `view` makes this less annoying.

`pack` and `unpack` never overwrite existing files.

## Results

Eight files from an iPhone: four photos and their Live Photo videos.

| file | original | hevcpress | saving |
|---|---:|---:|---:|
| IMG_0029.HEIC | 2,201,957 | 2,023,216 | 8.1% |
| IMG_0133.HEIC | 1,435,563 | 1,307,655 | 8.9% |
| IMG_0283.HEIC | 1,556,848 | 1,424,976 | 8.5% |
| IMG_9533.HEIC | 1,726,673 | 1,575,677 | 8.7% |
| IMG_0029.mov | 2,884,020 | 2,595,566 | 10.0% |
| IMG_0133.mov | 3,989,550 | 3,461,383 | 13.2% |
| IMG_0283.mov | 1,203,420 | 1,040,829 | 13.5% |
| IMG_9533.mov | 4,288,293 | 3,809,633 | 11.2% |
| total | 19,286,324 | 17,238,935 | 10.6% |

The IMG_9533 pair was added after the model was tuned, as a check that it
wasn't fitted to the other six. Videos do better than photos for two reasons:
the motion and mode syntax in P and B frames leaves the model more to work
with, and the audio is uncompressed PCM, which gets its own predictor (it ends
up 15-27% smaller than FLAC -8).

How it compares with general-purpose compressors on the same eight files, each
at its strongest setting:

| tool | saving |
|---|---:|
| hevcpress | 10.62% |
| zpaq -m5 | 3.38% |
| 7-Zip 23.01, LZMA2 -mx9 | 2.96% |
| xz -9e | 2.95% |
| brotli -q 11 -w 24 | 2.62% |
| zstd --ultra -22 --long=27 | 2.43% |
| 7-Zip 23.01, PPMd -mx9 | 0.42% |

paq8px -8 was too slow for the whole set (about 2 KB/s), but on IMG_0133.HEIC
it saved 3.97% in 590 seconds, against 8.91% in about 6.5 seconds for
hevcpress. Since paq8px is close to the limit of what generic modelling can do,
the gap really does come from understanding the format.

To run the comparison on your own files: `scripts/bench.sh file...`

Speed, on a 2019 MacBook Pro (8-core i9): about 6.5 s to compress a 1.5 MB
photo and 2.5 s to restore it; about 20 s and 7 s for a 4.3 MB video.
Compression time includes a full verification pass.

## How it works

`bmff.rs` reads the ISO-BMFF container (MOV or HEIF) and finds the HEVC samples
and the PCM audio. `ps.rs` and `ctu.rs` are an HEVC slice-data parser: I, P and
B slices, wavefront parallel processing (which Apple's encoder uses), 4:2:0 and
monochrome, 8 and 10 bit. The parser reads every CABAC bin but stops there; it
has no prediction, transforms or loop filters.

Each bin is then coded with the model in `model.rs`, which mixes predictions
from a dozen or so hashed contexts: neighbouring blocks, already-decoded
coefficients, QP, intra modes, CTU position and so on. The model is integer-only
with fixed lookup tables, because the decompressor has to reproduce every
probability exactly on any machine.

Decompression runs the same parser, takes the bins from the model instead of
from CABAC, and feeds them to a CABAC encoder (`cabac_enc.rs`) that rebuilds
the original slice data. To make sure that is safe, compression replays every
slice through that encoder first. A slice that doesn't come back identical is
left in the file as it is.

Everything that isn't slice data (container boxes, parameter sets, slice
headers, metadata tracks) goes into an xz-compressed block.

## Safety

- Every compressed file carries a CRC-32 of the original, checked on
  decompression. A corrupt file or a bug produces an error, not a wrong file.
- `compress` decompresses its own output and compares it with the input
  before writing anything. If that fails, it falls back to plain xz.
- The header has a format version. The bin stream can only be decoded by the
  exact model that wrote it, so the version must change whenever the model
  does, and a build refuses formats it doesn't know.
- Inputs it doesn't understand (tiles, PCM coding units, 4:2:2 or 4:4:4
  chroma, some range-extension tools, truncated or non-HEVC files) fall back
  to xz.

Still, this is young software with its own file format. I wouldn't delete the
originals of anything irreplaceable on the strength of it.

## Things that didn't work

Worth recording, since some of them looked promising:

- Making the mixer bigger, adding a second weight set, or tuning learning
  rates: a few tenths of a percent at most.
- Warm-starting the model from other files: no gain, it learns fast enough
  within one file.
- Context from the previous frame's coefficient activity: nothing.
- Pixel-domain context. I decoded the pictures with ffmpeg and gave the model
  the texture and edge direction of the already-decoded neighbourhood. It
  saved 0.05-0.12%, because the neighbouring blocks' coefficients already
  carry the same information. A full decoder for sign prediction and
  template-based intra modes might add a point or two, which didn't seem worth
  writing a complete HEVC decoder for.
- Re-encoding Apple's tone-map metadata in the HEIC, a binary plist of 16-bit
  floats: about 0.3%, and dependent on an undocumented format.

## Credits and notes

The CABAC tables and syntax come from ITU-T H.265. I cross-checked the tables
against FFmpeg's HEVC decoder. Dropbox's Lepton (for JPEG) and avrecode (for
H.264) showed that this kind of recompression is worth doing.

hevcpress parses and regenerates HEVC bitstreams (the entropy-coding layer
only; it does not decode images). HEVC is covered by patent licensing
programmes, and commercial use may require a licence.

## License

MIT or Apache-2.0, at your option.
