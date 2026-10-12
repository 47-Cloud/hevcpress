//! Adaptive context-mixing bin model. Everything is integer arithmetic with
//! hard-coded tables, so predictions are bit-exact on every platform: the
//! decompressor must reproduce the compressor's probabilities exactly.

use crate::cabac::NCAT;
use crate::tables::{SQUASH, STRETCH};

pub const MAXIN: usize = 14;
const NIN: usize = 2 * MAXIN + 1;

/// A set of hashed contexts (one per model input) plus a mixer weight-set selector.
pub struct Mc {
    pub n: usize,
    pub h: [u32; MAXIN],
    pub sel: usize,
}

impl Mc {
    pub fn new(sel: usize) -> Mc {
        Mc { n: 0, h: [0; MAXIN], sel }
    }

    pub fn add(&mut self, parts: &[u32]) -> &mut Self {
        self.h[self.n] = hh(parts);
        self.n += 1;
        self
    }
}

#[inline]
pub fn hh(parts: &[u32]) -> u32 {
    let mut x = 0x9E37_79B1u32;
    for &v in parts {
        x = (x ^ v).wrapping_mul(0x85EB_CA6B);
        x ^= x >> 15;
        x = x.wrapping_mul(0xC2B2_AE35);
    }
    x ^ (x >> 13)
}

const TBITS: u32 = 22;
const NSEL: usize = 1024;
const LIMIT: u16 = 255;
const FAST_LIMIT: u16 = 30;
const LR_MIN: i64 = 197; // 0.003 in 16.16
const LR_BOOST: i64 = 3277; // 0.05 in 16.16
const W_INIT: i32 = 9830; // 0.15 in 16.16
const BIAS: i32 = 102; // 0.4 in 1/256 units

#[derive(Clone, Copy)]
struct Entry {
    /// Slow and fast estimates of P(bin = 1), 16 bit.
    p: u16,
    f: u16,
    n: u16,
}

/// Mixer learning-rate multiplier in sixteenths, per syntax element category.
fn lr_scale(cat: usize) -> i64 {
    16 + 0 * cat as i64
}

#[derive(Default)]
struct Scratch {
    cat: usize,
    set: usize,
    n: usize,
    idx: [u32; MAXIN],
    st: [i32; NIN],
    p: u32,
}

pub struct Model {
    t: Vec<Entry>,
    w: Vec<i32>,
    wn: Vec<u32>,
    recip: Vec<i32>,
    s: Scratch,
}

#[inline]
fn stretch(p16: u16) -> i32 {
    STRETCH[(p16 >> 4) as usize] as i32
}

impl Model {
    pub fn new() -> Model {
        Model {
            t: vec![Entry { p: 32768, f: 32768, n: 0 }; 1 << TBITS],
            w: (0..NCAT * NSEL * NIN)
                .map(|i| if i / (NSEL * NIN) == crate::cabac::Cat::CoeffSign as usize { W_INIT / 6 } else { W_INIT })
                .collect(),
            wn: vec![0; NCAT * NSEL],
            recip: (0..=LIMIT as i32).map(|n| 65536 / (n + 2)).collect(),
            s: Scratch::default(),
        }
    }

    /// Probability (16 bit, in [16, 65520]) that the next bin is 1.
    pub fn predict(&mut self, cat: usize, mc: &Mc) -> u32 {
        let set = cat * NSEL + (mc.sel % NSEL);
        let wb = set * NIN;
        let s = &mut self.s;
        s.cat = cat;
        s.set = set;
        s.n = mc.n;
        let mut dot: i64 = 0;
        for i in 0..mc.n {
            let k = hh(&[cat as u32, i as u32, mc.h[i]]) >> (32 - TBITS);
            s.idx[i] = k;
            let e = self.t[k as usize];
            s.st[2 * i] = stretch(e.p);
            s.st[2 * i + 1] = stretch(e.f);
        }
        let ni = 2 * mc.n;
        s.st[ni] = BIAS;
        for i in 0..=ni {
            dot += self.w[wb + i] as i64 * s.st[i] as i64;
        }
        let d = ((dot >> 16) as i32).clamp(-3071, 3071);
        s.p = (SQUASH[(d + 3072) as usize] as u32).clamp(16, 65520);
        s.p
    }

    /// Learn from the actual bin of the last `predict`.
    pub fn update(&mut self, bin: u32) {
        let s = &self.s;
        let wb = s.set * NIN;
        let ni = 2 * s.n;
        let err = ((bin as i64) << 16) - s.p as i64;
        let cnt = self.wn[s.set];
        self.wn[s.set] = cnt.saturating_add(1);
        let lr = (LR_MIN + LR_BOOST * 50 / (50 + cnt as i64)) * lr_scale(s.cat) / 16;
        for i in 0..=ni {
            self.w[wb + i] += ((lr * err * s.st[i] as i64) >> 24) as i32;
        }
        let target: i64 = if bin == 1 { 65535 } else { 0 };
        for i in 0..s.n {
            let e = &mut self.t[s.idx[i] as usize];
            let r = self.recip[e.n as usize] as i64;
            let rf = self.recip[e.n.min(FAST_LIMIT) as usize] as i64;
            e.p = (e.p as i64 + (((target - e.p as i64) * r) >> 16)) as u16;
            e.f = (e.f as i64 + (((target - e.f as i64) * rf) >> 16)) as u16;
            if e.n < LIMIT {
                e.n += 1;
            }
        }
    }
}

/// Ideal cost in bits of coding `bin` with probability `p16` of a 1.
pub fn cost_bits(p16: u32, bin: u32) -> f64 {
    let p = if bin == 1 { p16 as f64 } else { 65536.0 - p16 as f64 } / 65536.0;
    -p.log2()
}
