//! Run the slice-data parser over every slice of a file and report where
//! the bits go, plus whether each slice parsed to exactly its last byte.

use crate::bits::unescape;
use crate::bmff::Extracted;
use crate::cabac::{Stats, CAT_NAMES, NCAT};
use crate::cabac::{Cabac, Io};
use crate::ctu::{decode_slice, Pic};
use crate::model::Model;
use crate::nal::{for_each_nal, is_slice, load_config};
use crate::ps::{parse_slice_header, ParamSets, SliceHeader};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Group {
    slices: u32,
    ok: u32,
    data_bytes: u64,
    est_bits: f64,
    stats: Stats,
    /// data_bits - consumed_bits -> count
    tail: BTreeMap<i64, u32>,
    errors: Vec<String>,
    failed: u32,
}

impl Group {
    fn add_to(&self, total: &mut Group) {
        total.slices += self.slices;
        total.ok += self.ok;
        total.failed += self.failed;
        total.data_bytes += self.data_bytes;
        total.est_bits += self.est_bits;
        total.stats.add(&self.stats);
        for (k, v) in &self.tail {
            *total.tail.entry(*k).or_default() += v;
        }
    }
}

struct ConfigState {
    ps: ParamSets,
    pic: Option<Pic>,
    prev: Option<SliceHeader>,
}

pub fn run(data: &[u8], ex: &Extracted, verbose: bool, model: &mut Model) {
    let mut states: Vec<ConfigState> = ex
        .configs
        .iter()
        .map(|c| {
            let mut ps = ParamSets::default();
            load_config(&mut ps, c);
            ConfigState { ps, pic: None, prev: None }
        })
        .collect();

    if verbose {
        for (i, st) in states.iter().enumerate() {
            for sps in st.ps.sps.values() {
                println!("  config {i} SPS: {sps:?}");
            }
            for pps in st.ps.pps.values() {
                println!("  config {i} PPS: {pps:?}");
            }
        }
    }

    // Group by (label, slice type).
    let mut groups: BTreeMap<(String, u32), Group> = BTreeMap::new();

    for u in &ex.units {
        let n = ex.configs[u.config].nal_len_size;
        let st = &mut states[u.config];
        for_each_nal(&data[u.start..u.start + u.len], n, |nal| {
            let t = (nal[0] >> 1) & 0x3f;
            st.ps.absorb(nal);
            if !is_slice(t) {
                return;
            }
            let rbsp = unescape(&nal[2..]);
            let sh = match parse_slice_header(t, &rbsp, &st.ps, st.prev.as_ref()) {
                Ok(sh) => sh,
                Err(e) => {
                    let g = groups.entry((u.label.clone(), 3)).or_default();
                    g.slices += 1;
                    g.failed += 1;
                    g.errors.push(format!("slice header: {e}"));
                    return;
                }
            };
            let pps = &st.ps.pps[&sh.pps_id];
            let sps = &st.ps.sps[&pps.sps_id];
            if sh.first_slice_segment_in_pic || st.pic.is_none() {
                st.pic = Some(Pic::new(sps));
            }
            let g = groups.entry((u.label.clone(), sh.slice_type)).or_default();
            g.slices += 1;
            g.data_bytes += (rbsp.len() - sh.data_offset) as u64;
            let data = &rbsp[sh.data_offset.min(rbsp.len())..];
            let cabac = Cabac::new(Io::Analyze, data, Some(&mut *model));
            match decode_slice(sps, pps, &sh, st.pic.as_mut().unwrap(), cabac) {
                Ok((r, _)) => {
                    g.ok += 1;
                    g.est_bits += r.stats.total_bits();
                    g.stats.add(&r.stats);
                    *g.tail.entry((data.len() * 8) as i64 - r.consumed_bits as i64).or_default() += 1;
                }
                Err(e) => {
                    g.failed += 1;
                    if g.errors.len() < 3 {
                        g.errors.push(e);
                    }
                }
            }
            st.prev = Some(sh);
        });
    }

    let names = ["B", "P", "I", "?"];
    let mut total = Group::default();
    for ((label, ty), g) in &groups {
        println!(
            "  {label}, {}-slices: {} parsed ok / {} ({} bytes of slice data, model estimate {:.0} bytes)",
            names[*ty as usize],
            g.ok,
            g.slices,
            g.data_bytes,
            g.est_bits / 8.0
        );
        if !g.tail.is_empty() {
            let tail: Vec<String> = g.tail.iter().take(8).map(|(k, v)| format!("{k}:{v}")).collect();
            println!("    unread bits at end of slice data (bits:count): {}", tail.join(" "));
        }
        for e in &g.errors {
            println!("    error: {e}");
        }
        g.add_to(&mut total);
    }

    println!();
    println!(
        "  all slices: {} parsed ok / {}; {} bytes of slice data; model estimate {:.0} bytes",
        total.ok,
        total.slices,
        total.data_bytes,
        total.est_bits / 8.0
    );
    if verbose || total.ok > 0 {
        print_breakdown(&total.stats);
    }
}

fn print_breakdown(s: &Stats) {
    let total = s.total_bits();
    println!();
    println!(
        "  {:<28} {:>10} {:>10} {:>12} {:>7} {:>12} {:>8}",
        "syntax element", "ctx bins", "bypass", "CABAC bits", "%", "model bits", "saving"
    );
    let mut rows: Vec<(usize, f64)> = (0..NCAT)
        .map(|i| (i, s.ctx_bits[i] + s.bypass_bins[i] as f64))
        .filter(|r| r.1 > 0.0)
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    for (i, bits) in rows {
        let bins = s.ctx_bins[i] + s.bypass_bins[i];
        let _ = bins;
        println!(
            "  {:<28} {:>10} {:>10} {:>12.0} {:>6.2}% {:>12.0} {:>7.1}%",
            CAT_NAMES[i],
            s.ctx_bins[i],
            s.bypass_bins[i],
            bits,
            100.0 * bits / total,
            s.model_bits[i],
            100.0 * (1.0 - s.model_bits[i] / bits)
        );
    }
    let model_total: f64 = s.model_bits.iter().sum();
    println!(
        "  {:<28} {:>10} {:>10} {:>12.0} {:>6.2}% {:>12.0} {:>7.1}%",
        "TOTAL", "", "", total, 100.0, model_total, 100.0 * (1.0 - model_total / total)
    );
}
