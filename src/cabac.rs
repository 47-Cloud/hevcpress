//! CABAC arithmetic decoder (H.265 9.3) with per-syntax-element bit accounting.

use crate::cabac_enc::CabacEnc;
use crate::coder::{Decoder, Encoder};
use crate::model::{cost_bits, Mc, Model};
use std::sync::OnceLock;

/// Context index layout (same order as the init tables below).
pub mod ctx {
    pub const SAO_MERGE: usize = 0;
    pub const SAO_TYPE: usize = 1;
    pub const SPLIT_CU: usize = 2;
    pub const TQ_BYPASS: usize = 5;
    pub const SKIP: usize = 6;
    pub const CU_QP_DELTA: usize = 9;
    pub const PRED_MODE: usize = 12;
    pub const PART_MODE: usize = 13;
    pub const PREV_INTRA_LUMA: usize = 17;
    pub const INTRA_CHROMA: usize = 18;
    pub const MERGE_FLAG: usize = 20;
    pub const MERGE_IDX: usize = 21;
    pub const INTER_PRED_IDC: usize = 22;
    pub const REF_IDX_L0: usize = 27;
    pub const REF_IDX_L1: usize = 29;
    pub const MVD_GT0: usize = 31;
    pub const MVD_GT1: usize = 33;
    pub const MVP_FLAG: usize = 35;
    pub const NO_RESIDUAL: usize = 36;
    pub const SPLIT_TRANSFORM: usize = 37;
    pub const CBF_LUMA: usize = 40;
    pub const CBF_CHROMA: usize = 42;
    pub const TRANSFORM_SKIP: usize = 47;
    pub const EXPLICIT_RDPCM: usize = 49;
    pub const EXPLICIT_RDPCM_DIR: usize = 51;
    pub const LAST_X_PREFIX: usize = 53;
    pub const LAST_Y_PREFIX: usize = 71;
    pub const CODED_SUB_BLOCK: usize = 89;
    pub const SIG_COEFF: usize = 93;
    pub const GT1: usize = 137;
    pub const GT2: usize = 161;
    pub const LOG2_RES_SCALE: usize = 167;
    pub const RES_SCALE_SIGN: usize = 175;
    pub const CU_CHROMA_QP_OFFSET_FLAG: usize = 177;
    pub const CU_CHROMA_QP_OFFSET_IDX: usize = 178;
    pub const COUNT: usize = 179;
}

use ctx::COUNT as NUM_CTX;

const CNU: u8 = 154;

/// initValue per initType (0 = I, 1 and 2 = P/B).
#[rustfmt::skip]
static INIT_VALUES: [[u8; NUM_CTX]; 3] = [
    [
        153, 200, 139, 141, 157, 154, CNU, CNU, CNU, 154, 154, 154, CNU,
        184, CNU, CNU, CNU, 184, 63, 139, CNU, CNU, CNU, CNU, CNU, CNU, CNU,
        CNU, CNU, CNU, CNU, CNU, CNU, CNU, CNU, CNU, CNU, 153, 138, 138, 111, 141,
        94, 138, 182, 154, 154, 139, 139, 139, 139, 139, 139,
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
        91, 171, 134, 141,
        111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153,
        125, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140,
        139, 182, 182, 152, 136, 152, 136, 153, 136, 139, 111, 136, 139, 111,
        141, 111,
        140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107,
        122, 152, 140, 179, 166, 182, 140, 227, 122, 197,
        138, 153, 136, 167, 152, 152,
        154, 154, 154, 154, 154, 154, 154, 154,
        154, 154,
        154,
        154,
    ],
    [
        153, 185, 107, 139, 126, 154, 197, 185, 201, 154, 154, 154, 149,
        154, 139, 154, 154, 154, 152, 139, 110, 122, 95, 79, 63, 31, 31,
        153, 153, 153, 153, 140, 198, 140, 198, 168, 79, 124, 138, 94, 153, 111,
        149, 107, 167, 154, 154, 139, 139, 139, 139, 139, 139,
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95, 94, 108, 123, 108,
        125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95, 94, 108, 123, 108,
        121, 140, 61, 154,
        155, 154, 139, 153, 139, 123, 123, 63, 153, 166, 183, 140, 136, 153,
        154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170,
        153, 123, 123, 107, 121, 107, 121, 167, 151, 183, 140, 151, 183, 140,
        140, 140,
        154, 196, 196, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121,
        136, 137, 169, 194, 166, 167, 154, 167, 137, 182,
        107, 167, 91, 122, 107, 167,
        154, 154, 154, 154, 154, 154, 154, 154,
        154, 154,
        154,
        154,
    ],
    [
        153, 160, 107, 139, 126, 154, 197, 185, 201, 154, 154, 154, 134,
        154, 139, 154, 154, 183, 152, 139, 154, 137, 95, 79, 63, 31, 31,
        153, 153, 153, 153, 169, 198, 169, 198, 168, 79, 224, 167, 122, 153, 111,
        149, 92, 167, 154, 154, 139, 139, 139, 139, 139, 139,
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111, 79, 108, 123, 93,
        125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111, 79, 108, 123, 93,
        121, 140, 61, 154,
        170, 154, 139, 153, 139, 123, 123, 63, 124, 166, 183, 140, 136, 153,
        154, 166, 183, 140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170,
        153, 138, 138, 122, 121, 122, 121, 167, 151, 183, 140, 151, 183, 140,
        140, 140,
        154, 196, 167, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121,
        136, 122, 169, 208, 166, 167, 154, 152, 167, 182,
        107, 167, 91, 107, 107, 167,
        154, 154, 154, 154, 154, 154, 154, 154,
        154, 154,
        154,
        154,
    ],
];

#[rustfmt::skip]
static RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128,176,208,240],[128,167,197,227],[128,158,187,216],[123,150,178,205],[116,142,169,195],[111,135,160,185],[105,128,152,175],[100,122,144,166],
    [95,116,137,158],[90,110,130,150],[85,104,123,142],[81,99,117,135],[77,94,111,128],[73,89,105,122],[69,85,100,116],[66,80,95,110],
    [62,76,90,104],[59,72,86,99],[56,69,81,94],[53,65,77,89],[51,62,73,85],[48,59,69,80],[46,56,66,76],[43,53,63,72],
    [41,50,59,69],[39,48,56,65],[37,45,54,62],[35,43,51,59],[33,41,48,56],[32,39,46,53],[30,37,43,50],[29,35,41,48],
    [27,33,39,45],[26,31,37,43],[24,30,35,41],[23,28,33,39],[22,27,32,37],[21,26,30,35],[20,24,29,33],[19,23,27,31],
    [18,22,26,30],[17,21,25,28],[16,20,23,27],[15,19,22,25],[14,18,21,24],[14,17,20,23],[13,16,19,22],[12,15,18,21],
    [12,14,17,20],[11,14,16,19],[11,13,15,18],[10,12,15,17],[10,12,14,16],[9,11,13,15],[9,11,12,14],[8,10,12,14],
    [8,9,11,13],[7,9,11,12],[7,9,10,12],[7,8,10,11],[6,8,9,11],[6,7,9,10],[6,7,8,9],[2,2,2,2],
];

#[rustfmt::skip]
static TRANS_IDX_LPS: [u8; 64] = [
    0,0,1,2,2,4,4,5,6,7,8,9,9,11,11,12,13,13,15,15,16,16,18,18,19,19,21,21,22,22,23,24,
    24,25,26,26,27,27,28,29,29,30,30,30,31,32,32,33,33,33,34,34,35,35,35,36,36,36,37,37,37,38,38,63,
];

/// Syntax element categories for bit accounting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Cat {
    SaoMerge,
    SaoType,
    SaoOffset,
    SaoBand,
    SaoEo,
    EndOfSlice,
    SplitCu,
    TqBypass,
    SkipFlag,
    PredMode,
    PartMode,
    PrevIntraLuma,
    MpmIdx,
    RemIntraMode,
    IntraChroma,
    MergeFlag,
    MergeIdx,
    InterPredIdc,
    RefIdx,
    MvdGt0,
    MvdGt1,
    MvdRem,
    MvdSign,
    MvpFlag,
    NoResidual,
    SplitTransform,
    CbfLuma,
    CbfChroma,
    CuQpDelta,
    ChromaQpOffset,
    TransformSkip,
    LastPrefix,
    LastSuffix,
    CodedSubBlock,
    SigCoeff,
    Gt1,
    Gt2,
    CoeffSign,
    CoeffRemaining,
    Pcm,
}

pub const NCAT: usize = Cat::Pcm as usize + 1;

pub const CAT_NAMES: [&str; NCAT] = [
    "sao_merge", "sao_type_idx", "sao_offset_abs", "sao_band_position", "sao_eo_class", "end_of_slice",
    "split_cu_flag", "cu_transquant_bypass", "cu_skip_flag", "pred_mode_flag", "part_mode",
    "prev_intra_luma_pred", "mpm_idx", "rem_intra_luma_pred", "intra_chroma_pred_mode", "merge_flag",
    "merge_idx", "inter_pred_idc", "ref_idx", "mvd_greater0", "mvd_greater1", "mvd_abs_minus2", "mvd_sign",
    "mvp_flag", "rqt_root_cbf", "split_transform_flag", "cbf_luma", "cbf_cb_cr", "cu_qp_delta",
    "cu_chroma_qp_offset", "transform_skip_flag", "last_sig_coeff_prefix", "last_sig_coeff_suffix",
    "coded_sub_block_flag", "sig_coeff_flag", "coeff_abs_greater1", "coeff_abs_greater2", "coeff_sign",
    "coeff_abs_level_remaining", "pcm_flag",
];

/// Bits spent, split by whether the bin was context coded or bypass.
#[derive(Clone)]
pub struct Stats {
    pub ctx_bins: [u64; NCAT],
    pub ctx_bits: [f64; NCAT],
    pub bypass_bins: [u64; NCAT],
    /// Bits the experimental model spends (equal to CABAC's for unmodelled bins).
    pub model_bits: [f64; NCAT],
}

impl Default for Stats {
    fn default() -> Self {
        Stats { ctx_bins: [0; NCAT], ctx_bits: [0.0; NCAT], bypass_bins: [0; NCAT], model_bits: [0.0; NCAT] }
    }
}

impl Stats {
    pub fn add(&mut self, o: &Stats) {
        for i in 0..NCAT {
            self.ctx_bins[i] += o.ctx_bins[i];
            self.ctx_bits[i] += o.ctx_bits[i];
            self.bypass_bins[i] += o.bypass_bins[i];
            self.model_bits[i] += o.model_bits[i];
        }
    }

    pub fn total_bits(&self) -> f64 {
        self.ctx_bits.iter().sum::<f64>() + self.bypass_bins.iter().sum::<u64>() as f64
    }
}

fn log2_table() -> &'static [f32; 512] {
    static T: OnceLock<[f32; 512]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f32; 512];
        for (i, v) in t.iter_mut().enumerate().skip(1) {
            *v = (i as f32).log2();
        }
        t
    })
}

/// Where bins come from and go to.
pub enum Io<'a> {
    /// Decode the original CABAC data and gather statistics.
    Analyze,
    /// Decode the original CABAC data, model-encoding every bin.
    Compress(&'a mut Encoder),
    /// Decode bins from the model stream and re-encode them as CABAC.
    Decompress(&'a mut Decoder),
    /// Replay recorded bins and re-encode them as CABAC (no model involved).
    Replay(&'a [u8], usize),
}

pub struct Cabac<'a> {
    io: Io<'a>,
    data: &'a [u8],
    /// Bits consumed from `data`, counted from its start.
    pos: usize,
    range: u32,
    offset: u32,
    enc: CabacEnc,
    state: [u8; NUM_CTX],
    mps: [u8; NUM_CTX],
    pub stats: Stats,
    log2: &'static [f32; 512],
    model: Option<&'a mut Model>,
    /// When set (Analyze), every bin is appended here.
    pub rec: Option<Vec<u8>>,
}

/// Saved context variables, for dependent slice segments.
#[derive(Clone)]
pub struct SavedState {
    state: [u8; NUM_CTX],
    mps: [u8; NUM_CTX],
}

impl<'a> Cabac<'a> {
    pub fn new(io: Io<'a>, data: &'a [u8], model: Option<&'a mut Model>) -> Self {
        let decoding = matches!(io, Io::Analyze | Io::Compress(_));
        let mut c = Cabac {
            io,
            data,
            pos: 0,
            range: 510,
            offset: 0,
            enc: CabacEnc::new(),
            state: [0; NUM_CTX],
            mps: [0; NUM_CTX],
            stats: Stats::default(),
            log2: log2_table(),
            model,
            rec: None,
        };
        if decoding {
            c.init_engine();
        }
        c
    }

    fn decoding(&self) -> bool {
        matches!(self.io, Io::Analyze | Io::Compress(_))
    }

    fn init_engine(&mut self) {
        self.range = 510;
        self.offset = 0;
        for _ in 0..9 {
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    /// Initialise context variables for a slice.
    pub fn init_contexts(&mut self, init_type: usize, slice_qp: i32) {
        let qp = slice_qp.clamp(0, 51);
        for i in 0..NUM_CTX {
            let v = INIT_VALUES[init_type][i] as i32;
            let m = (v >> 4) * 5 - 45;
            let n = ((v & 15) << 3) - 16;
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            self.mps[i] = (pre > 63) as u8;
            self.state[i] = if pre > 63 { (pre - 64) as u8 } else { (63 - pre) as u8 };
        }
    }

    pub fn save_contexts(&self) -> SavedState {
        SavedState { state: self.state, mps: self.mps }
    }

    pub fn restore_contexts(&mut self, s: &SavedState) {
        self.state = s.state;
        self.mps = s.mps;
    }

    /// Start a new substream at the next byte boundary (wavefront rows).
    pub fn align_and_reinit(&mut self) {
        if self.decoding() {
            self.pos = self.pos.div_ceil(8) * 8;
            self.init_engine();
        } else {
            self.enc.align_and_restart();
        }
    }

    /// Finish an encoding-side slice and return the regenerated slice data.
    pub fn finish_encoded(mut self) -> Vec<u8> {
        self.enc.align_and_restart();
        self.enc.into_bytes()
    }

    /// Bits consumed so far, counted from the start of the data.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    fn read_bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        b as u32
    }

    fn renorm(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    fn record(&mut self, bin: u32) {
        if let Some(r) = self.rec.as_mut() {
            r.push(bin as u8);
        }
    }

    /// Obtain the next bin from the non-CABAC source (decompress / replay).
    fn external_bin(&mut self, p16: u32) -> u32 {
        match &mut self.io {
            Io::Decompress(d) => d.decode(p16),
            Io::Replay(bins, i) => {
                let b = bins.get(*i).copied().unwrap_or(0) as u32;
                *i += 1;
                b
            }
            _ => unreachable!(),
        }
    }

    fn model_predict(&mut self, cat: Cat, mc: &Mc) -> u32 {
        match self.model.as_mut() {
            Some(m) => m.predict(cat as usize, mc),
            None => 32768,
        }
    }

    /// Model-side bookkeeping once the bin is known.
    fn model_finish(&mut self, bin: u32, p16: u32, modelled: bool) {
        if let Io::Compress(e) = &mut self.io {
            e.encode(bin, p16);
        }
        if modelled {
            if let Some(m) = self.model.as_mut() {
                m.update(bin);
            }
        }
    }

    fn code_decision(&mut self, ctx: usize, cat: Cat, mc: Option<&Mc>) -> u32 {
        let st = self.state[ctx] as usize;
        let mps = self.mps[ctx] as u32;
        let decoding = self.decoding();
        let range = if decoding { self.range } else { self.enc.range() };
        let lps = RANGE_TAB_LPS[st][((range >> 6) & 3) as usize] as u32;

        let p16 = match mc {
            Some(mc) => self.model_predict(cat, mc),
            None => {
                let p1 = if mps == 1 { range - lps } else { lps };
                ((p1 as u64 * 65536 / range as u64) as u32).clamp(16, 65520)
            }
        };

        let (bin, is_lps);
        if decoding {
            let range_before = self.range;
            self.range -= lps;
            if self.offset >= self.range {
                self.offset -= self.range;
                self.range = lps;
                is_lps = true;
            } else {
                is_lps = false;
            }
            bin = if is_lps { 1 - mps } else { mps };
            if matches!(self.io, Io::Analyze) {
                let cost = if is_lps {
                    self.log2[range_before as usize] - self.log2[lps as usize]
                } else {
                    self.log2[range_before as usize] - self.log2[self.range as usize]
                } as f64;
                let c = cat as usize;
                self.stats.ctx_bins[c] += 1;
                self.stats.ctx_bits[c] += cost;
                self.stats.model_bits[c] += if mc.is_some() { cost_bits(p16, bin) } else { cost };
            }
            self.renorm();
        } else {
            bin = self.external_bin(p16);
            is_lps = bin != mps;
            self.enc.encode(lps, is_lps);
        }
        if is_lps {
            if st == 0 {
                self.mps[ctx] ^= 1;
            }
            self.state[ctx] = TRANS_IDX_LPS[st];
        } else if st < 62 {
            self.state[ctx] += 1;
        }
        self.model_finish(bin, p16, mc.is_some());
        self.record(bin);
        bin
    }

    pub fn decision(&mut self, ctx: usize, cat: Cat) -> u32 {
        self.code_decision(ctx, cat, None)
    }

    /// A context-coded bin that the model predicts with richer contexts.
    pub fn decision_m(&mut self, ctx: usize, cat: Cat, mc: &Mc) -> u32 {
        self.code_decision(ctx, cat, Some(mc))
    }

    fn code_bypass(&mut self, cat: Cat, mc: Option<&Mc>) -> u32 {
        let p16 = match mc {
            Some(mc) => self.model_predict(cat, mc),
            None => 32768,
        };
        let bin;
        if self.decoding() {
            self.offset = (self.offset << 1) | self.read_bit();
            bin = if self.offset >= self.range {
                self.offset -= self.range;
                1
            } else {
                0
            };
        } else {
            bin = self.external_bin(p16);
            self.enc.encode_bypass(bin);
        }
        if matches!(self.io, Io::Analyze) {
            let c = cat as usize;
            self.stats.bypass_bins[c] += 1;
            self.stats.model_bits[c] += if mc.is_some() { cost_bits(p16, bin) } else { 1.0 };
        }
        self.model_finish(bin, p16, mc.is_some());
        self.record(bin);
        bin
    }

    pub fn bypass(&mut self, cat: Cat) -> u32 {
        self.code_bypass(cat, None)
    }

    /// A bypass bin that the model predicts with richer contexts.
    pub fn bypass_m(&mut self, cat: Cat, mc: &Mc) -> u32 {
        self.code_bypass(cat, Some(mc))
    }

    /// `n` bypass bins, MSB first.
    pub fn bypass_bits(&mut self, n: u32, cat: Cat) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bypass(cat);
        }
        v
    }

    pub fn terminate(&mut self, cat: Cat) -> u32 {
        let decoding = self.decoding();
        let range_before = if decoding { self.range } else { self.enc.range() };
        let p16 = (2 * 65536 / range_before).clamp(16, 65520);
        let bin;
        if decoding {
            self.range -= 2;
            bin = (self.offset >= self.range) as u32;
            if matches!(self.io, Io::Analyze) {
                let cost = if bin == 1 {
                    self.log2[range_before as usize] - 1.0
                } else {
                    self.log2[range_before as usize] - self.log2[self.range as usize]
                } as f64;
                let c = cat as usize;
                self.stats.ctx_bins[c] += 1;
                self.stats.ctx_bits[c] += cost;
                self.stats.model_bits[c] += cost;
            }
            if bin == 0 {
                self.renorm();
            }
        } else {
            bin = self.external_bin(p16);
            self.enc.encode_terminate(bin);
        }
        self.model_finish(bin, p16, false);
        self.record(bin);
        bin
    }
}
