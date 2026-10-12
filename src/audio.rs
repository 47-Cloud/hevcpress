//! Lossless coder for 16-bit PCM audio: a cascade of integer NLMS predictors
//! whose residual is coded bin by bin with the shared context-mixing model.
//! All arithmetic is integer so the decoder reproduces the predictor exactly.

use crate::cabac::Cat;
use crate::coder::{Decoder, Encoder};
use crate::model::{Mc, Model};

const MAXK: u32 = 17;

/// One normalised-LMS stage with 16.16 fixed-point weights.
struct Stage {
    w: Vec<i32>,
    /// History, newest first.
    h: Vec<i32>,
    energy: i64,
    mu: i64,
}

impl Stage {
    fn new(n: usize, mu: i64) -> Stage {
        Stage { w: vec![0; n], h: vec![0; n], energy: 0, mu }
    }

    fn predict(&self) -> i32 {
        let mut s: i64 = 0;
        for i in 0..self.w.len() {
            s += self.w[i] as i64 * self.h[i] as i64;
        }
        (s >> 16) as i32
    }

    /// `err` is the error of this stage's prediction; `input` is the new
    /// value pushed into its history.
    fn update(&mut self, err: i32, input: i32) {
        let denom = self.energy + 1024;
        for i in 0..self.w.len() {
            self.w[i] += ((self.mu * err as i64 * self.h[i] as i64) / denom) as i32;
        }
        let n = self.h.len();
        let old = self.h[n - 1] as i64;
        self.energy -= old * old;
        self.h.copy_within(0..n - 1, 1);
        self.h[0] = input;
        self.energy += input as i64 * input as i64;
    }
}

struct Predictor {
    s1: Stage,
    s2: Stage,
    p1: i32,
    p2: i32,
}

impl Predictor {
    fn new() -> Predictor {
        Predictor { s1: Stage::new(16, 6000), s2: Stage::new(16, 8000), p1: 0, p2: 0 }
    }

    fn predict(&mut self) -> i32 {
        self.p1 = self.s1.predict();
        self.p2 = self.s2.predict();
        (self.p1 + self.p2).clamp(-32768, 32767)
    }

    fn update(&mut self, x: i32) {
        let e1 = x - self.p1;
        self.s1.update(e1, x);
        let e2 = e1 - self.p2;
        self.s2.update(e2, e1);
    }
}

enum Io<'a> {
    Enc(&'a mut Encoder),
    Dec(&'a mut Decoder),
}

/// Per-channel adaptive state for the residual coder.
#[derive(Default, Clone)]
struct ChanCtx {
    k1: u32,
    k2: u32,
    kavg: u32,
    s1: u32,
    s2: u32,
}

struct Coder<'a> {
    io: Io<'a>,
    model: &'a mut Model,
}

impl<'a> Coder<'a> {
    fn bit(&mut self, mc: &Mc, bin: u32) -> u32 {
        let p = self.model.predict(Cat::Pcm as usize, mc);
        let b = match &mut self.io {
            Io::Enc(e) => {
                e.encode(bin, p);
                bin
            }
            Io::Dec(d) => d.decode(p),
        };
        self.model.update(b);
        b
    }

    fn raw_bit(&mut self, bin: u32) -> u32 {
        match &mut self.io {
            Io::Enc(e) => {
                e.encode(bin, 32768);
                bin
            }
            Io::Dec(d) => d.decode(32768),
        }
    }

    /// Code one residual (encoder passes the value, decoder ignores it).
    fn residual(&mut self, c: &mut ChanCtx, ch: u32, r: i32) -> i32 {
        let u = r.unsigned_abs();
        let k_in = 32 - u.leading_zeros();
        let mut k = 0u32;
        while k < MAXK {
            let mut mc = Mc::new(k as usize);
            mc.add(&[1, ch, k, c.k1]);
            mc.add(&[2, ch, k, c.k1, c.k2]);
            mc.add(&[3, ch, k, c.kavg >> 2]);
            mc.add(&[4, ch, k, c.kavg >> 3, c.k1]);
            mc.add(&[5, ch, k, c.k1, c.s1]);
            if self.bit(&mc, (k_in > k) as u32) == 0 {
                break;
            }
            k += 1;
        }
        let mut val = 0u32;
        if k >= 1 {
            val = 1;
            for j in (0..k - 1).rev() {
                let bit_in = (u >> j) & 1;
                let depth = k - 1 - j; // 1 = first bit below the leading one
                let bit = if depth <= 2 {
                    let mut mc = Mc::new(32 + depth as usize);
                    mc.add(&[1, ch, k, depth, val]);
                    mc.add(&[2, ch, k, depth, val, c.k1]);
                    self.bit(&mc, bit_in)
                } else {
                    self.raw_bit(bit_in)
                };
                val = (val << 1) | bit;
            }
        }
        let mut neg = 0;
        if val != 0 {
            let mut mc = Mc::new(40);
            mc.add(&[1, ch, c.s1, c.s2]);
            neg = self.bit(&mc, (r < 0) as u32);
        }
        c.k2 = c.k1;
        c.k1 = k;
        c.kavg = c.kavg - (c.kavg >> 3) + (k << 2);
        c.s2 = c.s1;
        c.s1 = if val == 0 { 0 } else { 1 + neg };
        if neg == 1 { -(val as i32) } else { val as i32 }
    }
}

/// Encode interleaved samples with `channels` channels.
pub fn encode(samples: &[i16], channels: usize, model: &mut Model, enc: &mut Encoder) {
    let mut coder = Coder { io: Io::Enc(enc), model };
    let mut preds: Vec<Predictor> = (0..channels).map(|_| Predictor::new()).collect();
    let mut ctxs = vec![ChanCtx::default(); channels];
    for (i, &x) in samples.iter().enumerate() {
        let ch = i % channels;
        let p = preds[ch].predict();
        coder.residual(&mut ctxs[ch], ch as u32, x as i32 - p);
        preds[ch].update(x as i32);
    }
}

pub fn decode(n: usize, channels: usize, model: &mut Model, dec: &mut Decoder) -> Vec<i16> {
    let mut coder = Coder { io: Io::Dec(dec), model };
    let mut preds: Vec<Predictor> = (0..channels).map(|_| Predictor::new()).collect();
    let mut ctxs = vec![ChanCtx::default(); channels];
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let ch = i % channels;
        let p = preds[ch].predict();
        let r = coder.residual(&mut ctxs[ch], ch as u32, 0);
        let x = (p + r).clamp(-32768, 32767);
        preds[ch].update(x);
        out.push(x as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut state = 7u32;
        let samples: Vec<i16> = (0..5000)
            .map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((state >> 20) as i32 & 63) - 32;
                ((i as f64 * 0.05).sin() * 8000.0) as i32 as i16 + noise as i16
            })
            .collect();
        let mut model = Model::new();
        let mut enc = Encoder::new();
        encode(&samples, 1, &mut model, &mut enc);
        let bytes = enc.finish();
        let mut model = Model::new();
        let mut dec = Decoder::new(bytes);
        assert_eq!(decode(samples.len(), 1, &mut model, &mut dec), samples);
    }
}
