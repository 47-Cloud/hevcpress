//! Just enough ISO-BMFF (QuickTime .mov and HEIF) to locate HEVC samples.

use std::collections::HashMap;

#[derive(Clone, Copy)]
pub struct BoxRef {
    pub kind: [u8; 4],
    pub body: usize,
    pub end: usize,
}

fn be16(b: &[u8]) -> u32 {
    u16::from_be_bytes(b[..2].try_into().unwrap()) as u32
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// Big-endian integer of `n` bytes (0..=8).
fn be_n(b: &[u8], n: usize) -> u64 {
    b[..n].iter().fold(0u64, |a, &x| (a << 8) | x as u64)
}

/// Direct child boxes in `data[start..end]`. Stops at the first malformed box.
pub fn children(data: &[u8], start: usize, end: usize) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let mut size = be32(&data[pos..]) as u64;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let mut header = 8;
        if size == 1 {
            if pos + 16 > end {
                break;
            }
            size = be64(&data[pos + 8..]);
            header = 16;
        } else if size == 0 {
            size = (end - pos) as u64;
        }
        let box_end = pos.saturating_add(size as usize);
        if size < header as u64 || box_end > end {
            break;
        }
        out.push(BoxRef {
            kind,
            body: pos + header,
            end: box_end,
        });
        pos = box_end;
    }
    out
}

pub fn find(boxes: &[BoxRef], kind: &[u8; 4]) -> Option<BoxRef> {
    boxes.iter().find(|b| &b.kind == kind).copied()
}

/// Children of a `meta` box, which is a FullBox in HEIF but a plain
/// container in QuickTime.
pub fn meta_children(data: &[u8], m: BoxRef) -> Vec<BoxRef> {
    let quicktime = m.body + 8 <= m.end && &data[m.body + 4..m.body + 8] == b"hdlr";
    children(data, if quicktime { m.body } else { m.body + 4 }, m.end)
}

/// HEVC decoder configuration (hvcC): NAL length size plus parameter-set NALs
/// (each including its 2-byte NAL header).
#[derive(Clone, Default)]
pub struct Config {
    pub nal_len_size: usize,
    pub param_sets: Vec<Vec<u8>>,
}

pub fn parse_hvcc(d: &[u8]) -> Option<Config> {
    if d.len() < 23 {
        return None;
    }
    let mut cfg = Config {
        nal_len_size: (d[21] & 3) as usize + 1,
        param_sets: Vec::new(),
    };
    let mut p = 23;
    for _ in 0..d[22] {
        if p + 3 > d.len() {
            return None;
        }
        let n = be16(&d[p + 1..]) as usize;
        p += 3;
        for _ in 0..n {
            if p + 2 > d.len() {
                return None;
            }
            let len = be16(&d[p..]) as usize;
            p += 2;
            cfg.param_sets.push(d.get(p..p + len)?.to_vec());
            p += len;
        }
    }
    Some(cfg)
}

/// A run of length-prefixed NAL units in the file, with the config it decodes under.
pub struct Unit {
    pub label: String,
    pub start: usize,
    pub len: usize,
    pub config: usize,
}

pub struct Extracted {
    pub configs: Vec<Config>,
    pub units: Vec<Unit>,
}

pub fn extract(data: &[u8]) -> Result<Extracted, String> {
    let top = children(data, 0, data.len());
    if let Some(moov) = find(&top, b"moov") {
        extract_mov(data, moov)
    } else if let Some(meta) = find(&top, b"meta") {
        extract_heif(data, meta)
    } else {
        Err("neither moov nor meta box found".into())
    }
}

fn extract_mov(data: &[u8], moov: BoxRef) -> Result<Extracted, String> {
    let mut ex = Extracted {
        configs: Vec::new(),
        units: Vec::new(),
    };
    let mut track = 0;
    for trak in children(data, moov.body, moov.end)
        .into_iter()
        .filter(|b| &b.kind == b"trak")
    {
        track += 1;
        let Some(stbl) = (|| {
            let mdia = find(&children(data, trak.body, trak.end), b"mdia")?;
            let minf = find(&children(data, mdia.body, mdia.end), b"minf")?;
            find(&children(data, minf.body, minf.end), b"stbl")
        })() else {
            continue;
        };
        let st = children(data, stbl.body, stbl.end);

        // First sample entry must be HEVC.
        let Some(stsd) = find(&st, b"stsd") else { continue };
        let entries = children(data, stsd.body + 8, stsd.end);
        let Some(entry) = entries.first().filter(|e| &e.kind == b"hvc1" || &e.kind == b"hev1") else {
            continue;
        };
        let (w, h) = (be16(&data[entry.body + 24..]), be16(&data[entry.body + 26..]));
        let sub = children(data, entry.body + 78, entry.end);
        let Some(hvcc) = find(&sub, b"hvcC") else { continue };
        let cfg = parse_hvcc(&data[hvcc.body..hvcc.end]).ok_or("bad hvcC")?;
        ex.configs.push(cfg);
        let config = ex.configs.len() - 1;

        // Sample sizes.
        let stsz = find(&st, b"stsz").ok_or("no stsz")?;
        let b = &data[stsz.body..stsz.end];
        let (fixed, count) = (be32(&b[4..]) as usize, be32(&b[8..]) as usize);
        let sizes: Vec<usize> = (0..count)
            .map(|i| {
                if fixed != 0 {
                    fixed
                } else {
                    be32(&b[12 + 4 * i..]) as usize
                }
            })
            .collect();

        // Chunk offsets.
        let chunk_offsets: Vec<u64> = if let Some(co) = find(&st, b"stco") {
            let b = &data[co.body..co.end];
            (0..be32(&b[4..]) as usize)
                .map(|i| be32(&b[8 + 4 * i..]) as u64)
                .collect()
        } else if let Some(co) = find(&st, b"co64") {
            let b = &data[co.body..co.end];
            (0..be32(&b[4..]) as usize).map(|i| be64(&b[8 + 8 * i..])).collect()
        } else {
            return Err("no stco/co64".into());
        };

        // Sample-to-chunk.
        let stsc = find(&st, b"stsc").ok_or("no stsc")?;
        let b = &data[stsc.body..stsc.end];
        let runs: Vec<(usize, usize)> = (0..be32(&b[4..]) as usize)
            .map(|i| (be32(&b[8 + 12 * i..]) as usize, be32(&b[12 + 12 * i..]) as usize))
            .collect();

        let label = format!("mov track {track} hevc {w}x{h}");
        let mut sample = 0;
        for (ci, &off) in chunk_offsets.iter().enumerate() {
            let chunk_no = ci + 1;
            let per_chunk = runs.iter().rev().find(|r| r.0 <= chunk_no).map_or(0, |r| r.1);
            let mut pos = off as usize;
            for _ in 0..per_chunk {
                let Some(&len) = sizes.get(sample) else { break };
                ex.units.push(Unit {
                    label: label.clone(),
                    start: pos,
                    len,
                    config,
                });
                pos += len;
                sample += 1;
            }
        }
    }
    Ok(ex)
}

fn extract_heif(data: &[u8], meta: BoxRef) -> Result<Extracted, String> {
    let mc = meta_children(data, meta);

    // Item types (infe).
    let iinf = find(&mc, b"iinf").ok_or("no iinf")?;
    let version = data[iinf.body];
    let skip = if version == 0 { 6 } else { 8 };
    let mut item_type: HashMap<u32, [u8; 4]> = HashMap::new();
    for infe in children(data, iinf.body + skip, iinf.end) {
        let b = &data[infe.body..infe.end];
        let v = b[0];
        if v >= 2 {
            let (id, rest) = if v == 2 {
                (be16(&b[4..]), 8)
            } else {
                (be32(&b[4..]), 10)
            };
            item_type.insert(id, b[rest..rest + 4].try_into().unwrap());
        }
    }

    // Properties: ipco children are 1-indexed; ipma links items to them.
    let iprp = find(&mc, b"iprp").ok_or("no iprp")?;
    let pr = children(data, iprp.body, iprp.end);
    let ipco = find(&pr, b"ipco").ok_or("no ipco")?;
    let props = children(data, ipco.body, ipco.end);
    let ipma = find(&pr, b"ipma").ok_or("no ipma")?;
    let b = &data[ipma.body..ipma.end];
    let (ver, flags) = (b[0], be32(&[0, b[1], b[2], b[3]]));
    let mut p = 8;
    let mut item_props: HashMap<u32, Vec<usize>> = HashMap::new();
    for _ in 0..be32(&b[4..]) {
        let id = if ver < 1 {
            let v = be16(&b[p..]);
            p += 2;
            v
        } else {
            let v = be32(&b[p..]);
            p += 4;
            v
        };
        let n = b[p] as usize;
        p += 1;
        let mut v = Vec::new();
        for _ in 0..n {
            if flags & 1 != 0 {
                v.push((be16(&b[p..]) & 0x7fff) as usize);
                p += 2;
            } else {
                v.push((b[p] & 0x7f) as usize);
                p += 1;
            }
        }
        item_props.insert(id, v);
    }

    // Locations (iloc).
    let iloc = find(&mc, b"iloc").ok_or("no iloc")?;
    let b = &data[iloc.body..iloc.end];
    let ver = b[0];
    let (off_sz, len_sz) = ((b[4] >> 4) as usize, (b[4] & 15) as usize);
    let (base_sz, idx_sz) = ((b[5] >> 4) as usize, if ver >= 1 { (b[5] & 15) as usize } else { 0 });
    let (count, mut p) = if ver < 2 {
        (be16(&b[6..]), 8)
    } else {
        (be32(&b[6..]), 10)
    };

    let mut ex = Extracted {
        configs: Vec::new(),
        units: Vec::new(),
    };
    let mut cfg_index: HashMap<usize, usize> = HashMap::new(); // hvcC prop index -> config
    for _ in 0..count {
        let id = if ver < 2 {
            let v = be16(&b[p..]);
            p += 2;
            v
        } else {
            let v = be32(&b[p..]);
            p += 4;
            v
        };
        let mut method = 0;
        if ver >= 1 {
            method = be16(&b[p..]) & 15;
            p += 2;
        }
        p += 2; // data_reference_index
        let base = be_n(&b[p..], base_sz);
        p += base_sz;
        let extents = be16(&b[p..]);
        p += 2;
        let mut ranges = Vec::new();
        for _ in 0..extents {
            p += idx_sz;
            let off = be_n(&b[p..], off_sz);
            p += off_sz;
            let len = be_n(&b[p..], len_sz);
            p += len_sz;
            ranges.push((base + off, len));
        }

        if item_type.get(&id) != Some(b"hvc1") || method != 0 || ranges.len() != 1 {
            continue;
        }
        let ps = item_props.get(&id).cloned().unwrap_or_default();
        let Some(&hv) = ps
            .iter()
            .find(|&&i| i >= 1 && props.get(i - 1).is_some_and(|b| &b.kind == b"hvcC"))
        else {
            continue;
        };
        let config = match cfg_index.get(&hv) {
            Some(&c) => c,
            None => {
                let pb = props[hv - 1];
                ex.configs.push(parse_hvcc(&data[pb.body..pb.end]).ok_or("bad hvcC")?);
                cfg_index.insert(hv, ex.configs.len() - 1);
                ex.configs.len() - 1
            }
        };
        let dims = ps.iter().find_map(|&i| {
            let pb = props.get(i.checked_sub(1)?)?;
            (&pb.kind == b"ispe").then(|| (be32(&data[pb.body + 4..]), be32(&data[pb.body + 8..])))
        });
        let label = match dims {
            Some((w, h)) => format!("heic item hvc1 {w}x{h}"),
            None => "heic item hvc1".into(),
        };
        ex.units.push(Unit {
            label,
            start: ranges[0].0 as usize,
            len: ranges[0].1 as usize,
            config,
        });
    }
    Ok(ex)
}

/// PCM audio samples located in the file: 16-bit little-endian, `channels` interleaved.
pub struct Audio {
    pub channels: usize,
    /// Byte ranges (offset, length) in file order, one per chunk.
    pub runs: Vec<(usize, usize)>,
}

/// Find the first track of 16-bit little-endian integer PCM ('sowt' or 'lpcm').
pub fn extract_audio(data: &[u8]) -> Option<Audio> {
    let top = children(data, 0, data.len());
    let moov = find(&top, b"moov")?;
    for trak in children(data, moov.body, moov.end)
        .into_iter()
        .filter(|b| &b.kind == b"trak")
    {
        let mdia = find(&children(data, trak.body, trak.end), b"mdia")?;
        let minf = find(&children(data, mdia.body, mdia.end), b"minf")?;
        let stbl = find(&children(data, minf.body, minf.end), b"stbl")?;
        let st = children(data, stbl.body, stbl.end);
        let stsd = find(&st, b"stsd")?;
        let entries = children(data, stsd.body + 8, stsd.end);
        let Some(e) = entries.first() else { continue };
        let b = &data[e.body..e.end];
        let channels = match &e.kind {
            b"sowt" if b.len() >= 28 && be16(&b[8..]) <= 1 && be16(&b[18..]) == 16 => be16(&b[16..]) as usize,
            b"lpcm" if b.len() >= 64 && be16(&b[8..]) == 2 => {
                let (ch, bits, flags) = (be32(&b[40..]), be32(&b[48..]), be32(&b[52..]));
                // Integer (not float), little-endian.
                if bits == 16 && flags & 3 == 0 {
                    ch as usize
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if !(1..=2).contains(&channels) {
            continue;
        }

        let stsz = find(&st, b"stsz")?;
        let z = &data[stsz.body..stsz.end];
        let (fixed, count) = (be32(&z[4..]) as usize, be32(&z[8..]) as usize);
        let size_of = |i: usize| {
            if fixed != 0 {
                fixed
            } else {
                be32(&z[12 + 4 * i..]) as usize
            }
        };
        let chunk_offsets: Vec<u64> = if let Some(co) = find(&st, b"stco") {
            let c = &data[co.body..co.end];
            (0..be32(&c[4..]) as usize)
                .map(|i| be32(&c[8 + 4 * i..]) as u64)
                .collect()
        } else {
            let co = find(&st, b"co64")?;
            let c = &data[co.body..co.end];
            (0..be32(&c[4..]) as usize).map(|i| be64(&c[8 + 8 * i..])).collect()
        };
        let stsc = find(&st, b"stsc")?;
        let c = &data[stsc.body..stsc.end];
        let runs_tab: Vec<(usize, usize)> = (0..be32(&c[4..]) as usize)
            .map(|i| (be32(&c[8 + 12 * i..]) as usize, be32(&c[12 + 12 * i..]) as usize))
            .collect();

        let mut runs = Vec::new();
        let mut sample = 0;
        for (ci, &off) in chunk_offsets.iter().enumerate() {
            let per_chunk = runs_tab.iter().rev().find(|r| r.0 <= ci + 1).map_or(0, |r| r.1);
            let mut len = 0;
            for _ in 0..per_chunk {
                if sample >= count {
                    break;
                }
                len += size_of(sample);
                sample += 1;
            }
            if off as usize + len > data.len() {
                return None;
            }
            if len > 0 {
                runs.push((off as usize, len));
            }
        }
        let total: usize = runs.iter().map(|r| r.1).sum();
        if total == 0 || total % (2 * channels) != 0 {
            return None;
        }
        return Some(Audio { channels, runs });
    }
    None
}
