//! HEVC parameter sets (SPS, PPS) and slice segment headers, parsed far
//! enough to drive slice-data parsing.

use crate::bits::{unescape, Bits};
use std::collections::HashMap;

/// A short-term reference picture set: (delta POC, used by current picture).
#[derive(Clone, Debug, Default)]
pub struct StRps {
    pub s0: Vec<(i32, bool)>,
    pub s1: Vec<(i32, bool)>,
}

impl StRps {
    pub fn num_delta_pocs(&self) -> usize {
        self.s0.len() + self.s1.len()
    }

    pub fn num_used(&self) -> usize {
        self.s0.iter().chain(&self.s1).filter(|d| d.1).count()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sps {
    pub id: u32,
    pub max_sub_layers_minus1: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub log2_max_poc_lsb: u32,
    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_th_depth_inter: u32,
    pub max_th_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub amp: bool,
    pub sao: bool,
    pub pcm: bool,
    pub pcm_bit_depth_luma: u32,
    pub pcm_bit_depth_chroma: u32,
    pub log2_min_pcm_cb: u32,
    pub log2_max_pcm_cb: u32,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<StRps>,
    pub long_term_ref_pics_present: bool,
    /// (poc lsb, used by current picture)
    pub lt_ref_pics: Vec<(u32, bool)>,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    // Range extension.
    pub transform_skip_rotation: bool,
    pub transform_skip_context: bool,
    pub implicit_rdpcm: bool,
    pub explicit_rdpcm: bool,
    pub extended_precision: bool,
    pub intra_smoothing_disabled: bool,
    pub high_precision_offsets: bool,
    pub persistent_rice: bool,
    pub cabac_bypass_alignment: bool,
}

impl Sps {
    /// 0 for monochrome (or separate colour planes), else `chroma_format_idc`.
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane { 0 } else { self.chroma_format_idc }
    }

    pub fn ctb_width(&self) -> u32 {
        self.width.div_ceil(1 << self.log2_ctb)
    }

    pub fn ctb_height(&self) -> u32 {
        self.height.div_ceil(1 << self.log2_ctb)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_control_present: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub lists_modification_present: bool,
    pub slice_header_extension_present: bool,
    // Range extension.
    pub log2_max_transform_skip_size: u32,
    pub cross_component_prediction: bool,
    pub chroma_qp_offset_list_enabled: bool,
    pub diff_cu_chroma_qp_offset_depth: u32,
    pub chroma_qp_offset_list_len_minus1: u32,
}

#[derive(Default)]
pub struct ParamSets {
    pub sps: HashMap<u32, Sps>,
    pub pps: HashMap<u32, Pps>,
}

impl ParamSets {
    /// Absorb a parameter-set NAL (including its 2-byte header). Other NAL
    /// types are ignored. Returns a description if a parameter set failed to parse.
    pub fn absorb(&mut self, nal: &[u8]) -> Option<String> {
        if nal.len() < 3 {
            return None;
        }
        match (nal[0] >> 1) & 0x3f {
            33 => match parse_sps(&unescape(&nal[2..])) {
                Ok(s) => {
                    self.sps.insert(s.id, s);
                }
                Err(e) => return Some(format!("SPS: {e}")),
            },
            34 => match parse_pps(&unescape(&nal[2..])) {
                Ok(p) => {
                    self.pps.insert(p.id, p);
                }
                Err(e) => return Some(format!("PPS: {e}")),
            },
            _ => {}
        }
        None
    }
}

fn profile_tier_level(r: &mut Bits, max_sub_layers_minus1: u32) {
    r.skip(96);
    let mut flags = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        flags.push((r.bit() == 1, r.bit() == 1));
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize));
    }
    for (profile, level) in flags {
        if profile {
            r.skip(88);
        }
        if level {
            r.skip(8);
        }
    }
}

fn parse_st_rps(r: &mut Bits, idx: usize, num_sets: usize, sets: &[StRps]) -> Result<StRps, String> {
    let inter = idx != 0 && r.bit() == 1;
    if inter {
        let delta_idx = if idx == num_sets { r.ue() as usize + 1 } else { 1 };
        if delta_idx > idx {
            return Err("bad delta_idx in st_ref_pic_set".into());
        }
        let rf = &sets[idx - delta_idx];
        let sign = r.bit() as i32;
        let delta_rps = (1 - 2 * sign) * (r.ue() as i32 + 1);
        let n = rf.num_delta_pocs();
        let (nneg, npos) = (rf.s0.len(), rf.s1.len());
        let mut used = vec![false; n + 1];
        let mut use_delta = vec![true; n + 1];
        for j in 0..=n {
            used[j] = r.bit() == 1;
            if !used[j] {
                use_delta[j] = r.bit() == 1;
            }
        }
        let mut s0 = Vec::new();
        for j in (0..npos).rev() {
            let d = rf.s1[j].0 + delta_rps;
            if d < 0 && use_delta[nneg + j] {
                s0.push((d, used[nneg + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            s0.push((delta_rps, used[n]));
        }
        for j in 0..nneg {
            let d = rf.s0[j].0 + delta_rps;
            if d < 0 && use_delta[j] {
                s0.push((d, used[j]));
            }
        }
        let mut s1 = Vec::new();
        for j in (0..nneg).rev() {
            let d = rf.s0[j].0 + delta_rps;
            if d > 0 && use_delta[j] {
                s1.push((d, used[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            s1.push((delta_rps, used[n]));
        }
        for j in 0..npos {
            let d = rf.s1[j].0 + delta_rps;
            if d > 0 && use_delta[nneg + j] {
                s1.push((d, used[nneg + j]));
            }
        }
        Ok(StRps { s0, s1 })
    } else {
        let (nn, np) = (r.ue(), r.ue());
        if nn > 16 || np > 16 {
            return Err("too many pictures in st_ref_pic_set".into());
        }
        let mut rps = StRps::default();
        let mut prev = 0i32;
        for _ in 0..nn {
            prev -= r.ue() as i32 + 1;
            rps.s0.push((prev, r.bit() == 1));
        }
        prev = 0;
        for _ in 0..np {
            prev += r.ue() as i32 + 1;
            rps.s1.push((prev, r.bit() == 1));
        }
        Ok(rps)
    }
}

fn skip_scaling_list_data(r: &mut Bits) {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if r.bit() == 0 {
                r.ue(); // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se(); // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    r.se();
                }
            }
            matrix_id += step;
        }
    }
}

fn skip_sub_layer_hrd(r: &mut Bits, cpb_cnt: u32, sub_pic: bool) {
    for _ in 0..cpb_cnt {
        r.ue();
        r.ue();
        if sub_pic {
            r.ue();
            r.ue();
        }
        r.bit();
    }
}

fn skip_hrd(r: &mut Bits, common: bool, max_sub_layers_minus1: u32) {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common {
        nal = r.bit() == 1;
        vcl = r.bit() == 1;
        if nal || vcl {
            sub_pic = r.bit() == 1;
            if sub_pic {
                r.skip(8 + 5 + 1 + 5);
            }
            r.skip(8);
            if sub_pic {
                r.skip(4);
            }
            r.skip(15);
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.bit() == 1;
        let fixed_cvs = fixed_general || r.bit() == 1;
        let mut low_delay = false;
        if fixed_cvs {
            r.ue();
        } else {
            low_delay = r.bit() == 1;
        }
        let cpb_cnt = if low_delay { 1 } else { r.ue() + 1 };
        if nal {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic);
        }
        if vcl {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic);
        }
    }
}

fn skip_vui(r: &mut Bits, max_sub_layers_minus1: u32) {
    if r.bit() == 1 && r.u(8) == 255 {
        r.skip(32); // sar_width, sar_height
    }
    if r.bit() == 1 {
        r.skip(1); // overscan_appropriate_flag
    }
    if r.bit() == 1 {
        r.skip(4); // video_format, video_full_range_flag
        if r.bit() == 1 {
            r.skip(24);
        }
    }
    if r.bit() == 1 {
        r.ue();
        r.ue();
    }
    r.skip(3); // neutral_chroma_indication, field_seq, frame_field_info_present
    if r.bit() == 1 {
        for _ in 0..4 {
            r.ue();
        }
    }
    if r.bit() == 1 {
        r.skip(64);
        if r.bit() == 1 {
            r.ue();
        }
        if r.bit() == 1 {
            skip_hrd(r, true, max_sub_layers_minus1);
        }
    }
    if r.bit() == 1 {
        r.skip(3);
        for _ in 0..5 {
            r.ue();
        }
    }
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps, String> {
    let mut r = Bits::new(rbsp);
    let mut s = Sps::default();
    r.skip(4); // sps_video_parameter_set_id
    s.max_sub_layers_minus1 = r.u(3);
    r.skip(1);
    profile_tier_level(&mut r, s.max_sub_layers_minus1);
    s.id = r.ue();
    s.chroma_format_idc = r.ue();
    if s.chroma_format_idc == 3 {
        s.separate_colour_plane = r.bit() == 1;
    }
    s.width = r.ue();
    s.height = r.ue();
    if r.bit() == 1 {
        for _ in 0..4 {
            r.ue();
        }
    }
    s.bit_depth_luma = r.ue() + 8;
    s.bit_depth_chroma = r.ue() + 8;
    s.log2_max_poc_lsb = r.ue() + 4;
    let first = if r.bit() == 1 { 0 } else { s.max_sub_layers_minus1 };
    for _ in first..=s.max_sub_layers_minus1 {
        r.ue();
        r.ue();
        r.ue();
    }
    s.log2_min_cb = r.ue() + 3;
    s.log2_ctb = s.log2_min_cb + r.ue();
    s.log2_min_tb = r.ue() + 2;
    s.log2_max_tb = s.log2_min_tb + r.ue();
    s.max_th_depth_inter = r.ue();
    s.max_th_depth_intra = r.ue();
    s.scaling_list_enabled = r.bit() == 1;
    if s.scaling_list_enabled && r.bit() == 1 {
        skip_scaling_list_data(&mut r);
    }
    s.amp = r.bit() == 1;
    s.sao = r.bit() == 1;
    s.pcm = r.bit() == 1;
    if s.pcm {
        s.pcm_bit_depth_luma = r.u(4) + 1;
        s.pcm_bit_depth_chroma = r.u(4) + 1;
        s.log2_min_pcm_cb = r.ue() + 3;
        s.log2_max_pcm_cb = s.log2_min_pcm_cb + r.ue();
        s.pcm_loop_filter_disabled = r.bit() == 1;
    }
    let num_st_rps = r.ue() as usize;
    if num_st_rps > 64 {
        return Err("too many short-term RPS".into());
    }
    for i in 0..num_st_rps {
        let rps = parse_st_rps(&mut r, i, num_st_rps, &s.st_rps)?;
        s.st_rps.push(rps);
    }
    s.long_term_ref_pics_present = r.bit() == 1;
    if s.long_term_ref_pics_present {
        let n = r.ue();
        if n > 32 {
            return Err("too many long-term ref pics".into());
        }
        for _ in 0..n {
            let lsb = r.u(s.log2_max_poc_lsb);
            s.lt_ref_pics.push((lsb, r.bit() == 1));
        }
    }
    s.temporal_mvp = r.bit() == 1;
    s.strong_intra_smoothing = r.bit() == 1;
    if r.bit() == 1 {
        skip_vui(&mut r, s.max_sub_layers_minus1);
    }
    if r.bit() == 1 {
        let range = r.bit() == 1;
        r.skip(7); // multilayer, 3d, scc, 4 reserved bits
        if range {
            s.transform_skip_rotation = r.bit() == 1;
            s.transform_skip_context = r.bit() == 1;
            s.implicit_rdpcm = r.bit() == 1;
            s.explicit_rdpcm = r.bit() == 1;
            s.extended_precision = r.bit() == 1;
            s.intra_smoothing_disabled = r.bit() == 1;
            s.high_precision_offsets = r.bit() == 1;
            s.persistent_rice = r.bit() == 1;
            s.cabac_bypass_alignment = r.bit() == 1;
        }
    }
    if r.exhausted() {
        return Err("SPS ran past its end".into());
    }
    if s.log2_ctb > 6 || s.log2_ctb < 4 || s.log2_max_tb > 5 {
        return Err(format!("implausible SPS (ctb {}, max tb {})", s.log2_ctb, s.log2_max_tb));
    }
    Ok(s)
}

pub fn parse_pps(rbsp: &[u8]) -> Result<Pps, String> {
    let mut r = Bits::new(rbsp);
    let mut p = Pps::default();
    p.id = r.ue();
    p.sps_id = r.ue();
    p.dependent_slice_segments_enabled = r.bit() == 1;
    p.output_flag_present = r.bit() == 1;
    p.num_extra_slice_header_bits = r.u(3);
    p.sign_data_hiding = r.bit() == 1;
    p.cabac_init_present = r.bit() == 1;
    p.num_ref_idx_default = [r.ue() + 1, r.ue() + 1];
    p.init_qp_minus26 = r.se();
    p.constrained_intra_pred = r.bit() == 1;
    p.transform_skip_enabled = r.bit() == 1;
    p.cu_qp_delta_enabled = r.bit() == 1;
    if p.cu_qp_delta_enabled {
        p.diff_cu_qp_delta_depth = r.ue();
    }
    p.cb_qp_offset = r.se();
    p.cr_qp_offset = r.se();
    p.slice_chroma_qp_offsets_present = r.bit() == 1;
    p.weighted_pred = r.bit() == 1;
    p.weighted_bipred = r.bit() == 1;
    p.transquant_bypass_enabled = r.bit() == 1;
    p.tiles_enabled = r.bit() == 1;
    p.entropy_coding_sync = r.bit() == 1;
    if p.tiles_enabled {
        let (cols, rows) = (r.ue(), r.ue());
        if r.bit() == 0 {
            for _ in 0..cols {
                r.ue();
            }
            for _ in 0..rows {
                r.ue();
            }
        }
        r.skip(1); // loop_filter_across_tiles_enabled_flag
    }
    p.loop_filter_across_slices = r.bit() == 1;
    p.deblocking_control_present = r.bit() == 1;
    if p.deblocking_control_present {
        p.deblocking_override_enabled = r.bit() == 1;
        p.deblocking_disabled = r.bit() == 1;
        if !p.deblocking_disabled {
            r.se();
            r.se();
        }
    }
    if r.bit() == 1 {
        skip_scaling_list_data(&mut r);
    }
    p.lists_modification_present = r.bit() == 1;
    r.ue(); // log2_parallel_merge_level_minus2
    p.slice_header_extension_present = r.bit() == 1;
    p.log2_max_transform_skip_size = 2;
    if r.bit() == 1 {
        let range = r.bit() == 1;
        r.skip(7);
        if range {
            if p.transform_skip_enabled {
                p.log2_max_transform_skip_size = r.ue() + 2;
            }
            p.cross_component_prediction = r.bit() == 1;
            p.chroma_qp_offset_list_enabled = r.bit() == 1;
            if p.chroma_qp_offset_list_enabled {
                p.diff_cu_chroma_qp_offset_depth = r.ue();
                p.chroma_qp_offset_list_len_minus1 = r.ue();
                for _ in 0..=p.chroma_qp_offset_list_len_minus1 {
                    r.se();
                    r.se();
                }
            }
            r.ue(); // log2_sao_offset_scale_luma
            r.ue(); // log2_sao_offset_scale_chroma
        }
    }
    if r.exhausted() {
        return Err("PPS ran past its end".into());
    }
    Ok(p)
}

#[derive(Clone, Debug, Default)]
pub struct SliceHeader {
    pub first_slice_segment_in_pic: bool,
    pub pps_id: u32,
    pub dependent: bool,
    pub segment_address: u32,
    /// 0 = B, 1 = P, 2 = I.
    pub slice_type: u32,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx_active: [u32; 2],
    pub mvd_l1_zero: bool,
    pub cabac_init_flag: bool,
    pub max_num_merge_cand: u32,
    pub slice_qp: i32,
    pub cu_chroma_qp_offset_enabled: bool,
    pub num_entry_points: u32,
    /// Byte offset of the slice data within the unescaped RBSP.
    pub data_offset: usize,
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

/// Parse a slice segment header from the unescaped RBSP (after the 2-byte NAL header).
/// `prev` supplies the fields a dependent slice segment inherits.
pub fn parse_slice_header(
    nal_type: u8,
    rbsp: &[u8],
    ps: &ParamSets,
    prev: Option<&SliceHeader>,
) -> Result<SliceHeader, String> {
    let mut r = Bits::new(rbsp);
    let first = r.bit() == 1;
    if (16..=23).contains(&nal_type) {
        r.skip(1);
    }
    let pps_id = r.ue();
    let pps = ps.pps.get(&pps_id).ok_or(format!("slice references missing PPS {pps_id}"))?;
    let sps = ps.sps.get(&pps.sps_id).ok_or(format!("PPS references missing SPS {}", pps.sps_id))?;

    let mut dependent = false;
    let mut address = 0;
    if !first {
        if pps.dependent_slice_segments_enabled {
            dependent = r.bit() == 1;
        }
        let pic_size_in_ctbs = sps.ctb_width() * sps.ctb_height();
        address = r.u(ceil_log2(pic_size_in_ctbs));
    }

    let mut sh;
    if dependent {
        sh = prev.ok_or("dependent slice segment without a previous slice")?.clone();
        sh.first_slice_segment_in_pic = false;
        sh.dependent = true;
        sh.segment_address = address;
    } else {
        sh = SliceHeader {
            first_slice_segment_in_pic: first,
            pps_id,
            segment_address: address,
            ..Default::default()
        };
        r.skip(pps.num_extra_slice_header_bits as usize);
        sh.slice_type = r.ue();
        if sh.slice_type > 2 {
            return Err(format!("bad slice_type {}", sh.slice_type));
        }
        if pps.output_flag_present {
            r.skip(1);
        }
        if sps.separate_colour_plane {
            r.skip(2);
        }
        let mut num_pic_total_curr = 0u32;
        let mut temporal_mvp = false;
        if nal_type != 19 && nal_type != 20 {
            r.skip(sps.log2_max_poc_lsb as usize);
            let rps = if r.bit() == 0 {
                let n = sps.st_rps.len();
                parse_st_rps(&mut r, n, n, &sps.st_rps)?
            } else {
                let n = sps.st_rps.len();
                let idx = if n > 1 { r.u(ceil_log2(n as u32)) as usize } else { 0 };
                sps.st_rps.get(idx).cloned().ok_or("bad short_term_ref_pic_set_idx")?
            };
            num_pic_total_curr += rps.num_used() as u32;
            if sps.long_term_ref_pics_present {
                let num_lt_sps = sps.lt_ref_pics.len() as u32;
                let n_sps = if num_lt_sps > 0 { r.ue() } else { 0 };
                let n_pics = r.ue();
                for i in 0..n_sps + n_pics {
                    if i < n_sps {
                        let idx = if num_lt_sps > 1 { r.u(ceil_log2(num_lt_sps)) as usize } else { 0 };
                        if sps.lt_ref_pics.get(idx).ok_or("bad lt_idx_sps")?.1 {
                            num_pic_total_curr += 1;
                        }
                    } else {
                        r.skip(sps.log2_max_poc_lsb as usize);
                        if r.bit() == 1 {
                            num_pic_total_curr += 1;
                        }
                    }
                    if r.bit() == 1 {
                        r.ue(); // delta_poc_msb_cycle_lt
                    }
                }
            }
            if sps.temporal_mvp {
                temporal_mvp = r.bit() == 1;
            }
        }
        if sps.sao {
            sh.sao_luma = r.bit() == 1;
            if sps.chroma_array_type() != 0 {
                sh.sao_chroma = r.bit() == 1;
            }
        }
        sh.num_ref_idx_active = [0, 0];
        if sh.slice_type != 2 {
            sh.num_ref_idx_active = [pps.num_ref_idx_default[0], if sh.slice_type == 0 { pps.num_ref_idx_default[1] } else { 0 }];
            if r.bit() == 1 {
                sh.num_ref_idx_active[0] = r.ue() + 1;
                if sh.slice_type == 0 {
                    sh.num_ref_idx_active[1] = r.ue() + 1;
                }
            }
            if pps.lists_modification_present && num_pic_total_curr > 1 {
                let bits = ceil_log2(num_pic_total_curr);
                for list in 0..(if sh.slice_type == 0 { 2 } else { 1 }) {
                    if r.bit() == 1 {
                        for _ in 0..sh.num_ref_idx_active[list] {
                            r.skip(bits as usize);
                        }
                    }
                }
            }
            if sh.slice_type == 0 {
                sh.mvd_l1_zero = r.bit() == 1;
            }
            if pps.cabac_init_present {
                sh.cabac_init_flag = r.bit() == 1;
            }
            if temporal_mvp {
                let mut from_l0 = true;
                if sh.slice_type == 0 {
                    from_l0 = r.bit() == 1;
                }
                let list = if from_l0 { 0 } else { 1 };
                if sh.num_ref_idx_active[list] > 1 {
                    r.ue(); // collocated_ref_idx
                }
            }
            if (pps.weighted_pred && sh.slice_type == 1) || (pps.weighted_bipred && sh.slice_type == 0) {
                skip_pred_weight_table(&mut r, sps, &sh);
            }
            sh.max_num_merge_cand = 5u32.saturating_sub(r.ue());
        }
        let qp_delta = r.se();
        sh.slice_qp = 26 + pps.init_qp_minus26 + qp_delta;
        if pps.slice_chroma_qp_offsets_present {
            r.se();
            r.se();
        }
        if pps.chroma_qp_offset_list_enabled {
            sh.cu_chroma_qp_offset_enabled = r.bit() == 1;
        }
        let mut override_flag = false;
        if pps.deblocking_override_enabled {
            override_flag = r.bit() == 1;
        }
        let mut deblocking_disabled = pps.deblocking_disabled;
        if override_flag {
            deblocking_disabled = r.bit() == 1;
            if !deblocking_disabled {
                r.se();
                r.se();
            }
        }
        if pps.loop_filter_across_slices && (sh.sao_luma || sh.sao_chroma || !deblocking_disabled) {
            r.skip(1);
        }
    }

    sh.num_entry_points = 0;
    if pps.tiles_enabled || pps.entropy_coding_sync {
        sh.num_entry_points = r.ue();
        if sh.num_entry_points > 0 {
            let len = r.ue() + 1;
            if len > 32 {
                return Err("bad offset_len_minus1".into());
            }
            for _ in 0..sh.num_entry_points {
                r.skip(len as usize);
            }
        }
    }
    if pps.slice_header_extension_present {
        let n = r.ue();
        r.skip(8 * n as usize);
    }
    // byte_alignment(): a one bit then zeros up to the byte boundary.
    if r.bit() != 1 {
        return Err("slice header does not end in an alignment bit".into());
    }
    let bits = r.bit_pos();
    sh.data_offset = bits.div_ceil(8);
    if r.exhausted() {
        return Err("slice header ran past the end of the NAL".into());
    }
    Ok(sh)
}

fn skip_pred_weight_table(r: &mut Bits, sps: &Sps, sh: &SliceHeader) {
    r.ue(); // luma_log2_weight_denom
    let chroma = sps.chroma_array_type() != 0;
    if chroma {
        r.se();
    }
    for list in 0..(if sh.slice_type == 0 { 2 } else { 1 }) {
        let n = sh.num_ref_idx_active[list] as usize;
        let luma: Vec<bool> = (0..n).map(|_| r.bit() == 1).collect();
        let chroma_flags: Vec<bool> = if chroma { (0..n).map(|_| r.bit() == 1).collect() } else { vec![false; n] };
        for i in 0..n {
            if luma[i] {
                r.se();
                r.se();
            }
            if chroma_flags[i] {
                for _ in 0..2 {
                    r.se();
                    r.se();
                }
            }
        }
    }
}
