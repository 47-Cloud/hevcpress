//! The lossless recompressor.
//!
//! Compressed file layout:
//!   "HVP1", mode byte
//!   mode 0: an xz stream of the whole file (fallback)
//!   mode 1: varint original length, varint length of the xz-compressed
//!           "meta" block, the meta block, then the model-coded bin stream.
//!
//! The meta block holds everything that is not CABAC slice data: the file
//! with the slice NALs cut out (the "skeleton"), the parameter sets, and for
//! every cut-out slice its header bytes and where it goes. The bin stream
//! holds the slice data itself, coded bin by bin with the context-mixing model.

use crate::audio;
use crate::bits::unescape;
use crate::bmff::{self, Config};
use crate::cabac::{Cabac, Io};
use crate::coder::{Decoder, Encoder};
use crate::ctu::{decode_slice, Pic};
use crate::model::Model;
use crate::ps::{parse_slice_header, ParamSets, SliceHeader};
use std::io::{Read, Write};

const MAGIC: &[u8; 4] = b"HVP1";

pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 2);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    if out.last() == Some(&0) {
        out.push(3);
    }
    out
}

fn xz_compress(data: &[u8]) -> Vec<u8> {
    let mut e = xz2::write::XzEncoder::new(Vec::new(), 9 | 0x8000_0000);
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn xz_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut d = xz2::read::XzDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

// ---- varint byte writer / reader for the meta block ----

#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn v(&mut self, mut x: u64) {
        while x >= 0x80 {
            self.0.push((x as u8 & 0x7f) | 0x80);
            x >>= 7;
        }
        self.0.push(x as u8);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.v(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct Rd<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn v(&mut self) -> Result<u64, String> {
        let mut x = 0u64;
        let mut shift = 0;
        loop {
            let b = *self.d.get(self.p).ok_or("truncated meta block")?;
            self.p += 1;
            x |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(x);
            }
            shift += 7;
            if shift > 63 {
                return Err("bad varint".into());
            }
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.v()? as usize;
        let s = self.d.get(self.p..self.p + n).ok_or("truncated meta block")?;
        self.p += n;
        Ok(s)
    }
}

/// One slice NAL cut out of the file and re-created from the bin stream.
struct SliceRec {
    config: usize,
    nal_offset: usize,
    nal_len: usize,
    nal_header: [u8; 2],
    /// Unescaped slice header bytes, up to and including byte alignment.
    hdr: Vec<u8>,
    /// Unescaped bytes after the CABAC data (cabac_zero_words).
    tail: Vec<u8>,
}

/// A parameter-set NAL seen in-band, applied before slice `before`.
struct Event {
    before: usize,
    config: usize,
    nal: Vec<u8>,
}

struct Meta {
    configs: Vec<Config>,
    events: Vec<Event>,
    slices: Vec<SliceRec>,
    audio_channels: usize,
    /// (file offset, byte length) of each chunk of PCM audio cut out of the file.
    audio_runs: Vec<(usize, usize)>,
    skeleton: Vec<u8>,
}

impl Meta {
    fn write(&self) -> Vec<u8> {
        let mut w = W::default();
        w.v(self.configs.len() as u64);
        for c in &self.configs {
            w.v(c.nal_len_size as u64);
            w.v(c.param_sets.len() as u64);
            for p in &c.param_sets {
                w.bytes(p);
            }
        }
        w.v(self.events.len() as u64);
        for e in &self.events {
            w.v(e.before as u64);
            w.v(e.config as u64);
            w.bytes(&e.nal);
        }
        w.v(self.slices.len() as u64);
        for s in &self.slices {
            w.v(s.config as u64);
            w.v(s.nal_offset as u64);
            w.v(s.nal_len as u64);
            w.0.extend_from_slice(&s.nal_header);
            w.bytes(&s.hdr);
            w.bytes(&s.tail);
        }
        w.v(self.audio_channels as u64);
        w.v(self.audio_runs.len() as u64);
        for &(off, len) in &self.audio_runs {
            w.v(off as u64);
            w.v(len as u64);
        }
        w.bytes(&self.skeleton);
        w.0
    }

    fn read(d: &[u8]) -> Result<Meta, String> {
        let mut r = Rd { d, p: 0 };
        let mut configs = Vec::new();
        for _ in 0..r.v()? {
            let nal_len_size = r.v()? as usize;
            let mut param_sets = Vec::new();
            for _ in 0..r.v()? {
                param_sets.push(r.bytes()?.to_vec());
            }
            configs.push(Config { nal_len_size, param_sets });
        }
        let mut events = Vec::new();
        for _ in 0..r.v()? {
            events.push(Event { before: r.v()? as usize, config: r.v()? as usize, nal: r.bytes()?.to_vec() });
        }
        let mut slices = Vec::new();
        for _ in 0..r.v()? {
            let config = r.v()? as usize;
            let nal_offset = r.v()? as usize;
            let nal_len = r.v()? as usize;
            let h = r.d.get(r.p..r.p + 2).ok_or("truncated meta block")?;
            let nal_header = [h[0], h[1]];
            r.p += 2;
            slices.push(SliceRec {
                config,
                nal_offset,
                nal_len,
                nal_header,
                hdr: r.bytes()?.to_vec(),
                tail: r.bytes()?.to_vec(),
            });
        }
        let audio_channels = r.v()? as usize;
        let mut audio_runs = Vec::new();
        for _ in 0..r.v()? {
            audio_runs.push((r.v()? as usize, r.v()? as usize));
        }
        let skeleton = r.bytes()?.to_vec();
        Ok(Meta { configs, events, slices, audio_channels, audio_runs, skeleton })
    }
}

/// Per-configuration decoding state shared by compressor and decompressor.
struct ConfigState {
    ps: ParamSets,
    pic: Option<Pic>,
    prev: Option<SliceHeader>,
}

fn new_states(configs: &[Config]) -> Vec<ConfigState> {
    configs
        .iter()
        .map(|c| {
            let mut ps = ParamSets::default();
            for nal in &c.param_sets {
                ps.absorb(nal);
            }
            ConfigState { ps, pic: None, prev: None }
        })
        .collect()
}

/// Plain xz, or the bytes as they are if xz does not help.
fn fallback(data: &[u8]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let xz = xz_compress(data);
    if xz.len() < data.len() {
        out.push(0);
        out.extend(xz);
    } else {
        out.push(2);
        out.extend_from_slice(data);
    }
    out
}

pub struct Report {
    pub slices_total: usize,
    pub slices_coded: usize,
    pub skeleton_bytes: usize,
    pub meta_bytes: usize,
    pub stream_bytes: usize,
}

/// Compress `data`. Always succeeds: anything that cannot be modelled falls
/// back to plain xz. The result is verified by decompressing it again.
pub fn compress(data: &[u8], verify: bool) -> (Vec<u8>, Report) {
    let mut report = Report { slices_total: 0, slices_coded: 0, skeleton_bytes: 0, meta_bytes: 0, stream_bytes: 0 };
    // Container parsing sees arbitrary bytes: any panic means "not something we understand".
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| try_compress(data, &mut report)))
        .unwrap_or_else(|_| Err("malformed container".into()));
    let out = match attempt {
        Ok(out) => {
            if verify {
                match decompress(&out) {
                    Ok(back) if back == data => out,
                    Ok(_) => {
                        eprintln!("warning: round-trip mismatch, using fallback");
                        return (fallback(data), report);
                    }
                    Err(e) => {
                        eprintln!("warning: round-trip failed ({e}), using fallback");
                        return (fallback(data), report);
                    }
                }
            } else {
                out
            }
        }
        Err(e) => {
            eprintln!("note: not recompressing HEVC data ({e}), using fallback");
            return (fallback(data), report);
        }
    };
    let plain = fallback(data);
    if plain.len() <= out.len() { (plain, report) } else { (out, report) }
}

fn try_compress(data: &[u8], report: &mut Report) -> Result<Vec<u8>, String> {
    let ex = bmff::extract(data)?;
    let mut states = new_states(&ex.configs);
    let mut model = Model::new();
    let mut enc = Encoder::new();
    let mut slices: Vec<SliceRec> = Vec::new();
    let mut events: Vec<Event> = Vec::new();

    for unit in &ex.units {
        let n = ex.configs[unit.config].nal_len_size;
        let end = unit.start.checked_add(unit.len).filter(|&e| e <= data.len()).ok_or("sample extends past the end of the file")?;
        let bytes = &data[unit.start..end];
        let st = &mut states[unit.config];
        let mut pos = 0;
        while pos + n <= bytes.len() {
            let len = bytes[pos..pos + n].iter().fold(0usize, |a, &x| (a << 8) | x as usize);
            pos += n;
            if len < 2 || pos + len > bytes.len() {
                return Err("sample does not tile into NAL units".into());
            }
            let nal = &bytes[pos..pos + len];
            let nal_offset = unit.start + pos;
            pos += len;
            let t = (nal[0] >> 1) & 0x3f;

            if t == 33 || t == 34 {
                events.push(Event { before: slices.len(), config: unit.config, nal: nal.to_vec() });
                st.ps.absorb(nal);
                continue;
            }
            if !crate::nal::is_slice(t) {
                continue;
            }
            report.slices_total += 1;
            if let Some(rec) = try_code_slice(st, nal, nal_offset, unit.config, t, &mut model, &mut enc)? {
                slices.push(rec);
                report.slices_coded += 1;
            }
        }
    }
    if slices.is_empty() {
        return Err("no slices could be recompressed".into());
    }

    // Audio: PCM tracks are coded with a dedicated predictor.
    let (mut audio_channels, mut audio_runs) = (0, Vec::new());
    if let Some(a) = bmff::extract_audio(data) {
        let mut samples = Vec::new();
        for &(off, len) in &a.runs {
            samples.extend(data[off..off + len].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
        }
        audio::encode(&samples, a.channels, &mut model, &mut enc);
        audio_channels = a.channels;
        audio_runs = a.runs;
    }

    // Skeleton: the file with the coded slice NALs and audio cut out.
    let mut holes: Vec<(usize, usize)> = slices.iter().map(|s| (s.nal_offset, s.nal_len)).collect();
    holes.extend(audio_runs.iter().copied());
    holes.sort_unstable();
    let mut skeleton = Vec::with_capacity(data.len());
    let mut at = 0;
    for (off, len) in holes {
        if off < at {
            return Err("overlapping slice NALs".into());
        }
        skeleton.extend_from_slice(&data[at..off]);
        at = off + len;
    }
    skeleton.extend_from_slice(&data[at..]);

    report.skeleton_bytes = skeleton.len();
    let meta = Meta { configs: ex.configs, events, slices, audio_channels, audio_runs, skeleton }.write();
    let meta_xz = xz_compress(&meta);
    let stream = enc.finish();
    report.meta_bytes = meta_xz.len();
    report.stream_bytes = stream.len();

    let mut out = MAGIC.to_vec();
    out.push(1);
    let mut w = W::default();
    w.v(data.len() as u64);
    w.v(meta_xz.len() as u64);
    out.extend(w.0);
    out.extend(meta_xz);
    out.extend(stream);
    Ok(out)
}

/// Try to take over one slice NAL. Returns `None` (leaving all shared state
/// untouched) if it cannot be reproduced exactly; it then stays in the skeleton.
fn try_code_slice(
    st: &mut ConfigState,
    nal: &[u8],
    nal_offset: usize,
    config: usize,
    nal_type: u8,
    model: &mut Model,
    enc: &mut Encoder,
) -> Result<Option<SliceRec>, String> {
    code_slice(st, nal, nal_offset, config, nal_type, model, enc)
}

fn code_slice(
    st: &mut ConfigState,
    nal: &[u8],
    nal_offset: usize,
    config: usize,
    nal_type: u8,
    model: &mut Model,
    enc: &mut Encoder,
) -> Result<Option<SliceRec>, String> {
    let rbsp = unescape(&nal[2..]);
    if escape(&rbsp) != nal[2..] {
        return Ok(None);
    }
    let Ok(sh) = parse_slice_header(nal_type, &rbsp, &st.ps, st.prev.as_ref()) else { return Ok(None) };
    if sh.data_offset >= rbsp.len() {
        return Ok(None);
    }
    let (Some(pps), Some(sps)) = (st.ps.pps.get(&sh.pps_id), st.ps.pps.get(&sh.pps_id).and_then(|p| st.ps.sps.get(&p.sps_id))) else {
        return Ok(None);
    };
    let payload = &rbsp[sh.data_offset..];

    let base = if sh.first_slice_segment_in_pic || st.pic.is_none() {
        Pic::new(sps)
    } else {
        st.pic.clone().unwrap()
    };

    // Pass 1: decode the original data and record the bins.
    let mut cabac = Cabac::new(Io::Analyze, payload, None);
    cabac.rec = Some(Vec::new());
    let Ok((_, mut cabac)) = decode_slice(sps, pps, &sh, &mut base.clone(), cabac) else { return Ok(None) };
    let Some(bins) = cabac.rec.take() else { return Ok(None) };

    // Pass 2: re-encode the bins and check we get the original bytes back.
    let replay = Cabac::new(Io::Replay(&bins, 0), &[], None);
    let Ok((_, replay)) = decode_slice(sps, pps, &sh, &mut base.clone(), replay) else { return Ok(None) };
    let produced = replay.finish_encoded();
    if !payload.starts_with(&produced) {
        return Ok(None);
    }
    let tail = payload[produced.len()..].to_vec();
    if tail.len() > 4096 {
        return Ok(None);
    }

    // Pass 3: the real thing, with the shared model and coder.
    let mut pic = base;
    let cabac = Cabac::new(Io::Compress(enc), payload, Some(model));
    decode_slice(sps, pps, &sh, &mut pic, cabac).map_err(|e| format!("internal error while coding a verified slice: {e}"))?;
    st.pic = Some(pic);
    let rec = SliceRec {
        config,
        nal_offset,
        nal_len: nal.len(),
        nal_header: [nal[0], nal[1]],
        hdr: rbsp[..sh.data_offset].to_vec(),
        tail,
    };
    st.prev = Some(sh);
    Ok(Some(rec))
}

pub fn decompress(comp: &[u8]) -> Result<Vec<u8>, String> {
    if comp.len() < 5 || &comp[..4] != MAGIC {
        return Err("not a compressed file".into());
    }
    match comp[4] {
        0 => xz_decompress(&comp[5..]),
        1 => std::panic::catch_unwind(|| decompress_hevc(&comp[5..]))
            .unwrap_or_else(|_| Err("corrupt compressed data".into())),
        2 => Ok(comp[5..].to_vec()),
        m => Err(format!("unknown mode {m}")),
    }
}

fn decompress_hevc(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = Rd { d: body, p: 0 };
    let orig_len = r.v()? as usize;
    let meta_len = r.v()? as usize;
    let meta_xz = body.get(r.p..r.p + meta_len).ok_or("truncated file")?;
    let stream = body[r.p + meta_len..].to_vec();
    let meta = Meta::read(&xz_decompress(meta_xz)?)?;

    let mut states = new_states(&meta.configs);
    let mut model = Model::new();
    let mut dec = Decoder::new(stream);
    let mut nals: Vec<(usize, Vec<u8>)> = Vec::with_capacity(meta.slices.len());
    let mut ev = 0;

    for (k, rec) in meta.slices.iter().enumerate() {
        while ev < meta.events.len() && meta.events[ev].before <= k {
            let e = &meta.events[ev];
            states.get_mut(e.config).ok_or("bad config index")?.ps.absorb(&e.nal);
            ev += 1;
        }
        let st = states.get_mut(rec.config).ok_or("bad config index")?;
        let nal_type = (rec.nal_header[0] >> 1) & 0x3f;
        let sh = parse_slice_header(nal_type, &rec.hdr, &st.ps, st.prev.as_ref())?;
        let pps = st.ps.pps.get(&sh.pps_id).ok_or("missing PPS")?;
        let sps = st.ps.sps.get(&pps.sps_id).ok_or("missing SPS")?;
        let mut pic = if sh.first_slice_segment_in_pic || st.pic.is_none() {
            Pic::new(sps)
        } else {
            st.pic.take().unwrap()
        };
        let cabac = Cabac::new(Io::Decompress(&mut dec), &[], Some(&mut model));
        let (_, cabac) = decode_slice(sps, pps, &sh, &mut pic, cabac)?;
        let payload = cabac.finish_encoded();

        let mut rbsp = rec.hdr.clone();
        rbsp.extend_from_slice(&payload);
        rbsp.extend_from_slice(&rec.tail);
        let mut nal = rec.nal_header.to_vec();
        nal.extend(escape(&rbsp));
        if nal.len() != rec.nal_len {
            return Err(format!("slice {k} regenerated with the wrong length"));
        }
        nals.push((rec.nal_offset, nal));
        st.pic = Some(pic);
        st.prev = Some(sh);
    }

    if !meta.audio_runs.is_empty() {
        let total: usize = meta.audio_runs.iter().map(|r| r.1).sum();
        let samples = audio::decode(total / 2, meta.audio_channels, &mut model, &mut dec);
        let mut bytes = Vec::with_capacity(total);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut at = 0;
        for &(off, len) in &meta.audio_runs {
            nals.push((off, bytes[at..at + len].to_vec()));
            at += len;
        }
    }

    nals.sort_unstable_by_key(|n| n.0);
    let mut out = Vec::with_capacity(orig_len);
    let mut sk = 0;
    let mut at = 0;
    for (off, nal) in &nals {
        let take = off.checked_sub(at).ok_or("overlapping slices")?;
        out.extend_from_slice(meta.skeleton.get(sk..sk + take).ok_or("skeleton too short")?);
        sk += take;
        out.extend_from_slice(nal);
        at = off + nal.len();
    }
    out.extend_from_slice(&meta.skeleton[sk..]);
    if out.len() != orig_len {
        return Err("reconstructed file has the wrong size".into());
    }
    Ok(out)
}
