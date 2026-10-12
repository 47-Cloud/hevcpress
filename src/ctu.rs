//! HEVC slice data parsing (7.3.8): CTU, coding quadtree, coding unit,
//! prediction unit, transform tree and residual coding. Only the syntax is
//! parsed; no samples are reconstructed.

use crate::cabac::{ctx, Cabac, Cat, SavedState, Stats};
use crate::model::Mc;
use crate::ps::{Pps, SliceHeader, Sps};
use std::sync::OnceLock;

const PART_2NX2N: u8 = 0;
const PART_NXN: u8 = 3;

/// Per-picture state that later syntax depends on (neighbour lookups).
#[derive(Clone)]
pub struct Pic {
    min_cb_w: usize,
    w4: usize,
    ctb_w: usize,
    ct_depth: Vec<u8>,
    skip: Vec<u8>,
    qp: Vec<i8>,
    /// SAO parameters per CTU (raster order).
    sao: Vec<SaoCtb>,
    /// Per min-CB: bit0 skip, bit1 intra, bit2 merge, bits3-4 merge_idx, bits5-7 part mode.
    info: Vec<u8>,
    /// Per min-CB intra_chroma_pred_mode syntax value (4 = derived from luma).
    cmode: Vec<u8>,
    /// Per 4x4: log2 size of the transform unit covering it.
    tu_log2: Vec<u8>,
    ipm: Vec<u8>,
    slice_addr: Vec<i32>,
    cur_slice_addr: i32,
    saved: Option<(SavedState, [u8; 4])>,
    /// Context variables stored after the second CTU of a row (WPP).
    wpp: Option<(SavedState, [u8; 4])>,
    /// Coefficients of every coded transform block, per colour plane.
    tbs: [Plane; 3],
}

/// SAO parameters of one CTU, kept so neighbours can predict each other.
#[derive(Clone, Copy, Default)]
struct SaoCtb {
    ty: [u8; 3],
    eo: [u8; 3],
    band: [u8; 3],
    off: [[u8; 4]; 3],
}

/// Absolute levels and signs of one transform block, kept for neighbour contexts.
#[derive(Clone)]
struct Blk {
    log2: u8,
    abs: Vec<u8>,
    sgn: Vec<i8>,
    sum: u32,
    /// Last significant coefficient prefix values (x, y) as coded.
    last: [u32; 2],
}

#[derive(Default, Clone)]
struct Plane {
    w4: usize,
    ids: Vec<u32>,
    blocks: Vec<Blk>,
}

/// What the neighbouring blocks tell us about the current transform block.
#[derive(Clone, Copy, Default)]
struct Nb {
    l: Option<usize>,
    a: Option<usize>,
    /// Cross-component block (luma for chroma, Cb for Cr) and its plane.
    x: Option<usize>,
    xp: usize,
    act: u32,
    xact: u32,
    l_last: [u32; 2],
    a_last: [u32; 2],
}

fn qlog(v: u32) -> u32 {
    if v == 0 { 0 } else { (32 - v.leading_zeros()).min(12) }
}

fn last_prefix(c: u32) -> u32 {
    if c < 4 {
        c
    } else {
        let k = 31 - c.leading_zeros();
        2 * k + ((c >> (k - 1)) & 1)
    }
}

impl Plane {
    fn lookup(&self, x4: isize, y4: isize) -> Option<usize> {
        if x4 < 0 || y4 < 0 || x4 as usize >= self.w4 {
            return None;
        }
        let i = y4 as usize * self.w4 + x4 as usize;
        match self.ids.get(i) {
            Some(&id) if id != 0 => Some(id as usize - 1),
            _ => None,
        }
    }
}

impl Pic {
    fn neighbours(&self, plane: usize, px: usize, py: usize) -> Nb {
        let p = &self.tbs[plane];
        let mut nb = Nb::default();
        nb.l = p.lookup(px as isize / 4 - 1, py as isize / 4);
        nb.a = p.lookup(px as isize / 4, py as isize / 4 - 1);
        let norm = |b: &Blk| {
            let m = 1u32 << b.log2;
            b.sum * 64 / (m * m)
        };
        let mut e = 0;
        if let Some(i) = nb.l {
            e += norm(&p.blocks[i]);
            nb.l_last = [last_prefix(p.blocks[i].last[0]), last_prefix(p.blocks[i].last[1])];
        } else {
            nb.l_last = [99, 99];
        }
        if let Some(i) = nb.a {
            e += norm(&p.blocks[i]);
            nb.a_last = [last_prefix(p.blocks[i].last[0]), last_prefix(p.blocks[i].last[1])];
        } else {
            nb.a_last = [99, 99];
        }
        nb.act = qlog(e);
        if plane > 0 {
            nb.xp = plane - 1;
            let (cx, cy) = if plane == 1 { (px * 2, py * 2) } else { (px, py) };
            nb.x = self.tbs[nb.xp].lookup(cx as isize / 4, cy as isize / 4);
            if let Some(i) = nb.x {
                let b = &self.tbs[nb.xp].blocks[i];
                let m = 1u32 << b.log2;
                nb.xact = qlog(b.sum * 64 / (m * m));
            } else {
                nb.xact = 0;
            }
        }
        nb
    }

    /// Absolute level (capped at 3, or 15 if there is no block) and sign
    /// class (0 none, 1 positive, 2 negative) of the neighbour block's
    /// coefficient at the same relative frequency as (x, y) in an n x n block.
    fn nb_val(&self, plane: usize, blk: Option<usize>, x: usize, y: usize, n: usize) -> (u32, u32) {
        let Some(i) = blk else { return (15, 0) };
        let b = &self.tbs[plane].blocks[i];
        let m = 1usize << b.log2;
        let (bx, by) = if m >= n { (x * (m / n), y * (m / n)) } else { (x / (n / m), y / (n / m)) };
        let k = by * m + bx;
        let sg = match b.sgn[k] {
            0 => 0,
            s if s > 0 => 1,
            _ => 2,
        };
        ((b.abs[k] as u32).min(3), sg)
    }

    #[allow(clippy::too_many_arguments)]
    fn register(&mut self, plane: usize, px: usize, py: usize, log2: u32, absv: &[u16], sgn: &[i8], last: [u32; 2]) {
        let n = 1usize << log2;
        let p = &mut self.tbs[plane];
        let abs: Vec<u8> = absv[..n * n].iter().map(|&v| v.min(255) as u8).collect();
        let sum = absv[..n * n].iter().map(|&v| v as u32).sum();
        p.blocks.push(Blk { log2: log2 as u8, abs, sgn: sgn[..n * n].to_vec(), sum, last });
        let id = p.blocks.len() as u32;
        let (x4, y4, n4) = (px / 4, py / 4, (n / 4).max(1));
        for j in 0..n4 {
            for i in 0..n4 {
                let k = (y4 + j) * p.w4 + x4 + i;
                if x4 + i < p.w4 && k < p.ids.len() {
                    p.ids[k] = id;
                }
            }
        }
    }
}

impl Pic {
    pub fn new(sps: &Sps) -> Pic {
        let min_cb_w = (sps.width >> sps.log2_min_cb) as usize;
        let min_cb_h = (sps.height >> sps.log2_min_cb) as usize;
        let (w4, h4) = ((sps.width / 4) as usize, (sps.height / 4) as usize);
        Pic {
            min_cb_w,
            w4,
            ctb_w: sps.ctb_width() as usize,
            ct_depth: vec![0; min_cb_w * min_cb_h],
            skip: vec![0; min_cb_w * min_cb_h],
            qp: vec![0; min_cb_w * min_cb_h],
            sao: vec![SaoCtb::default(); (sps.ctb_width() * sps.ctb_height()) as usize],
            info: vec![0; min_cb_w * min_cb_h],
            cmode: vec![0; min_cb_w * min_cb_h],
            tu_log2: vec![0; w4 * h4],
            ipm: vec![1; w4 * h4],
            slice_addr: vec![-1; (sps.ctb_width() * sps.ctb_height()) as usize],
            cur_slice_addr: 0,
            saved: None,
            wpp: None,
            tbs: {
                let plane = |w: usize, h: usize| {
                    let w4 = w.div_ceil(4);
                    Plane { w4, ids: vec![0; w4 * h.div_ceil(4)], blocks: Vec::new() }
                };
                let (w, h) = (sps.width as usize, sps.height as usize);
                [plane(w, h), plane(w / 2, h / 2), plane(w / 2, h / 2)]
            },
        }
    }
}

pub struct SliceResult {
    pub stats: Stats,
    pub ctus: u32,
    /// Bits of slice data consumed, counting from the start of the slice data.
    pub consumed_bits: usize,
}

#[derive(Clone, Copy, Default)]
struct Cu {
    tq_bypass: bool,
    intra: bool,
    part: u8,
    intra_split: bool,
    luma_modes: [u8; 4],
    chroma_mode: u8,
    merge_flag: bool,
    merge_idx: u32,
    chroma_syn: u32,
    x: u32,
    y: u32,
    depth: u32,
    max_trafo_depth: u32,
}

struct P<'a, 'c> {
    sps: &'a Sps,
    pps: &'a Pps,
    sh: &'a SliceHeader,
    c: Cabac<'c>,
    pic: &'a mut Pic,
    chroma: bool,
    stat_coeff: [u8; 4],
    absv: Vec<u16>,
    sgn: Vec<i8>,
    prev_last: [[u32; 2]; 2],
    cu: Cu,
    qp_delta_coded: bool,
    /// QpY of the last CU decoded, qPY_PRED of the current quantization group,
    /// and the CuQpDeltaVal decoded so far in it.
    cur_qp: i32,
    qp_pred: i32,
    qg_new: bool,
    dqp_val: i32,
    last_dqp: [i32; 2],
    last_idc: u32,
    last_cbf_luma: u32,
    last_ref: [u32; 2],
    last_mvp: [u32; 2],
    last_mvd: [u32; 2],
    chroma_qp_offset_coded: bool,
    slice_addr: i32,
}

type R<T> = Result<T, String>;

/// Scan order tables: `scan(scan_idx, log2_blk)` gives (x, y) for each position.
fn scan(scan_idx: u32, log2_blk: u32) -> &'static [(u8, u8)] {
    static T: OnceLock<Vec<Vec<Vec<(u8, u8)>>>> = OnceLock::new();
    let t = T.get_or_init(|| {
        (0..3)
            .map(|idx| {
                (0..4u32)
                    .map(|l| {
                        let blk = 1i32 << l;
                        let mut v = Vec::new();
                        match idx {
                            0 => {
                                let (mut x, mut y) = (0i32, 0i32);
                                while v.len() < (blk * blk) as usize {
                                    while y >= 0 {
                                        if x < blk && y < blk {
                                            v.push((x as u8, y as u8));
                                        }
                                        y -= 1;
                                        x += 1;
                                    }
                                    y = x;
                                    x = 0;
                                }
                            }
                            1 => {
                                for y in 0..blk {
                                    for x in 0..blk {
                                        v.push((x as u8, y as u8));
                                    }
                                }
                            }
                            _ => {
                                for x in 0..blk {
                                    for y in 0..blk {
                                        v.push((x as u8, y as u8));
                                    }
                                }
                            }
                        }
                        v
                    })
                    .collect()
            })
            .collect()
    });
    &t[scan_idx as usize][log2_blk as usize]
}

const CTX_IDX_MAP_4X4: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// Parse one slice segment's data through `c`, which supplies and receives the bins.
/// Returns the result together with `c` so the caller can collect any regenerated bytes.
pub fn decode_slice<'c>(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceHeader,
    pic: &mut Pic,
    mut c: Cabac<'c>,
) -> R<(SliceResult, Cabac<'c>)> {
    if pps.tiles_enabled {
        return Err("tiles are not supported".into());
    }
    if sps.chroma_array_type() > 1 {
        return Err(format!("chroma_format_idc {} is not supported", sps.chroma_format_idc));
    }
    if sps.cabac_bypass_alignment || sps.extended_precision {
        return Err("cabac_bypass_alignment / extended_precision are not supported".into());
    }
    let init_type = match sh.slice_type {
        2 => 0,
        1 => if sh.cabac_init_flag { 2 } else { 1 },
        _ => if sh.cabac_init_flag { 1 } else { 2 },
    };
    let mut stat_coeff = [0u8; 4];
    if sh.dependent {
        let (saved, stat) = pic.saved.as_ref().ok_or("dependent slice segment without saved contexts")?;
        c.restore_contexts(saved);
        stat_coeff = *stat;
    } else {
        c.init_contexts(init_type, sh.slice_qp);
        pic.cur_slice_addr = sh.segment_address as i32;
    }

    let slice_addr = pic.cur_slice_addr;
    let mut p = P {
        sps,
        pps,
        sh,
        c,
        pic,
        chroma: sps.chroma_array_type() != 0,
        stat_coeff,
        absv: vec![0; 1024],
        sgn: vec![0; 1024],
        prev_last: [[0; 2]; 2],
        cu: Cu::default(),
        qp_delta_coded: false,
        cur_qp: sh.slice_qp,
        qp_pred: sh.slice_qp,
        qg_new: false,
        dqp_val: 0,
        last_dqp: [0; 2],
        last_idc: 0,
        last_cbf_luma: 0,
        last_ref: [0; 2],
        last_mvp: [0; 2],
        last_mvd: [0; 2],
        chroma_qp_offset_coded: false,
        slice_addr,
    };

    let ctb_w = sps.ctb_width();
    let total = ctb_w * sps.ctb_height();
    let wpp = pps.entropy_coding_sync;
    let mut rs = sh.segment_address;
    let mut ctus = 0;
    loop {
        if rs >= total {
            return Err("slice data runs past the end of the picture".into());
        }
        let (rx, ry) = (rs % ctb_w, rs / ctb_w);
        p.pic.slice_addr[rs as usize] = p.slice_addr;
        if wpp && rx == 0 && (rs != sh.segment_address || sh.dependent) {
            // First CTU of a row: sync from the CTU above-right if it is in this slice.
            let top_right = ctb_w > 1 && ry > 0 && p.pic.slice_addr[(rs - ctb_w + 1) as usize] == p.slice_addr;
            if top_right {
                let (saved, stat) = p.pic.wpp.as_ref().ok_or("missing WPP context storage")?;
                p.c.restore_contexts(saved);
                p.stat_coeff = *stat;
            } else {
                p.c.init_contexts(init_type, sh.slice_qp);
                p.stat_coeff = [0; 4];
            }
        }
        if wpp && rx == 0 {
            p.cur_qp = sh.slice_qp;
        }
        p.sao(rx, ry);
        p.coding_quadtree(rx << sps.log2_ctb, ry << sps.log2_ctb, sps.log2_ctb, 0)
            .map_err(|e| format!("{e} (CTU {rs})"))?;
        ctus += 1;
        if wpp && rx == 1.min(ctb_w - 1) && ctb_w > 1 {
            p.pic.wpp = Some((p.c.save_contexts(), p.stat_coeff));
        }
        if p.c.terminate(Cat::EndOfSlice) == 1 {
            break;
        }
        rs += 1;
        if wpp && rs % ctb_w == 0 {
            if p.c.terminate(Cat::EndOfSlice) != 1 {
                return Err(format!("end_of_subset_one_bit is not 1 (CTU {rs})"));
            }
            p.c.align_and_reinit();
        }
    }

    if pps.dependent_slice_segments_enabled {
        p.pic.saved = Some((p.c.save_contexts(), p.stat_coeff));
    }
    let result = SliceResult { stats: p.c.stats.clone(), ctus, consumed_bits: p.c.bit_pos() };
    Ok((result, p.c))
}

impl<'a, 'c> P<'a, 'c> {
    fn w(&self) -> u32 {
        self.sps.width
    }

    fn h(&self) -> u32 {
        self.sps.height
    }

    fn ctb_rs(&self, x: u32, y: u32) -> usize {
        (y >> self.sps.log2_ctb) as usize * self.pic.ctb_w + (x >> self.sps.log2_ctb) as usize
    }

    /// Is the sample to the left of (x, y) available (inside the slice)?
    fn avail_left(&self, x: u32, y: u32) -> bool {
        let mask = (1 << self.sps.log2_ctb) - 1;
        x > 0 && (x & mask != 0 || self.pic.slice_addr[self.ctb_rs(x - 1, y)] == self.slice_addr)
    }

    fn avail_up(&self, x: u32, y: u32) -> bool {
        let mask = (1 << self.sps.log2_ctb) - 1;
        y > 0 && (y & mask != 0 || self.pic.slice_addr[self.ctb_rs(x, y - 1)] == self.slice_addr)
    }

    /// Neighbour CU record (see `Pic::info`), or 0x100 if unavailable.
    fn info_l(&self, x: u32, y: u32) -> u32 {
        if self.avail_left(x, y) { self.pic.info[self.cb_idx(x - 1, y)] as u32 } else { 0x100 }
    }

    fn info_u(&self, x: u32, y: u32) -> u32 {
        if self.avail_up(x, y) { self.pic.info[self.cb_idx(x, y - 1)] as u32 } else { 0x100 }
    }

    fn cb_idx(&self, x: u32, y: u32) -> usize {
        (y >> self.sps.log2_min_cb) as usize * self.pic.min_cb_w + (x >> self.sps.log2_min_cb) as usize
    }

    fn sao(&mut self, rx: u32, ry: u32) {
        if !(self.sh.sao_luma || self.sh.sao_chroma) {
            return;
        }
        let ctb_w = self.pic.ctb_w;
        let rs = ry as usize * ctb_w + rx as usize;
        let left = if rx > 0 { Some(self.pic.sao[rs - 1]) } else { None };
        let up = if ry > 0 { Some(self.pic.sao[rs - ctb_w]) } else { None };
        let left_ok = rx > 0 && self.pic.slice_addr[rs - 1] == self.slice_addr;
        let up_ok = ry > 0 && self.pic.slice_addr[rs - ctb_w] == self.slice_addr;
        let lt = |c: usize| left.map_or(9, |l| l.ty[c] as u32);
        let ut = |c: usize| up.map_or(9, |u| u.ty[c] as u32);

        let mut merge_left = false;
        let mut merge_up = false;
        if left_ok {
            let mut mc = Mc::new(0);
            mc.add(&[1, 0]);
            mc.add(&[2, lt(0), ut(0), up_ok as u32]);
            mc.add(&[3, lt(1), ut(1), self.sh.sao_luma as u32, self.sh.sao_chroma as u32]);
            merge_left = self.c.decision_m(ctx::SAO_MERGE, Cat::SaoMerge, &mc) == 1;
        }
        if up_ok && !merge_left {
            let mut mc = Mc::new(1);
            mc.add(&[1, 1]);
            mc.add(&[2, lt(0), ut(0), left_ok as u32]);
            mc.add(&[3, lt(1), ut(1), self.sh.sao_luma as u32, self.sh.sao_chroma as u32]);
            merge_up = self.c.decision_m(ctx::SAO_MERGE, Cat::SaoMerge, &mc) == 1;
        }
        if merge_left || merge_up {
            self.pic.sao[rs] = if merge_left { left.unwrap() } else { up.unwrap() };
            return;
        }

        let mut cur = SaoCtb::default();
        for c_idx in 0..(if self.chroma { 3 } else { 1 }) {
            if !(if c_idx == 0 { self.sh.sao_luma } else { self.sh.sao_chroma }) {
                continue;
            }
            let ci = c_idx as u32;
            if c_idx < 2 {
                let mut mc = Mc::new(c_idx);
                mc.add(&[1, ci]);
                mc.add(&[2, ci, lt(c_idx), ut(c_idx)]);
                mc.add(&[3, ci, if c_idx == 1 { cur.ty[0] as u32 } else { 9 }, lt(c_idx)]);
                let ty = if self.c.decision_m(ctx::SAO_TYPE, Cat::SaoType, &mc) == 0 {
                    0
                } else {
                    let mut mc = Mc::new(2 + c_idx);
                    mc.add(&[1, ci, lt(c_idx), ut(c_idx)]);
                    mc.add(&[2, ci, if c_idx == 1 { cur.ty[0] as u32 } else { 9 }]);
                    if self.c.bypass_m(Cat::SaoType, &mc) == 0 { 1 } else { 2 }
                };
                cur.ty[c_idx] = ty;
            } else {
                cur.ty[2] = cur.ty[1];
            }
            let ty = cur.ty[c_idx] as u32;
            if ty == 0 {
                continue;
            }
            let bit_depth = if c_idx == 0 { self.sps.bit_depth_luma } else { self.sps.bit_depth_chroma };
            let cmax = (1u32 << (bit_depth.min(10) - 5)) - 1;
            let ln = left.map_or([9u8; 4], |l| l.off[c_idx]);
            let un = up.map_or([9u8; 4], |u| u.off[c_idx]);
            for k in 0..4usize {
                let mut a = 0u32;
                while a < cmax {
                    let prev = if k > 0 { cur.off[c_idx][k - 1] as u32 } else { 99 };
                    let mut mc = Mc::new((ci * 2 + (ty - 1)) as usize);
                    mc.add(&[1, ci, ty, k as u32, a]);
                    mc.add(&[2, ty, k as u32, a, prev.min(9)]);
                    mc.add(&[3, ci, ty, k as u32, a, (ln[k] as u32).min(9)]);
                    mc.add(&[4, ty, k as u32, a, (ln[k] as u32).min(9), (un[k] as u32).min(9)]);
                    mc.add(&[5, ci, ty, a, prev.min(9), if k > 1 { cur.off[c_idx][k - 2] as u32 } else { 99 }.min(9)]);
                    if self.c.bypass_m(Cat::SaoOffset, &mc) == 0 {
                        break;
                    }
                    a += 1;
                }
                cur.off[c_idx][k] = a as u8;
            }
            if ty == 1 {
                for k in 0..4usize {
                    if cur.off[c_idx][k] != 0 {
                        let mut mc = Mc::new(8);
                        mc.add(&[1, ci, k as u32]);
                        self.c.bypass_m(Cat::SaoOffset, &mc);
                    }
                }
                let lb = left.map_or(99, |l| l.band[c_idx] as u32);
                let mut v = 0u32;
                for bit in 0..5u32 {
                    let mut mc = Mc::new(9 + bit as usize);
                    mc.add(&[1, ci, bit, v]);
                    mc.add(&[2, ci, bit, v, lb >> (4 - bit)]);
                    v = (v << 1) | self.c.bypass_m(Cat::SaoBand, &mc);
                }
                cur.band[c_idx] = v as u8;
            } else if c_idx != 2 {
                let le = left.map_or(9, |l| l.eo[c_idx] as u32);
                let ue = up.map_or(9, |u| u.eo[c_idx] as u32);
                let mut v = 0u32;
                for bit in 0..2u32 {
                    let mut mc = Mc::new(15 + bit as usize);
                    mc.add(&[1, ci, bit, v]);
                    mc.add(&[2, ci, bit, v, le, ue]);
                    v = (v << 1) | self.c.bypass_m(Cat::SaoEo, &mc);
                }
                cur.eo[c_idx] = v as u8;
            } else {
                cur.eo[2] = cur.eo[1];
            }
        }
        self.pic.sao[rs] = cur;
    }

    fn coding_quadtree(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> R<()> {
        let size = 1 << log2;
        let sps = self.sps;
        let split = if x0 + size <= self.w() && y0 + size <= self.h() && log2 > sps.log2_min_cb {
            let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
            let ld = if il != 0x100 { self.pic.ct_depth[self.cb_idx(x0 - 1, y0)] as u32 } else { 9 };
            let ud = if iu != 0x100 { self.pic.ct_depth[self.cb_idx(x0, y0 - 1)] as u32 } else { 9 };
            let inc = (ld != 9 && ld > depth) as usize + (ud != 9 && ud > depth) as usize;
            let qp4 = (self.qp_now().max(0) / 4) as u32;
            let cmask = (1u32 << sps.log2_ctb) - 1;
            let st = self.sh.slice_type;
            let mut mc = Mc::new(depth as usize);
            mc.add(&[1, inc as u32]);
            mc.add(&[2, depth, ld, ud]);
            mc.add(&[3, depth, il & 3, iu & 3, st]);
            mc.add(&[4, depth, (x0 & cmask) >> log2, (y0 & cmask) >> log2]);
            mc.add(&[5, depth, ld, ud, qp4]);
            mc.add(&[6, depth, il & 0x107, iu & 0x107, st]);
            self.c.decision_m(ctx::SPLIT_CU + inc, Cat::SplitCu, &mc) == 1
        } else {
            log2 > sps.log2_min_cb
        };
        if self.pps.cu_qp_delta_enabled && log2 >= sps.log2_ctb - self.pps.diff_cu_qp_delta_depth {
            self.qp_delta_coded = false;
            self.qg_new = true;
            self.dqp_val = 0;
        }
        if self.sh.cu_chroma_qp_offset_enabled && log2 >= sps.log2_ctb - self.pps.diff_cu_chroma_qp_offset_depth {
            self.chroma_qp_offset_coded = false;
        }
        if split {
            let half = size >> 1;
            let (x1, y1) = (x0 + half, y0 + half);
            self.coding_quadtree(x0, y0, log2 - 1, depth + 1)?;
            if x1 < self.w() {
                self.coding_quadtree(x1, y0, log2 - 1, depth + 1)?;
            }
            if y1 < self.h() {
                self.coding_quadtree(x0, y1, log2 - 1, depth + 1)?;
            }
            if x1 < self.w() && y1 < self.h() {
                self.coding_quadtree(x1, y1, log2 - 1, depth + 1)?;
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    fn fill_ipm(&mut self, x: u32, y: u32, size: u32, mode: u8) {
        let n = (size / 4).max(1) as usize;
        let (x4, y4) = ((x / 4) as usize, (y / 4) as usize);
        for j in 0..n {
            for i in 0..n {
                let idx = (y4 + j) * self.pic.w4 + x4 + i;
                if idx < self.pic.ipm.len() && x4 + i < self.pic.w4 {
                    self.pic.ipm[idx] = mode;
                }
            }
        }
    }

    fn coding_unit(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> R<()> {
        let size = 1u32 << log2;
        let sps = self.sps;
        self.cu = Cu { part: PART_2NX2N, luma_modes: [1; 4], x: x0, y: y0, depth, ..Cu::default() };
        self.fill_ipm(x0, y0, size, 1);
        if self.pps.cu_qp_delta_enabled && self.qg_new {
            self.qg_new = false;
            let qg_mask = !((1u32 << (sps.log2_ctb - self.pps.diff_cu_qp_delta_depth)) - 1);
            let (xq, yq) = (x0 & qg_mask, y0 & qg_mask);
            let ctb_mask = (1u32 << sps.log2_ctb) - 1;
            let prev = self.cur_qp;
            let a = if xq & ctb_mask != 0 { self.pic.qp[self.cb_idx(xq - 1, yq)] as i32 } else { prev };
            let b = if yq & ctb_mask != 0 { self.pic.qp[self.cb_idx(xq, yq - 1)] as i32 } else { prev };
            self.qp_pred = (a + b + 1) >> 1;
        } else if !self.pps.cu_qp_delta_enabled {
            self.qp_pred = self.sh.slice_qp;
        }

        if self.pps.transquant_bypass_enabled {
            self.cu.tq_bypass = self.c.decision(ctx::TQ_BYPASS, Cat::TqBypass) == 1;
        }
        let mut skip = false;
        if self.sh.slice_type != 2 {
            let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
            let inc = (il & 1) as usize * (il != 0x100) as usize + (iu & 1) as usize * (iu != 0x100) as usize;
            let qp4 = (self.qp_now().max(0) / 4) as u32;
            let st = self.sh.slice_type;
            let mut mc = Mc::new((il & 3 | (iu & 3) << 2) as usize);
            mc.add(&[1, inc as u32]);
            mc.add(&[2, il & 0x107, iu & 0x107]);
            mc.add(&[3, depth, il & 1, iu & 1, st]);
            mc.add(&[4, il & 0x1f, iu & 0x1f, depth]);
            mc.add(&[5, qp4, il & 3, iu & 3, st]);
            skip = self.c.decision_m(ctx::SKIP + inc, Cat::SkipFlag, &mc) == 1;
        }

        if skip {
            self.prediction_unit(x0, y0, size, size, depth, true)?;
        } else {
            self.cu.intra = if self.sh.slice_type == 2 {
                true
            } else {
                let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
                let qp4 = (self.qp_now().max(0) / 4) as u32;
                let mut mc = Mc::new((il & 3 | (iu & 3) << 2) as usize);
                mc.add(&[1, il & 0x103, iu & 0x103]);
                mc.add(&[2, depth, il & 2, iu & 2, self.sh.slice_type]);
                mc.add(&[3, qp4, il & 3, iu & 3]);
                self.c.decision_m(ctx::PRED_MODE, Cat::PredMode, &mc) == 1
            };
            if !self.cu.intra || log2 == sps.log2_min_cb {
                self.cu.part = self.part_mode(log2);
                self.cu.intra_split = self.cu.intra && self.cu.part == PART_NXN;
            }
            if self.cu.intra {
                if self.cu.part == PART_2NX2N
                    && sps.pcm
                    && log2 >= sps.log2_min_pcm_cb
                    && log2 <= sps.log2_max_pcm_cb
                    && self.c.terminate(Cat::Pcm) == 1
                {
                    return Err("PCM coding units are not supported".into());
                }
                self.intra_prediction_unit(x0, y0, log2);
            } else {
                let (s, h, q) = (size, size / 2, size / 4);
                let pus: &[(u32, u32, u32, u32)] = match self.cu.part {
                    0 => &[(0, 0, s, s)],
                    1 => &[(0, 0, s, h), (0, h, s, h)],
                    2 => &[(0, 0, h, s), (h, 0, h, s)],
                    3 => &[(0, 0, h, h), (h, 0, h, h), (0, h, h, h), (h, h, h, h)],
                    4 => &[(0, 0, s, q), (0, q, s, s - q)],
                    5 => &[(0, 0, s, s - q), (0, s - q, s, q)],
                    6 => &[(0, 0, q, s), (q, 0, s - q, s)],
                    _ => &[(0, 0, s - q, s), (s - q, 0, q, s)],
                };
                for &(dx, dy, w, hh) in pus {
                    self.prediction_unit(x0 + dx, y0 + dy, w, hh, depth, false)?;
                }
            }

            let mut rqt_root_cbf = true;
            if !self.cu.intra && !(self.cu.part == PART_2NX2N && self.cu.merge_flag) {
                let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
                let mut mc = Mc::new(self.cu.part as usize);
                mc.add(&[1, self.cu.part as u32, self.cu.merge_flag as u32]);
                mc.add(&[2, depth, il & 7, iu & 7]);
                mc.add(&[3, log2, self.cu.part as u32, self.cu.merge_flag as u32, self.cu.merge_idx.min(2)]);
                rqt_root_cbf = self.c.decision_m(ctx::NO_RESIDUAL, Cat::NoResidual, &mc) == 1;
            }
            if rqt_root_cbf {
                self.cu.max_trafo_depth = if self.cu.intra {
                    sps.max_th_depth_intra + self.cu.intra_split as u32
                } else {
                    sps.max_th_depth_inter
                };
                let mode = self.cu.luma_modes[0];
                self.transform_tree(x0, y0, x0, y0, log2, log2, 0, 0, false, false, mode)?;
            }
        }

        {
            let k = (y0 / 4) as usize * self.pic.w4 + (x0 / 4) as usize;
            if self.pic.tu_log2[k] == 0 || !(x0 == self.cu.x) {
                let n4 = (size / 4) as usize;
                for j in 0..n4 {
                    for i in 0..n4 {
                        let kk = ((y0 / 4) as usize + j) * self.pic.w4 + (x0 / 4) as usize + i;
                        if (x0 / 4) as usize + i < self.pic.w4 && kk < self.pic.tu_log2.len() && self.pic.tu_log2[kk] == 0 {
                            self.pic.tu_log2[kk] = log2 as u8;
                        }
                    }
                }
            }
        }
        // Record per-CU state for later neighbour lookups.
        let qp = self.qp_now();
        let n = (size >> sps.log2_min_cb) as usize;
        let (cx, cy) = ((x0 >> sps.log2_min_cb) as usize, (y0 >> sps.log2_min_cb) as usize);
        for j in 0..n {
            for i in 0..n {
                let idx = (cy + j) * self.pic.min_cb_w + cx + i;
                if idx < self.pic.ct_depth.len() && cx + i < self.pic.min_cb_w {
                    self.pic.ct_depth[idx] = depth as u8;
                    self.pic.skip[idx] = skip as u8;
                    self.pic.qp[idx] = qp as i8;
                    self.pic.info[idx] = skip as u8
                        | (self.cu.intra as u8) << 1
                        | (self.cu.merge_flag as u8) << 2
                        | (self.cu.merge_idx.min(3) as u8) << 3
                        | (self.cu.part.min(7)) << 5;
                    self.pic.cmode[idx] = self.cu.chroma_syn as u8;
                }
            }
        }
        self.cur_qp = qp;
        Ok(())
    }

    /// QpY of the current CU given what has been decoded so far.
    fn qp_now(&self) -> i32 {
        let off = 6 * (self.sps.bit_depth_luma as i32 - 8);
        ((self.qp_pred + self.dqp_val + 52 + 2 * off) % (52 + off)) - off
    }

    /// One context-coded bin of part_mode, modelled with neighbour and size contexts.
    fn pm_bin(&mut self, off: usize, bin_i: u32, log2: u32) -> u32 {
        let (x, y) = (self.cu.x, self.cu.y);
        let (il, iu) = (self.info_l(x, y), self.info_u(x, y));
        let intra = self.cu.intra as u32;
        let (pl, pu) = ((il >> 5) & 7, (iu >> 5) & 7);
        let mut mc = Mc::new((bin_i * 2 + intra) as usize);
        mc.add(&[1, off as u32]);
        mc.add(&[2, bin_i, log2, intra]);
        mc.add(&[3, bin_i, log2, intra, pl, pu]);
        mc.add(&[4, bin_i, log2, intra, il & 3, iu & 3]);
        mc.add(&[5, bin_i, self.cu.depth, intra, self.sh.slice_type]);
        self.c.decision_m(ctx::PART_MODE + off, Cat::PartMode, &mc)
    }

    fn part_mode(&mut self, log2: u32) -> u8 {
        let sps = self.sps;
        if self.pm_bin(0, 0, log2) == 1 {
            return PART_2NX2N;
        }
        if log2 == sps.log2_min_cb {
            if self.cu.intra {
                return PART_NXN;
            }
            if self.pm_bin(1, 1, log2) == 1 {
                return 1; // 2NxN
            }
            if log2 == 3 {
                return 2; // Nx2N
            }
            if self.pm_bin(2, 2, log2) == 1 {
                return 2;
            }
            return PART_NXN;
        }
        if !sps.amp {
            return if self.pm_bin(1, 1, log2) == 1 { 1 } else { 2 };
        }
        if self.pm_bin(1, 1, log2) == 1 {
            if self.pm_bin(3, 3, log2) == 1 {
                return 1;
            }
            return if self.c.bypass(Cat::PartMode) == 1 { 5 } else { 4 };
        }
        if self.pm_bin(3, 3, log2) == 1 {
            return 2;
        }
        if self.c.bypass(Cat::PartMode) == 1 { 7 } else { 6 }
    }

    /// Left and above neighbouring intra modes of the PU at (x, y); DC if unavailable.
    fn mpm_neighbours(&self, x: u32, y: u32) -> (u8, u8) {
        let ctb_mask = (1 << self.sps.log2_ctb) - 1;
        let at = |xx: u32, yy: u32| self.pic.ipm[(yy / 4) as usize * self.pic.w4 + (xx / 4) as usize];
        let left = if self.avail_left(x, y) { at(x - 1, y) } else { 1 };
        // Candidates never come from the CTB row above.
        let up = if y & ctb_mask != 0 && self.avail_up(x, y) { at(x, y - 1) } else { 1 };
        (left, up)
    }

    /// The three most probable modes (8.4.2).
    fn mpm_candidates(left: u8, up: u8) -> [u8; 3] {
        if left == up {
            if left < 2 {
                [0, 1, 26]
            } else {
                [left, 2 + ((left + 29) & 31), 2 + ((left - 2 + 1) & 31)]
            }
        } else {
            let third = if left != 0 && up != 0 {
                0
            } else if left != 1 && up != 1 {
                1
            } else {
                26
            };
            [left, up, third]
        }
    }

    fn intra_prediction_unit(&mut self, x0: u32, y0: u32, log2: u32) {
        let split = self.cu.part == PART_NXN;
        let pb = (1u32 << log2) >> split as u32;
        let side = split as usize + 1;
        let (l0, u0) = self.mpm_neighbours(x0, y0);
        let qp4 = (self.qp_now().max(0) / 4) as u32;
        let depth = self.cu.depth;
        let mut prev = [false; 4];
        for (k, p) in prev.iter_mut().enumerate().take(side * side) {
            let mut mc = Mc::new(split as usize * 4 + k);
            mc.add(&[1, 0]);
            mc.add(&[2, l0 as u32, u0 as u32, split as u32]);
            mc.add(&[3, (l0 == u0) as u32, split as u32, log2]);
            mc.add(&[4, k as u32, split as u32, (l0 < 2) as u32, (u0 < 2) as u32]);
            mc.add(&[5, depth, qp4, split as u32]);
            *p = self.c.decision_m(ctx::PREV_INTRA_LUMA, Cat::PrevIntraLuma, &mc) == 1;
        }
        let mut prev_rem = 99u32;
        for i in 0..side {
            for j in 0..side {
                let idx = 2 * i + j;
                let (px, py) = (x0 + pb * j as u32, y0 + pb * i as u32);
                let (left, up) = self.mpm_neighbours(px, py);
                let cand = Self::mpm_candidates(left, up);
                let (lu, uu) = (left as u32, up as u32);
                let mode;
                if prev[idx] {
                    let mut mpm = 0usize;
                    while mpm < 2 {
                        let mut mc = Mc::new(mpm);
                        mc.add(&[1, mpm as u32]);
                        mc.add(&[2, mpm as u32, lu, uu]);
                        mc.add(&[3, mpm as u32, (left == up) as u32, split as u32, log2]);
                        mc.add(&[4, mpm as u32, cand[0] as u32, cand[1] as u32, cand[2] as u32]);
                        if self.c.bypass_m(Cat::MpmIdx, &mc) == 0 {
                            break;
                        }
                        mpm += 1;
                    }
                    mode = cand[mpm];
                } else {
                    let mut rem = 0u32;
                    for bit in 0..5u32 {
                        let mut mc = Mc::new(bit as usize);
                        mc.add(&[1, bit, rem]);
                        mc.add(&[2, bit, rem, lu, uu]);
                        mc.add(&[3, bit, rem, cand[0] as u32, cand[1] as u32, cand[2] as u32]);
                        mc.add(&[4, bit, rem, split as u32, log2]);
                        mc.add(&[5, bit, rem, prev_rem]);
                        mc.add(&[6, bit, rem, lu]);
                        mc.add(&[7, bit, rem, uu]);
                        rem = (rem << 1) | self.c.bypass_m(Cat::RemIntraMode, &mc);
                    }
                    prev_rem = rem;
                    let mut sorted = cand;
                    sorted.sort_unstable();
                    let mut m = rem as u8;
                    for c in sorted {
                        if m >= c {
                            m += 1;
                        }
                    }
                    mode = m;
                }
                self.cu.luma_modes[idx] = mode;
                self.fill_ipm(px, py, pb, mode);
            }
        }
        if self.chroma {
            let luma = self.cu.luma_modes[0] as u32;
            let cm = |v: u32| if v == 0x100 { 7 } else { self.pic.cmode[v as usize] as u32 };
            let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
            let lc = if il != 0x100 { self.pic.cmode[self.cb_idx(x0 - 1, y0)] as u32 } else { 7 };
            let uc = if iu != 0x100 { self.pic.cmode[self.cb_idx(x0, y0 - 1)] as u32 } else { 7 };
            let _ = cm;
            let mut mc = Mc::new(0);
            mc.add(&[1, 0]);
            mc.add(&[2, luma]);
            mc.add(&[3, lc, uc]);
            mc.add(&[4, luma, lc, uc]);
            mc.add(&[5, (luma < 2) as u32, (luma == 10) as u32, (luma == 26) as u32, lc.min(4)]);
            let syntax = if self.c.decision_m(ctx::INTRA_CHROMA, Cat::IntraChroma, &mc) == 0 {
                4
            } else {
                let mut v = 0u32;
                for bit in 0..2u32 {
                    let mut mc = Mc::new(1 + bit as usize);
                    mc.add(&[1, bit, v]);
                    mc.add(&[2, bit, v, luma]);
                    mc.add(&[3, bit, v, lc, uc]);
                    v = (v << 1) | self.c.bypass_m(Cat::IntraChroma, &mc);
                }
                v
            };
            self.cu.chroma_syn = syntax;
            const TABLE: [u8; 4] = [0, 26, 10, 1];
            let luma = self.cu.luma_modes[0];
            self.cu.chroma_mode = if syntax == 4 {
                luma
            } else if luma == TABLE[syntax as usize] {
                34
            } else {
                TABLE[syntax as usize]
            };
        }
    }

    fn prediction_unit(&mut self, x0: u32, y0: u32, w: u32, h: u32, depth: u32, skip: bool) -> R<()> {
        let max_merge = self.sh.max_num_merge_cand;
        let (il, iu) = (self.info_l(x0, y0), self.info_u(x0, y0));
        let qp4 = (self.qp_now().max(0) / 4) as u32;
        let part = self.cu.part as u32;
        let merge = skip || {
            let mut mc = Mc::new((il & 4 | (iu & 4) << 1) as usize >> 2);
            mc.add(&[1, il & 0x107, iu & 0x107]);
            mc.add(&[2, part, depth, w + h]);
            mc.add(&[3, il & 4, iu & 4, part]);
            mc.add(&[4, qp4, self.sh.slice_type, il & 4, iu & 4]);
            self.c.decision_m(ctx::MERGE_FLAG, Cat::MergeFlag, &mc) == 1
        };
        self.cu.merge_flag = merge;
        if merge {
            let mut idx = 0u32;
            if max_merge > 1 {
                let (ml, mu) = ((il >> 3) & 3, (iu >> 3) & 3);
                let mut mc = Mc::new(0);
                mc.add(&[1, 0]);
                mc.add(&[2, ml, mu, il & 4, iu & 4]);
                mc.add(&[3, depth, part, skip as u32]);
                mc.add(&[4, max_merge, ml, mu]);
                idx = self.c.decision_m(ctx::MERGE_IDX, Cat::MergeIdx, &mc);
                if idx != 0 {
                    while idx < max_merge - 1 {
                        let mut mc = Mc::new(idx as usize);
                        mc.add(&[1, idx, max_merge]);
                        mc.add(&[2, idx, ml, mu]);
                        mc.add(&[3, idx, depth, skip as u32]);
                        if self.c.bypass_m(Cat::MergeIdx, &mc) == 0 {
                            break;
                        }
                        idx += 1;
                    }
                }
            }
            self.cu.merge_idx = idx;
            return Ok(());
        }
        let mut idc = 0; // 0 = L0, 1 = L1, 2 = BI
        if self.sh.slice_type == 0 {
            idc = if w + h == 12 {
                self.c.decision(ctx::INTER_PRED_IDC + 4, Cat::InterPredIdc)
            } else {
                let mut mc = Mc::new(depth as usize);
                mc.add(&[1, depth]);
                mc.add(&[2, depth, w, h]);
                mc.add(&[3, depth, part, self.last_idc]);
                if self.c.decision_m(ctx::INTER_PRED_IDC + depth as usize, Cat::InterPredIdc, &mc) == 1 {
                    2
                } else {
                    self.c.decision(ctx::INTER_PRED_IDC + 4, Cat::InterPredIdc)
                }
            };
            self.last_idc = idc;
        }
        for list in 0..2 {
            if (list == 0 && idc == 1) || (list == 1 && idc == 0) {
                continue;
            }
            let n = self.sh.num_ref_idx_active[list];
            if n > 1 {
                let base = if list == 0 { ctx::REF_IDX_L0 } else { ctx::REF_IDX_L1 };
                let max = n - 1;
                let mut i = 0;
                while i < max.min(2) {
                    let mut mc = Mc::new(i as usize);
                    mc.add(&[1, list as u32, i]);
                    mc.add(&[2, list as u32, i, idc, self.last_ref[list]]);
                    if self.c.decision_m(base + i as usize, Cat::RefIdx, &mc) == 0 {
                        break;
                    }
                    i += 1;
                }
                if i == 2 {
                    while i < max && self.c.bypass(Cat::RefIdx) == 1 {
                        i += 1;
                    }
                }
                self.last_ref[list] = i;
            }
            if !(list == 1 && self.sh.mvd_l1_zero && idc == 2) {
                self.mvd_coding(list as u32, idc);
            }
            let mut mc = Mc::new(list);
            mc.add(&[1, list as u32]);
            mc.add(&[2, list as u32, idc, self.last_mvp[list]]);
            self.last_mvp[list] = self.c.decision_m(ctx::MVP_FLAG, Cat::MvpFlag, &mc);
        }
        Ok(())
    }

    fn mvd_coding(&mut self, list: u32, idc: u32) {
        let lm = self.last_mvd;
        let mut g0 = [0u32; 2];
        for c in 0..2 {
            let mut mc = Mc::new(c);
            mc.add(&[1, c as u32]);
            mc.add(&[2, c as u32, list, idc, if c == 1 { g0[0] } else { 9 }]);
            mc.add(&[3, c as u32, lm[c].min(3), if c == 1 { g0[0] } else { 9 }]);
            mc.add(&[4, c as u32, lm[0].min(2), lm[1].min(2)]);
            g0[c] = self.c.decision_m(ctx::MVD_GT0, Cat::MvdGt0, &mc);
        }
        let mut g1 = [0u32; 2];
        for c in 0..2 {
            if g0[c] == 1 {
                let mut mc = Mc::new(c);
                mc.add(&[1, c as u32]);
                mc.add(&[2, c as u32, lm[c].min(3), if c == 1 { g1[0] } else { 9 }]);
                mc.add(&[3, c as u32, list, idc, g0[1 - c]]);
                g1[c] = self.c.decision_m(ctx::MVD_GT1 + 1, Cat::MvdGt1, &mc);
            }
        }
        let mut mag = [0u32; 2];
        for c in 0..2 {
            if g0[c] == 1 {
                mag[c] = 1;
                if g1[c] == 1 {
                    // abs_mvd_minus2: EG1
                    let mut k = 1;
                    loop {
                        if k >= 31 {
                            break;
                        }
                        let mut mc = Mc::new(k as usize);
                        mc.add(&[1, c as u32, k]);
                        mc.add(&[2, c as u32, k, lm[c].min(4)]);
                        if self.c.bypass_m(Cat::MvdRem, &mc) == 0 {
                            break;
                        }
                        k += 1;
                    }
                    let v = self.c.bypass_bits(k, Cat::MvdRem);
                    mag[c] = 2 + ((1u32 << k) - 2) + v;
                }
                self.c.bypass(Cat::MvdSign);
            }
        }
        self.last_mvd = [mag[0].min(255), mag[1].min(255)];
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_cb: u32,
        log2: u32,
        depth: u32,
        blk_idx: u32,
        parent_cb: bool,
        parent_cr: bool,
        parent_mode: u8,
    ) -> R<()> {
        let sps = self.sps;
        let cu = self.cu;
        let luma_mode = if cu.intra_split && depth == 1 { cu.luma_modes[blk_idx as usize] } else { parent_mode };

        let qp4 = (self.qp_now().max(0) / 4) as u32;
        let intra = cu.intra as u32;
        let split = if log2 <= sps.log2_max_tb
            && log2 > sps.log2_min_tb
            && depth < cu.max_trafo_depth
            && !(cu.intra_split && depth == 0)
        {
            let mut mc = Mc::new((log2 - 2) as usize * 2 + intra as usize);
            mc.add(&[1, 5 - log2]);
            mc.add(&[2, log2, depth, intra, cu.part as u32]);
            mc.add(&[3, log2, self.tu_cmp(x0, y0, log2, true), self.tu_cmp(x0, y0, log2, false)]);
            mc.add(&[4, log2, depth, qp4, intra]);
            mc.add(&[5, log2, cu.max_trafo_depth - depth, intra]);
            self.c.decision_m(ctx::SPLIT_TRANSFORM + 5 - log2 as usize, Cat::SplitTransform, &mc) == 1
        } else {
            let inter_split = sps.max_th_depth_inter == 0 && !cu.intra && cu.part != PART_2NX2N && depth == 0;
            log2 > sps.log2_max_tb || (cu.intra_split && depth == 0) || inter_split
        };

        let (mut cbf_cb, mut cbf_cr) = (parent_cb, parent_cr);
        if self.chroma && log2 > 2 {
            let mode_c = if cu.intra { luma_mode as u32 } else { 35 };
            for which in 0..2u32 {
                let parent = if which == 0 { parent_cb } else { parent_cr };
                let flag = if depth == 0 || parent {
                    let (lh, uh) = self.tb_nb(1 + which as usize, x0 / 2, y0 / 2);
                    let other = if which == 1 { cbf_cb as u32 } else { 9 };
                    let mut mc = Mc::new((which * 4 + depth) as usize);
                    mc.add(&[1, depth]);
                    mc.add(&[2, which, depth, log2, intra, cu.part as u32]);
                    mc.add(&[3, which, depth, parent_cb as u32, parent_cr as u32, other]);
                    mc.add(&[4, which, depth, lh, uh, other]);
                    mc.add(&[5, which, depth, qp4, intra]);
                    mc.add(&[6, which, depth, mode_c, other]);
                    self.c.decision_m(ctx::CBF_CHROMA + depth as usize, Cat::CbfChroma, &mc) == 1
                } else {
                    false
                };
                if which == 0 {
                    cbf_cb = flag;
                } else {
                    cbf_cr = flag;
                }
            }
        }

        if split {
            let half = 1u32 << (log2 - 1);
            let (x1, y1) = (x0 + half, y0 + half);
            for (idx, (x, y)) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)].into_iter().enumerate() {
                self.transform_tree(x, y, x0, y0, log2_cb, log2 - 1, depth + 1, idx as u32, cbf_cb, cbf_cr, luma_mode)?;
            }
            return Ok(());
        }

        let mut cbf_luma = true;
        if cu.intra || depth != 0 || cbf_cb || cbf_cr {
            let (lh, uh) = self.tb_nb(0, x0, y0);
            let mode_c = if cu.intra { luma_mode as u32 } else { 35 };
            let mut mc = Mc::new((depth == 0) as usize * 2 + intra as usize);
            mc.add(&[1, (depth == 0) as u32]);
            mc.add(&[2, depth, log2, intra, cbf_cb as u32, cbf_cr as u32]);
            mc.add(&[3, depth, lh, uh, intra]);
            mc.add(&[4, depth, log2, mode_c]);
            mc.add(&[5, depth, qp4, log2]);
            mc.add(&[6, self.last_cbf_luma, depth, log2, intra]);
            mc.add(&[7, lh, uh, cbf_cb as u32, cbf_cr as u32, self.last_cbf_luma]);
            cbf_luma = self.c.decision_m(ctx::CBF_LUMA + (depth == 0) as usize, Cat::CbfLuma, &mc) == 1;
            self.last_cbf_luma = cbf_luma as u32;
        }
        {
            // Remember the TU size for neighbour contexts.
            let n4 = ((1u32 << log2) / 4).max(1) as usize;
            let (x4, y4) = ((x0 / 4) as usize, (y0 / 4) as usize);
            for j in 0..n4 {
                for i in 0..n4 {
                    let k = (y4 + j) * self.pic.w4 + x4 + i;
                    if x4 + i < self.pic.w4 && k < self.pic.tu_log2.len() {
                        self.pic.tu_log2[k] = log2 as u8;
                    }
                }
            }
        }
        self.transform_unit(x0, y0, x_base, y_base, log2, blk_idx, cbf_luma, cbf_cb, cbf_cr, luma_mode)
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2: u32,
        blk_idx: u32,
        cbf_luma: bool,
        cbf_cb: bool,
        cbf_cr: bool,
        luma_mode: u8,
    ) -> R<()> {
        let cbf_chroma = self.chroma && (cbf_cb || cbf_cr);
        if !(cbf_luma || cbf_chroma) {
            return Ok(());
        }
        let cu = self.cu;

        if self.pps.cu_qp_delta_enabled && !self.qp_delta_coded {
            let (d0, d1) = (self.last_dqp[0], self.last_dqp[1]);
            let (a0, a1) = (d0.unsigned_abs().min(6), d1.unsigned_abs().min(6));
            let s0 = (d0.signum() + 1) as u32;
            let qp4 = (self.qp_pred.max(0) / 4) as u32;
            let cmask = (1u32 << self.sps.log2_ctb) - 1;
            let edge = ((x0 & cmask == 0) as u32) | (((y0 & cmask == 0) as u32) << 1);
            let intra = cu.intra as u32;
            let mut prefix = 0u32;
            while prefix < 5 {
                let mut mc = Mc::new(prefix as usize);
                mc.add(&[1, prefix, (prefix > 0) as u32]);
                mc.add(&[2, prefix, a0, a1]);
                mc.add(&[3, prefix, a0, s0]);
                mc.add(&[4, prefix, cu.depth, qp4]);
                mc.add(&[5, prefix, intra, log2]);
                mc.add(&[6, prefix, edge, a0]);
                if self.c.decision_m(ctx::CU_QP_DELTA + (prefix > 0) as usize, Cat::CuQpDelta, &mc) == 0 {
                    break;
                }
                prefix += 1;
            }
            let mut abs = prefix;
            if prefix >= 5 {
                let mut k = 0;
                while k < 7 {
                    let mut mc = Mc::new(8 + k as usize);
                    mc.add(&[1, k, a0]);
                    mc.add(&[2, k]);
                    if self.c.bypass_m(Cat::CuQpDelta, &mc) == 0 {
                        break;
                    }
                    abs += 1 << k;
                    k += 1;
                }
                if k == 7 {
                    return Err("cu_qp_delta_abs prefix too long".into());
                }
                abs += self.c.bypass_bits(k, Cat::CuQpDelta);
            }
            let mut delta = abs as i32;
            if abs != 0 {
                let mut mc = Mc::new(16);
                mc.add(&[1, s0, a0.min(2)]);
                mc.add(&[2, abs.min(6)]);
                mc.add(&[3, s0, (d1.signum() + 1) as u32]);
                if self.c.bypass_m(Cat::CuQpDelta, &mc) == 1 {
                    delta = -delta;
                }
            }
            self.dqp_val = delta;
            self.last_dqp = [delta, self.last_dqp[0]];
            self.qp_delta_coded = true;
        }

        if self.sh.cu_chroma_qp_offset_enabled && cbf_chroma && !cu.tq_bypass && !self.chroma_qp_offset_coded {
            if self.c.decision(ctx::CU_CHROMA_QP_OFFSET_FLAG, Cat::ChromaQpOffset) == 1
                && self.pps.chroma_qp_offset_list_len_minus1 > 0
            {
                let mut i = 0;
                while i < self.pps.chroma_qp_offset_list_len_minus1
                    && self.c.decision(ctx::CU_CHROMA_QP_OFFSET_IDX, Cat::ChromaQpOffset) == 1
                {
                    i += 1;
                }
            }
            self.chroma_qp_offset_coded = true;
        }

        let mode_scan = |mode: u8| match mode {
            6..=14 => 2,
            22..=30 => 1,
            _ => 0,
        };
        let (scan_luma, scan_chroma) = if cu.intra && log2 < 4 {
            (mode_scan(luma_mode), mode_scan(cu.chroma_mode))
        } else {
            (0, 0)
        };

        if cbf_luma {
            self.residual_coding(log2, 0, scan_luma, luma_mode as u32, x0 as usize, y0 as usize)?;
        }
        if self.chroma {
            if log2 > 2 {
                if cbf_cb {
                    self.residual_coding(log2 - 1, 1, scan_chroma, cu.chroma_mode as u32, x0 as usize / 2, y0 as usize / 2)?;
                }
                if cbf_cr {
                    self.residual_coding(log2 - 1, 2, scan_chroma, cu.chroma_mode as u32, x0 as usize / 2, y0 as usize / 2)?;
                }
            } else if blk_idx == 3 {
                if cbf_cb {
                    self.residual_coding(2, 1, scan_chroma, cu.chroma_mode as u32, x_base as usize / 2, y_base as usize / 2)?;
                }
                if cbf_cr {
                    self.residual_coding(2, 2, scan_chroma, cu.chroma_mode as u32, x_base as usize / 2, y_base as usize / 2)?;
                }
            }
        }
        Ok(())
    }

    fn sig_ctx(&self, x_c: u32, y_c: u32, log2: u32, c_idx: u32, scan_idx: u32, csbf: &[[bool; 8]; 8], ts_ctx: bool) -> usize {
        if ts_ctx {
            return if c_idx == 0 { 42 } else { 43 };
        }
        let sig = if log2 == 2 {
            CTX_IDX_MAP_4X4[((y_c << 2) + x_c) as usize] as usize
        } else if x_c + y_c == 0 {
            0
        } else {
            let (xs, ys) = ((x_c >> 2) as usize, (y_c >> 2) as usize);
            let (xp, yp) = (x_c & 3, y_c & 3);
            let n = (1usize << (log2 - 2)) - 1;
            let prev = (xs < n && csbf[xs + 1][ys]) as u32 + 2 * (ys < n && csbf[xs][ys + 1]) as u32;
            let mut s = match prev {
                0 => if xp + yp == 0 { 2 } else if xp + yp < 3 { 1 } else { 0 },
                1 => if yp == 0 { 2 } else if yp == 1 { 1 } else { 0 },
                2 => if xp == 0 { 2 } else if xp == 1 { 1 } else { 0 },
                _ => 2,
            };
            if c_idx == 0 {
                if xs > 0 || ys > 0 {
                    s += 3;
                }
                s += if log2 == 3 { if scan_idx == 0 { 9 } else { 15 } } else { 21 };
            } else {
                s += if log2 == 3 { 9 } else { 12 };
            }
            s as usize
        };
        if c_idx == 0 { sig } else { 27 + sig }
    }

    /// Sum and count of already-known absolute levels at the template
    /// positions to the right of and below (x, y).
    fn tmpl(&self, x: usize, y: usize, n: usize) -> (u32, u32) {
        let (mut sum, mut cnt) = (0, 0);
        for (dx, dy) in [(1usize, 0usize), (2, 0), (0, 1), (0, 2), (1, 1)] {
            if x + dx < n && y + dy < n {
                let v = self.absv[(y + dy) * n + x + dx] as u32;
                sum += v;
                cnt += (v > 0) as u32;
            }
        }
        (sum, cnt)
    }

    /// Whether the blocks left of and above (px, py) in `plane` carry coefficients.
    fn tb_nb(&self, plane: usize, px: u32, py: u32) -> (u32, u32) {
        let p = &self.pic.tbs[plane];
        let l = px > 0 && p.lookup(px as isize / 4 - 1, py as isize / 4).is_some();
        let u = py > 0 && p.lookup(px as isize / 4, py as isize / 4 - 1).is_some();
        (l as u32, u as u32)
    }

    /// 0 none, 1 smaller, 2 equal, 3 larger: the neighbouring TU size relative to `log2`.
    fn tu_cmp(&self, x: u32, y: u32, log2: u32, left: bool) -> u32 {
        let (nx, ny) = if left { (x.wrapping_sub(1), y) } else { (x, y.wrapping_sub(1)) };
        if nx >= self.sps.width || ny >= self.sps.height {
            return 0;
        }
        let v = self.pic.tu_log2[(ny / 4) as usize * self.pic.w4 + (nx / 4) as usize] as u32;
        match v {
            0 => 0,
            v if v < log2 => 1,
            v if v == log2 => 2,
            _ => 3,
        }
    }

    fn sign_at(&self, x: usize, y: usize, n: usize) -> u32 {
        if x >= n || y >= n {
            return 0;
        }
        match self.sgn[y * n + x] {
            0 => 0,
            s if s > 0 => 1,
            _ => 2,
        }
    }

    fn residual_coding(&mut self, log2: u32, c_idx: u32, scan_idx: u32, pred_mode_intra: u32, px: usize, py: usize) -> R<()> {
        let sps = self.sps;
        let cu = self.cu;
        let chroma = (c_idx > 0) as u32;
        let l2 = log2;
        let n = 1usize << log2;
        self.absv[..n * n].fill(0);
        self.sgn[..n * n].fill(0);
        let mode_ctx = if cu.intra { pred_mode_intra } else { 35 };
        let qpb = (self.qp_now().max(0) / 2) as u32;
        let plane = c_idx as usize;
        let nb = self.pic.neighbours(plane, px, py);
        let (act, xact) = (nb.act, nb.xact);

        let mut transform_skip = false;
        if self.pps.transform_skip_enabled && !cu.tq_bypass && log2 <= self.pps.log2_max_transform_skip_size {
            transform_skip = self.c.decision(ctx::TRANSFORM_SKIP + chroma as usize, Cat::TransformSkip) == 1;
        }
        let mut explicit_rdpcm = false;
        if !cu.intra && sps.explicit_rdpcm && (transform_skip || cu.tq_bypass) {
            explicit_rdpcm = self.c.decision(ctx::EXPLICIT_RDPCM + chroma as usize, Cat::TransformSkip) == 1;
            if explicit_rdpcm {
                self.c.decision(ctx::EXPLICIT_RDPCM_DIR + chroma as usize, Cat::TransformSkip);
            }
        }

        // Last significant coefficient position.
        let (ctx_offset, ctx_shift) = if c_idx == 0 {
            (3 * (log2 as usize - 2) + ((log2 as usize - 1) >> 2), (log2 as usize + 1) >> 2)
        } else {
            (15, log2 as usize - 2)
        };
        let max_prefix = (log2 << 1) - 1;
        let mut prefix = [0u32; 2];
        for (axis, base) in [ctx::LAST_X_PREFIX, ctx::LAST_Y_PREFIX].into_iter().enumerate() {
            let ax = axis as u32;
            let mut i = 0;
            while i < max_prefix {
                let mut mc = Mc::new(((chroma * 4 + (log2 - 2)) + ax * 8) as usize);
                mc.add(&[1, chroma, l2, ax, i]);
                mc.add(&[2, chroma, l2, ax, i, mode_ctx]);
                mc.add(&[3, ax, i, if axis == 1 { prefix[0] } else { 99 }, chroma, l2]);
                mc.add(&[4, chroma, l2, i, qpb, ax]);
                mc.add(&[5, chroma, l2, ax, i, self.prev_last[chroma as usize][axis]]);
                mc.add(&[6, chroma, l2, ax, i, nb.l_last[axis], nb.a_last[axis]]);
                mc.add(&[7, chroma, l2, ax, i, act, xact]);
                if self.c.decision_m(base + (i as usize >> ctx_shift) + ctx_offset, Cat::LastPrefix, &mc) == 0 {
                    break;
                }
                i += 1;
            }
            prefix[axis] = i;
            self.prev_last[chroma as usize][axis] = i;
        }
        let mut last = [prefix[0], prefix[1]];
        for (axis, v) in last.iter_mut().enumerate() {
            if *v > 3 {
                let len = (*v >> 1) - 1;
                let pre = *v;
                let mut suffix = 0;
                for k in 0..len {
                    let mut mc = Mc::new(k as usize);
                    mc.add(&[1, chroma, l2, axis as u32, pre, k]);
                    mc.add(&[2, pre, k, suffix]);
                    suffix = (suffix << 1) | self.c.bypass_m(Cat::LastSuffix, &mc);
                }
                *v = (1 << len) * (2 + (pre & 1)) + suffix;
            }
        }
        if scan_idx == 2 {
            last.swap(0, 1);
        }
        let (last_x, last_y) = (last[0], last[1]);

        // Locate the last coefficient within the sub-block scan.
        let cg_scan = scan(scan_idx, log2 - 2);
        let pos_scan = scan(scan_idx, 2);
        let last_sub = cg_scan
            .iter()
            .position(|&(x, y)| x as u32 == last_x >> 2 && y as u32 == last_y >> 2)
            .ok_or("last significant coefficient outside the block")?;
        let last_pos = pos_scan
            .iter()
            .position(|&(x, y)| x as u32 == last_x & 3 && y as u32 == last_y & 3)
            .unwrap();

        let ts_ctx = sps.transform_skip_context && (transform_skip || cu.tq_bypass);
        let mut csbf = [[false; 8]; 8];
        let mut cg_nsig = [[0u8; 8]; 8];
        let mut greater1_ctx = 1u32;
        let n_cg = 1usize << (log2 - 2);
        let mut tb_nsig = 0u32;

        for i in (0..=last_sub).rev() {
            let (xs, ys) = (cg_scan[i].0 as usize, cg_scan[i].1 as usize);
            let from_last = (last_sub - i).min(15) as u32;
            let mut infer_dc = false;
            if i < last_sub && i > 0 {
                let mut ctx_cg = 0;
                if xs < n_cg - 1 {
                    ctx_cg += csbf[xs + 1][ys] as usize;
                }
                if ys < n_cg - 1 {
                    ctx_cg += csbf[xs][ys + 1] as usize;
                }
                let inc = ctx_cg.min(1) + if c_idx > 0 { 2 } else { 0 };
                let nz_r = if xs < n_cg - 1 { cg_nsig[xs + 1][ys].min(4) as u32 } else { 9 };
                let nz_b = if ys < n_cg - 1 { cg_nsig[xs][ys + 1].min(4) as u32 } else { 9 };
                let mut mc = Mc::new((chroma * 8 + (log2 - 2)) as usize);
                mc.add(&[1, inc as u32]);
                mc.add(&[2, chroma, l2, xs as u32, ys as u32]);
                mc.add(&[3, chroma, l2, nz_r, nz_b]);
                mc.add(&[4, chroma, l2, from_last]);
                mc.add(&[5, chroma, l2, mode_ctx, (xs + ys) as u32]);
                {
                    let (lv, _) = self.pic.nb_val(plane, nb.l, xs * 4, ys * 4, n);
                    let (av, _) = self.pic.nb_val(plane, nb.a, xs * 4, ys * 4, n);
                    let (xv, _) = self.pic.nb_val(nb.xp, nb.x, xs * 4, ys * 4, n);
                    mc.add(&[6, chroma, l2, lv, av, nz_r.min(2), nz_b.min(2)]);
                    mc.add(&[7, chroma, l2, act, from_last.min(7)]);
                    mc.add(&[8, chroma, l2, xv, xact, from_last.min(7)]);
                }
                csbf[xs][ys] = self.c.decision_m(ctx::CODED_SUB_BLOCK + inc, Cat::CodedSubBlock, &mc) == 1;
                infer_dc = true;
            } else {
                csbf[xs][ys] = true;
            }

            // Significant coefficient flags, in decreasing scan order.
            let mut sig_pos = [0u8; 16];
            let mut nsig = 0usize;
            let start = if i == last_sub {
                sig_pos[0] = last_pos as u8;
                nsig = 1;
                let (px, py) = pos_scan[last_pos];
                self.absv[((ys * 4 + py as usize) * n) + xs * 4 + px as usize] = 1;
                tb_nsig += 1;
                last_pos as i32 - 1
            } else {
                15
            };
            if csbf[xs][ys] {
                let mut nn = start;
                while nn >= 0 {
                    let (px, py) = pos_scan[nn as usize];
                    let (x_c, y_c) = ((xs as u32) * 4 + px as u32, (ys as u32) * 4 + py as u32);
                    let at = (y_c as usize) * n + x_c as usize;
                    if nn > 0 || !infer_dc {
                        let inc = self.sig_ctx(x_c, y_c, log2, c_idx, scan_idx, &csbf, ts_ctx);
                        let (sum, cnt) = self.tmpl(x_c as usize, y_c as usize, n);
                        let pb = ((x_c + y_c) * 16 / (2 * n as u32)).min(15);
                        let mut mc = Mc::new((((l2 - 2) * 2 + chroma) * 4 + cnt.min(3)) as usize);
                        mc.add(&[1, inc as u32, l2]);
                        mc.add(&[2, chroma, l2, x_c, y_c]);
                        mc.add(&[3, chroma, l2, qs(sum), cnt]);
                        mc.add(&[4, chroma, l2, mode_ctx, pb]);
                        mc.add(&[5, chroma, l2, tb_nsig.min(8), (i as u32).min(7), nn as u32]);
                        mc.add(&[6, chroma, l2, qpb, pb]);
                        mc.add(&[7, chroma, l2, from_last, nn as u32]);
                        let (xi, yi) = (x_c as usize, y_c as usize);
                        let (lv, _) = self.pic.nb_val(plane, nb.l, xi, yi, n);
                        let (av, _) = self.pic.nb_val(plane, nb.a, xi, yi, n);
                        let (xv, _) = self.pic.nb_val(nb.xp, nb.x, xi, yi, n);
                        mc.add(&[8, chroma, l2, lv, av, cnt.min(2)]);
                        mc.add(&[9, chroma, l2, act, qs(sum)]);
                        mc.add(&[10, chroma, l2, xv, xact, pb]);
                        mc.add(&[11, chroma, l2, lv, av, xv, qs(sum).min(4)]);
                        if self.c.decision_m(ctx::SIG_COEFF + inc, Cat::SigCoeff, &mc) == 1 {
                            sig_pos[nsig] = nn as u8;
                            nsig += 1;
                            tb_nsig += 1;
                            infer_dc = false;
                            self.absv[at] = 1;
                        }
                    } else {
                        // DC of a coded sub-block with no other significant coefficient.
                        sig_pos[nsig] = 0;
                        nsig += 1;
                        tb_nsig += 1;
                        self.absv[at] = 1;
                    }
                    nn -= 1;
                }
            }
            cg_nsig[xs][ys] = nsig as u8;
            if nsig == 0 {
                continue;
            }
            let coord = |nn: u8| {
                let (px, py) = pos_scan[nn as usize];
                (xs * 4 + px as usize, ys * 4 + py as usize)
            };

            // Greater-than-1 / greater-than-2 flags.
            let mut ctx_set = if i > 0 && c_idx == 0 { 2 } else { 0 };
            if i != last_sub && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let mut gt1 = [false; 8];
            let mut first_gt1: Option<usize> = None;
            let (mut seen, mut prev1, mut prev2) = (0u32, 9u32, 9u32);
            for m in 0..nsig.min(8) {
                let (x_c, y_c) = coord(sig_pos[m]);
                let (sum, cnt) = self.tmpl(x_c, y_c, n);
                let pb = ((x_c + y_c) as u32 * 16 / (2 * n as u32)).min(15);
                let inc = (ctx_set << 2) + greater1_ctx as usize + if c_idx > 0 { 16 } else { 0 };
                let mut mc = Mc::new((chroma * 8) as usize + m);
                mc.add(&[1, inc as u32]);
                mc.add(&[2, chroma, l2, m as u32, (nsig as u32).min(8)]);
                mc.add(&[3, chroma, qs(sum), cnt]);
                mc.add(&[4, chroma, seen.min(3), m as u32, (nsig as u32).min(8)]);
                mc.add(&[5, chroma, prev1, prev2]);
                mc.add(&[6, chroma, l2, pb, mode_ctx]);
                mc.add(&[7, chroma, qpb, l2, (i == last_sub) as u32, (i > 0) as u32]);
                let (lv, _) = self.pic.nb_val(plane, nb.l, x_c, y_c, n);
                let (av, _) = self.pic.nb_val(plane, nb.a, x_c, y_c, n);
                let (xv, _) = self.pic.nb_val(nb.xp, nb.x, x_c, y_c, n);
                mc.add(&[8, chroma, l2, lv, av, m as u32]);
                mc.add(&[9, chroma, act, qs(sum), seen.min(2)]);
                mc.add(&[10, chroma, xv, xact, m as u32]);
                let flag = self.c.decision_m(ctx::GT1 + inc, Cat::Gt1, &mc) == 1;
                gt1[m] = flag;
                self.absv[y_c * n + x_c] = 1 + flag as u16;
                prev2 = prev1;
                prev1 = 1 + flag as u32;
                if flag {
                    seen += 1;
                    greater1_ctx = 0;
                    if first_gt1.is_none() {
                        first_gt1 = Some(m);
                    }
                } else if greater1_ctx > 0 && greater1_ctx < 3 {
                    greater1_ctx += 1;
                }
            }
            let mut gt2 = false;
            if let Some(f) = first_gt1 {
                let (x_c, y_c) = coord(sig_pos[f]);
                let (sum, cnt) = self.tmpl(x_c, y_c, n);
                let pb = ((x_c + y_c) as u32 * 16 / (2 * n as u32)).min(15);
                let inc = ctx_set + if c_idx > 0 { 4 } else { 0 };
                let mut mc = Mc::new(chroma as usize);
                mc.add(&[1, inc as u32]);
                mc.add(&[2, chroma, l2, (nsig as u32).min(8)]);
                mc.add(&[3, chroma, qs(sum), cnt]);
                mc.add(&[4, chroma, qpb, pb]);
                mc.add(&[5, chroma, mode_ctx, pb, seen.min(3)]);
                let (lv, _) = self.pic.nb_val(plane, nb.l, x_c, y_c, n);
                let (av, _) = self.pic.nb_val(plane, nb.a, x_c, y_c, n);
                mc.add(&[6, chroma, lv, av]);
                mc.add(&[7, chroma, act, qs(sum)]);
                gt2 = self.c.decision_m(ctx::GT2 + inc, Cat::Gt2, &mc) == 1;
                if gt2 {
                    self.absv[y_c * n + x_c] += 1;
                }
            }

            let last_nz = sig_pos[0] as i32;
            let first_nz = sig_pos[nsig - 1] as i32;
            let sign_hidden = !(cu.tq_bypass
                || (cu.intra && sps.implicit_rdpcm && transform_skip && (pred_mode_intra == 10 || pred_mode_intra == 26))
                || explicit_rdpcm)
                && last_nz - first_nz >= 4;
            let hide = self.pps.sign_data_hiding && sign_hidden;
            let nsigns = if hide { nsig - 1 } else { nsig };
            let mut signs = [0i8; 16];
            let (mut ps1, mut ps2) = (0u32, 0u32);
            for m in 0..nsigns {
                let (x_c, y_c) = coord(sig_pos[m]);
                let pb = ((x_c + y_c) as u32 * 16 / (2 * n as u32)).min(15);
                let sr = self.sign_at(x_c + 1, y_c, n);
                let sb = self.sign_at(x_c, y_c + 1, n);
                let mut mc = Mc::new(chroma as usize);
                mc.add(&[1, chroma, l2.min(3), (x_c as u32).min(7), (y_c as u32).min(7)]);
                mc.add(&[2, chroma, sr, sb]);
                mc.add(&[3, chroma, mode_ctx, pb, ps1]);
                mc.add(&[4, chroma, ps1, ps2]);
                mc.add(&[5, chroma, sr, sb, ps1, l2.min(3)]);
                {
                    let (xi, yi) = (x_c, y_c);
                    let (_, sl) = self.pic.nb_val(plane, nb.l, xi, yi, n);
                    let (_, sa) = self.pic.nb_val(plane, nb.a, xi, yi, n);
                    let (_, sx) = self.pic.nb_val(nb.xp, nb.x, xi, yi, n);
                    mc.add(&[6, chroma, sl, sa, (xi as u32).min(3), (yi as u32).min(3)]);
                    mc.add(&[7, chroma, sx, (xi as u32).min(7), (yi as u32).min(7), l2.min(3)]);
                    mc.add(&[8, chroma, sx, sl, sa, ps1]);
                }
                let neg = self.c.bypass_m(Cat::CoeffSign, &mc);
                signs[m] = if neg == 1 { -1 } else { 1 };
                ps2 = ps1;
                ps1 = 1 + neg;
            }

            // Remaining absolute levels.
            let sb_type = 2 * (c_idx == 0) as usize + (transform_skip || cu.tq_bypass) as usize;
            let mut rice = if sps.persistent_rice { (self.stat_coeff[sb_type] / 4) as u32 } else { 0 };
            let mut rice_init_done = false;
            let mut sum_abs = 0u32;
            let mut prev_rem = 99u32;
            for m in 0..nsig {
                let base = if m < 8 {
                    1 + gt1[m] as u32 + if Some(m) == first_gt1 { gt2 as u32 } else { 0 }
                } else {
                    1
                };
                let threshold = if m < 8 { if Some(m) == first_gt1 { 3 } else { 2 } } else { 1 };
                let mut level = base;
                if base == threshold {
                    let (x_c, y_c) = coord(sig_pos[m]);
                    let (sum, _) = self.tmpl(x_c, y_c, n);
                    let pb = ((x_c + y_c) as u32 * 16 / (2 * n as u32)).min(15);
                    let (lv, _) = self.pic.nb_val(plane, nb.l, x_c, y_c, n);
                    let (av, _) = self.pic.nb_val(plane, nb.a, x_c, y_c, n);
                    let (xv, _) = self.pic.nb_val(nb.xp, nb.x, x_c, y_c, n);
                    let extra = [act, qs(lv.min(3) + av.min(3) + xv.min(3)), xact];
                    let (rem, pre) = self.coeff_abs_level_remaining(rice, chroma, l2, qs(sum), threshold, prev_rem, pb, qpb, extra)?;
                    prev_rem = pre;
                    level = base + rem;
                    if level > (3 << rice) {
                        rice = if sps.persistent_rice { rice + 1 } else { (rice + 1).min(4) };
                    }
                    if sps.persistent_rice && !rice_init_done {
                        let init = (self.stat_coeff[sb_type] / 4) as u32;
                        if rem >= (3 << init) {
                            self.stat_coeff[sb_type] += 1;
                        } else if 2 * rem < (1 << init) && self.stat_coeff[sb_type] > 0 {
                            self.stat_coeff[sb_type] -= 1;
                        }
                        rice_init_done = true;
                    }
                }
                let (x_c, y_c) = coord(sig_pos[m]);
                self.absv[y_c * n + x_c] = level.min(60000) as u16;
                sum_abs += level;
            }
            // Publish this sub-block's signs for later neighbour contexts.
            for m in 0..nsig {
                let (x_c, y_c) = coord(sig_pos[m]);
                let s = if hide && m == nsig - 1 {
                    if sum_abs & 1 == 1 { -1 } else { 1 }
                } else {
                    signs[m]
                };
                self.sgn[y_c * n + x_c] = s;
            }
        }
        self.pic.register(plane, px, py, log2, &self.absv, &self.sgn, [last_x, last_y]);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn coeff_abs_level_remaining(
        &mut self,
        rice: u32,
        chroma: u32,
        l2: u32,
        qsum: u32,
        thresh: u32,
        prev_rem: u32,
        pb: u32,
        qpb: u32,
        extra: [u32; 3],
    ) -> R<(u32, u32)> {
        let mut prefix = 0u32;
        loop {
            if prefix >= 32 {
                return Err("coeff_abs_level_remaining prefix too long".into());
            }
            let mut mc = Mc::new((chroma * 16 + prefix.min(15)) as usize);
            mc.add(&[1, chroma, rice, prefix]);
            mc.add(&[2, chroma, qsum, prefix, rice.min(2)]);
            mc.add(&[3, chroma, thresh, prefix, rice]);
            mc.add(&[4, prev_rem, prefix, rice]);
            mc.add(&[5, chroma, l2, pb, prefix]);
            mc.add(&[6, chroma, qpb, prefix, rice]);
            mc.add(&[7, chroma, extra[0], prefix, rice]);
            mc.add(&[8, chroma, extra[1], qsum, prefix, rice.min(2)]);
            mc.add(&[9, chroma, extra[2], extra[0], prefix]);
            if self.c.bypass_m(Cat::CoeffRemaining, &mc) == 0 {
                break;
            }
            prefix += 1;
        }
        let nbits = if prefix < 3 { rice } else { prefix - 3 + rice };
        if nbits > 22 {
            return Err("coeff_abs_level_remaining suffix too long".into());
        }
        let mut suffix = 0u32;
        for k in 0..nbits {
            let mut mc = Mc::new((32 + k.min(15)) as usize);
            mc.add(&[1, rice, prefix.min(8), k]);
            mc.add(&[2, chroma, prefix.min(8), k, qsum]);
            mc.add(&[3, prefix.min(8), k, if k < 3 { suffix } else { 0 }]);
            suffix = (suffix << 1) | self.c.bypass_m(Cat::CoeffRemaining, &mc);
        }
        let value = if prefix < 3 {
            (prefix << rice) + suffix
        } else {
            (((1 << (prefix - 3)) + 3 - 1) << rice) + suffix
        };
        Ok((value, prefix))
    }
}

/// Quantise a neighbourhood magnitude sum.
fn qs(v: u32) -> u32 {
    match v {
        0..=4 => v,
        5..=6 => 5,
        7..=9 => 6,
        10..=14 => 7,
        15..=24 => 8,
        25..=39 => 9,
        40..=69 => 10,
        70..=119 => 11,
        120..=219 => 12,
        _ => 13,
    }
}
