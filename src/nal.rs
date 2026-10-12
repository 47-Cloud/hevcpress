//! HEVC NAL unit splitting and per-type byte accounting.

use crate::bits::unescape;
use crate::bmff::Extracted;
use crate::ps::{ParamSets, parse_slice_header};
use std::collections::BTreeMap;

pub fn nal_name(t: u8) -> String {
    match t {
        0 => "TRAIL_N".into(),
        1 => "TRAIL_R".into(),
        2 => "TSA_N".into(),
        3 => "TSA_R".into(),
        4 => "STSA_N".into(),
        5 => "STSA_R".into(),
        6 => "RADL_N".into(),
        7 => "RADL_R".into(),
        8 => "RASL_N".into(),
        9 => "RASL_R".into(),
        16 => "BLA_W_LP".into(),
        17 => "BLA_W_RADL".into(),
        18 => "BLA_N_LP".into(),
        19 => "IDR_W_RADL".into(),
        20 => "IDR_N_LP".into(),
        21 => "CRA".into(),
        32 => "VPS".into(),
        33 => "SPS".into(),
        34 => "PPS".into(),
        35 => "AUD".into(),
        36 => "EOS".into(),
        37 => "EOB".into(),
        38 => "FD".into(),
        39 => "PREFIX_SEI".into(),
        40 => "SUFFIX_SEI".into(),
        n => format!("type{n}"),
    }
}

/// VCL NAL types that carry slice segments.
pub fn is_slice(t: u8) -> bool {
    t <= 9 || (16..=21).contains(&t)
}

/// Visit every NAL (2-byte header included) of a length-prefixed unit.
/// Returns false if the unit does not tile into NALs exactly.
pub fn for_each_nal<'a>(unit: &'a [u8], len_size: usize, mut f: impl FnMut(&'a [u8])) -> bool {
    let mut pos = 0;
    while pos + len_size <= unit.len() {
        let len = unit[pos..pos + len_size]
            .iter()
            .fold(0usize, |a, &x| (a << 8) | x as usize);
        pos += len_size;
        if len < 2 || pos + len > unit.len() {
            return false;
        }
        f(&unit[pos..pos + len]);
        pos += len;
    }
    pos == unit.len()
}

#[derive(Default)]
struct Group {
    units: u64,
    unit_bytes: u64,
    length_prefix_bytes: u64,
    nals: BTreeMap<u8, (u64, u64)>, // type -> (count, bytes incl. 2-byte header)
    slice_types: BTreeMap<u32, (u64, u64)>,
}

pub fn load_config(ps: &mut ParamSets, cfg: &crate::bmff::Config) {
    for nal in &cfg.param_sets {
        if let Some(e) = ps.absorb(nal) {
            eprintln!("warning: {e}");
        }
    }
}

pub fn report(data: &[u8], ex: &Extracted) {
    let mut pss: Vec<ParamSets> = ex
        .configs
        .iter()
        .map(|c| {
            let mut ps = ParamSets::default();
            load_config(&mut ps, c);
            ps
        })
        .collect();

    let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
    let mut bad_units = 0;
    let mut unparsed_slices = 0u64;

    for u in &ex.units {
        let g = groups.entry(&u.label).or_default();
        g.units += 1;
        g.unit_bytes += u.len as u64;
        let n = ex.configs[u.config].nal_len_size;
        g.length_prefix_bytes += 0;
        let ps = &mut pss[u.config];
        let ok = for_each_nal(&data[u.start..u.start + u.len], n, |nal| {
            g.length_prefix_bytes += n as u64;
            let t = (nal[0] >> 1) & 0x3f;
            let e = g.nals.entry(t).or_default();
            e.0 += 1;
            e.1 += nal.len() as u64;
            ps.absorb(nal);
            if is_slice(t) {
                match parse_slice_header(t, &unescape(&nal[2..]), ps, None) {
                    Ok(sh) => {
                        let e = g.slice_types.entry(sh.slice_type).or_default();
                        e.0 += 1;
                        e.1 += nal.len() as u64;
                    }
                    Err(_) => unparsed_slices += 1,
                }
            }
        });
        if !ok {
            bad_units += 1;
        }
    }

    for (label, g) in &groups {
        println!(
            "  {label}: {} units, {} bytes ({} bytes of NAL length prefixes)",
            g.units, g.unit_bytes, g.length_prefix_bytes
        );
        for (t, (count, bytes)) in &g.nals {
            println!(
                "    {:<12} {:>6} NALs {:>10} bytes {:>6.2}%",
                nal_name(*t),
                count,
                bytes,
                100.0 * *bytes as f64 / g.unit_bytes as f64
            );
        }
        for (t, (count, bytes)) in &g.slice_types {
            let name = ["B", "P", "I"][*t as usize];
            println!("    slice type {name}: {count:>6} slices {bytes:>10} bytes");
        }
    }
    let total: u64 = groups.values().map(|g| g.unit_bytes).sum();
    println!("  total HEVC sample bytes: {total}");
    if bad_units > 0 || unparsed_slices > 0 {
        println!("  note: {bad_units} units did not tile into NALs, {unparsed_slices} slices without a parsed header");
    }
}
