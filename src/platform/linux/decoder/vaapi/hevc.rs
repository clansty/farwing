//! Progressive H.265 VA-API picture preparation.
//!
//! 移植自 dytan/farwing 的 linux 分支（commit 757172c），参数缓冲的写法与其
//! Windows DXVA 对应实现（`rust_dxva/hevc.rs`）同源，只是 VA-API 要完整的
//! picture 参数缓冲，外加每个 slice 一份长格式 slice 参数（含 RefPicList 在
//! ReferenceFrames 数组里的下标）。
//!
//! 位流部分（NAL、POC、RPS）用共用的 `openuuyc_h265_syntax`；§8.3.4 的列表装配
//! 在 [`super::hevc_ref_list`]。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use cros_libva::{
    BufferType, HevcLongSliceFlags, HevcPicFields, HevcSliceParsingFields, IQMatrix,
    IQMatrixBufferHEVC, Picture, PictureHEVC, PictureParameter, PictureParameterBufferHEVC,
    SliceParameter, SliceParameterBufferHEVC, VAProfile, VASurfaceID,
};
use openuuyc_codec::syntax::hevc::{
    bitreader::BitReader,
    dpb::{LongTermEntry, build_rps_poc_lists},
    nal::{NalHeader, strip_emulation_prevention},
    poc::{NalKind, PocState},
    pps::PicParameterSet,
    scaling_list::ScalingListData,
    slice::{SliceLongTermRefPicSource, SliceSegmentHeader, SliceType},
    sps::{MaterializedShortTermRefPicSet, SeqParameterSet, ShortTermRefPicSet},
};

use super::avc::Frame;
use super::display::{self, Pool};
use super::hevc_ref_list::{RefPicListParams, ResolvedRps, build_ref_pic_lists};
use super::output;
use crate::media::decode_api::DecodeError;

const PICTURE_INVALID: u32 = cros_libva::VA_PICTURE_HEVC_INVALID;
const PICTURE_LONG_TERM: u32 = cros_libva::VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
const PICTURE_ST_BEFORE: u32 = cros_libva::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
const PICTURE_ST_AFTER: u32 = cros_libva::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
const PICTURE_LT_CURR: u32 = cros_libva::VA_PICTURE_HEVC_RPS_LT_CURR;
const SLICE_DATA_FLAG_ALL: u32 = cros_libva::VA_SLICE_DATA_FLAG_ALL;

struct Reference {
    poc: i32,
    long: bool,
    /// Pool surface index.
    index: usize,
}

struct Slice<'a> {
    nal: &'a [u8],
    header: SliceSegmentHeader,
    /// Independent-slice fields used when this segment is dependent.
    slice_type: SliceType,
}

pub(super) struct Hevc {
    pool: Option<Pool>,
    profile: VAProfile::Type,
    sps: HashMap<u8, SeqParameterSet>,
    pps: HashMap<u8, PicParameterSet>,
    refs: Vec<Reference>,
    poc: PocState,
    fresh: bool,
    no_rasl_output: bool,
    timestamp: u64,
}

impl Hevc {
    pub(super) fn new() -> Self {
        Self {
            pool: None,
            profile: VAProfile::VAProfileHEVCMain,
            sps: HashMap::new(),
            pps: HashMap::new(),
            refs: Vec::new(),
            poc: PocState::new(),
            fresh: true,
            no_rasl_output: true,
            timestamp: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.refs.clear();
        self.poc = PocState::new();
        self.fresh = true;
        self.no_rasl_output = true;
        if let Some(pool) = self.pool.as_mut() {
            pool.release_all();
        }
    }

    pub(super) fn decode(
        &mut self,
        data: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Frame, DecodeError> {
        let result = self.decode_inner(data, cancel);
        if result.is_err() {
            self.reset();
        }
        result
    }

    #[allow(clippy::too_many_lines, reason = "One access unit, start to finish.")]
    fn decode_inner(&mut self, data: &[u8], cancel: &AtomicBool) -> Result<Frame, DecodeError> {
        if cancel.load(Ordering::Acquire) {
            return Err(DecodeError::Closed);
        }
        let mut first: Option<(
            NalHeader,
            SliceSegmentHeader,
            SeqParameterSet,
            PicParameterSet,
            u16,
            u8,
        )> = None;
        let mut slices: Vec<Slice<'_>> = Vec::new();
        let mut active_type = SliceType::I;
        for nal in openuuyc_codec::syntax::h264::nal::AnnexBSplitter::new(data) {
            let header = NalHeader::parse(nal).map_err(|_| DecodeError::InvalidInput)?;
            if header.nuh_layer_id != 0 {
                return Err(DecodeError::Unsupported);
            }
            let rbsp = strip_emulation_prevention(&nal[2..]);
            match header.nal_unit_type {
                32 => {}
                33 => {
                    let s = SeqParameterSet::parse(&rbsp).map_err(|_| DecodeError::InvalidInput)?;
                    self.sps.insert(s.sps_id, s);
                }
                34 => {
                    let p = PicParameterSet::parse(&rbsp).map_err(|_| DecodeError::InvalidInput)?;
                    self.pps.insert(p.pps_id, p);
                }
                0..=31 => {
                    let mut bits = BitReader::new(&rbsp);
                    let is_first = bits.u1().map_err(|_| DecodeError::InvalidInput)?;
                    if (16..=23).contains(&header.nal_unit_type) {
                        let _ = bits.u1().map_err(|_| DecodeError::InvalidInput)?;
                    }
                    let id = bits.ue().map_err(|_| DecodeError::InvalidInput)?;
                    if id > 63 {
                        return Err(DecodeError::InvalidInput);
                    }
                    let p = self
                        .pps
                        .get(&(id as u8))
                        .ok_or(DecodeError::NeedKeyframe)?
                        .clone();
                    let s = self
                        .sps
                        .get(&p.sps_id)
                        .ok_or(DecodeError::NeedKeyframe)?
                        .clone();
                    let sh = SliceSegmentHeader::parse(&rbsp, header.nal_unit_type, &s, &p)
                        .map_err(|_| DecodeError::InvalidInput)?;
                    // Weighted-pred / inter-RPS deferrals leave an opaque tail;
                    // VA long-form slice params need a fully parsed header.
                    if sh.opaque_tail.is_some() {
                        return Err(DecodeError::Unsupported);
                    }
                    if let Some((old_nal, old, old_sps, old_pps, _, _)) = &first {
                        let same = is_first == 0
                            && old_sps == &s
                            && old_pps == &p
                            && old_nal.nal_unit_type == header.nal_unit_type
                            && old_nal.temporal_id == header.temporal_id
                            && (sh.dependent_slice_segment_flag
                                || old.slice_pic_order_cnt_lsb == sh.slice_pic_order_cnt_lsb);
                        if !same {
                            return Err(DecodeError::InvalidInput);
                        }
                    }
                    if first.is_none() {
                        if is_first == 0 {
                            return Err(DecodeError::InvalidInput);
                        }
                        let rps_bits = inline_rps_bits(&rbsp, header.nal_unit_type, &s, &p)?;
                        let delta = 0u8;
                        first = Some((header, sh.clone(), s, p, rps_bits, delta));
                    }
                    if !sh.dependent_slice_segment_flag {
                        active_type = sh.slice_type.ok_or(DecodeError::InvalidInput)?;
                    }
                    slices.push(Slice {
                        nal,
                        header: sh,
                        slice_type: active_type,
                    });
                }
                _ => {}
            }
        }
        let (nal, sh, s, p, rps_bits, mut delta_count) = first.ok_or(DecodeError::InvalidInput)?;
        let kind = NalKind::new(nal.nal_unit_type);
        if kind.is_rasl() && self.no_rasl_output {
            return Err(DecodeError::NeedKeyframe);
        }
        if self.fresh && !(kind.is_idr() || kind.is_bla() || kind.is_cra()) {
            return Err(DecodeError::NeedKeyframe);
        }
        let no_rasl = kind.is_idr() || kind.is_bla() || (kind.is_cra() && self.fresh);
        if kind.is_idr() || kind.is_bla() || kind.is_cra() {
            self.no_rasl_output = no_rasl;
        }
        if kind.is_idr() || kind.is_bla() {
            self.reset();
        }
        // NV12 readback today: Main 8-bit 4:2:0 only.
        if s.chroma_format_idc != 1
            || s.separate_colour_plane_flag
            || s.bit_depth_luma_minus8 != 0
            || s.bit_depth_chroma_minus8 != 0
        {
            return Err(DecodeError::Unsupported);
        }
        let width = s.pic_width_in_luma_samples;
        let height = s.pic_height_in_luma_samples;
        let crop = &s.conformance_window;
        let crop_unit = 2u32;
        if crop
            .left_offset
            .checked_add(crop.right_offset)
            .is_none_or(|v| v >= width / crop_unit)
            || crop
                .top_offset
                .checked_add(crop.bottom_offset)
                .is_none_or(|v| v >= height / crop_unit)
        {
            return Err(DecodeError::InvalidInput);
        }
        // 与 avc 一致：4:2:0 的 CropUnit 两个方向都是 2，换算成像素后再减；
        // 并且保留左边/上边的原点，只减宽高不还原点的话，画面会从 coded 原点
        // 而不是可见原点起读。
        let visible = output::VisibleRect {
            x: crop.left_offset * crop_unit,
            y: crop.top_offset * crop_unit,
            width: width - crop_unit * (crop.left_offset + crop.right_offset),
            height: height - crop_unit * (crop.top_offset + crop.bottom_offset),
        };
        if visible.width == 0 || visible.height == 0 {
            return Err(DecodeError::InvalidInput);
        }
        let capacity = s.sub_layer_ordering_info[s.max_sub_layers_minus1 as usize]
            .max_dec_pic_buffering_minus1
            + 1;
        if !(1..=16).contains(&capacity) {
            return Err(DecodeError::InvalidInput);
        }
        self.ensure_pool(&s, width, height, capacity as usize)?;

        let max_lsb = 1u32 << (s.log2_max_pic_order_cnt_lsb_minus4 + 4);
        let poc = self.poc.derive(
            kind,
            no_rasl,
            sh.slice_pic_order_cnt_lsb.unwrap_or(0),
            max_lsb,
        );
        let sets = s
            .materialize_short_term_ref_pic_sets()
            .map_err(|_| DecodeError::InvalidInput)?;
        let rps = if kind.is_idr() {
            MaterializedShortTermRefPicSet::default()
        } else if let Some(inline) = &sh.inline_short_term_ref_pic_set {
            let source = if inline.inter_ref_pic_set_prediction_flag {
                let index = sets
                    .len()
                    .checked_sub(inline.delta_idx_minus1 as usize + 1)
                    .ok_or(DecodeError::InvalidInput)?;
                delta_count = sets[index].num_delta_pocs() as u8;
                Some(&sets[index])
            } else {
                None
            };
            inline
                .materialize(source)
                .map_err(|_| DecodeError::InvalidInput)?
        } else {
            sets.get(sh.short_term_ref_pic_set_idx.unwrap_or(0) as usize)
                .ok_or(DecodeError::InvalidInput)?
                .clone()
        };
        let _ = delta_count;
        let mut cycle = 0u32;
        let mut long = Vec::new();
        for (i, entry) in sh.long_term_ref_pics.iter().enumerate() {
            cycle = if i == 0 || i == sh.num_long_term_sps.unwrap_or(0) as usize {
                entry.delta_poc_msb_cycle_lt
            } else {
                cycle + entry.delta_poc_msb_cycle_lt
            };
            let (lsb, used) = match entry.source {
                SliceLongTermRefPicSource::Sps { lt_idx_sps } => {
                    let e = s
                        .long_term_ref_pics
                        .get(lt_idx_sps as usize)
                        .ok_or(DecodeError::InvalidInput)?;
                    (e.poc_lsb, e.used_by_curr_pic)
                }
                SliceLongTermRefPicSource::InSlice {
                    poc_lsb_lt,
                    used_by_curr_pic_lt_flag,
                } => (poc_lsb_lt, used_by_curr_pic_lt_flag),
            };
            long.push(LongTermEntry {
                poc_lsb_lt: lsb,
                used_by_curr_pic_lt: used,
                delta_poc_msb_present: entry.delta_poc_msb_present_flag,
                delta_poc_msb_cycle_lt: cycle,
            });
        }
        let lists = build_rps_poc_lists(kind.is_idr(), poc.val, max_lsb, &rps, &long);
        let mut keep = Vec::new();
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut lt = Vec::new();
        for (pocs, target, is_long, flags) in [
            (&lists.st_curr_before, &mut before, false, None),
            (&lists.st_curr_after, &mut after, false, None),
            (
                &lists.lt_curr,
                &mut lt,
                true,
                Some(&lists.curr_delta_poc_msb_present),
            ),
        ] {
            for (i, value) in pocs.iter().enumerate() {
                let found = self.refs.iter().find(|r| {
                    if is_long && flags.is_some_and(|f| !f[i]) {
                        (r.poc as u32 & (max_lsb - 1)) == (*value as u32 & (max_lsb - 1))
                    } else {
                        r.poc == *value
                    }
                });
                let Some(r) = found else {
                    return Err(DecodeError::NeedKeyframe);
                };
                target.push(r.poc);
                keep.push((r.poc, is_long));
            }
        }
        for (value, is_long, flag) in lists.st_foll.iter().map(|p| (*p, false, true)).chain(
            lists
                .lt_foll
                .iter()
                .zip(&lists.foll_delta_poc_msb_present)
                .map(|(p, f)| (*p, true, *f)),
        ) {
            if let Some(r) = self.refs.iter().find(|r| {
                if is_long && !flag {
                    (r.poc as u32 & (max_lsb - 1)) == (value as u32 & (max_lsb - 1))
                } else {
                    r.poc == value
                }
            }) {
                keep.push((r.poc, is_long));
            }
        }
        self.refs.retain(|r| keep.iter().any(|p| p.0 == r.poc));
        for r in &mut self.refs {
            r.long = keep.iter().any(|p| p.0 == r.poc && p.1);
        }

        let pool = self.pool.as_mut().ok_or(DecodeError::Backend)?;
        let (index, surface) = pool.take()?;
        let surface_id = pool.id(index);

        let reference_frames = reference_frames(&self.refs, pool, &before, &after, &lt);
        let picture_parameter = picture_parameter(
            &s,
            &p,
            nal.nal_unit_type,
            poc.val,
            surface_id,
            reference_frames,
            rps_bits,
        )?;

        let resolved = ResolvedRps {
            st_curr_before: map_pocs_to_ref_indices(&before, &self.refs)?,
            st_curr_after: map_pocs_to_ref_indices(&after, &self.refs)?,
            st_foll: Vec::new(),
            lt_curr: map_pocs_to_ref_indices(&lt, &self.refs)?,
            lt_foll: Vec::new(),
        };

        let mut buffers = vec![BufferType::PictureParameter(PictureParameter::HEVC(
            picture_parameter,
        ))];
        if s.scaling_list_enabled_flag {
            buffers.push(BufferType::IQMatrix(IQMatrix::HEVC(iq_matrix(&s, &p))));
        }

        let mut slice_data_chunks: Vec<Vec<u8>> = Vec::new();
        let slice_count = slices.len();
        for (slice_index, slice) in slices.iter().enumerate() {
            let last = slice_index + 1 == slice_count;
            let (param, data) = slice_parameter(
                slice,
                &s,
                &p,
                &resolved,
                &self.refs,
                &reference_frames,
                last,
            )?;
            buffers.push(BufferType::SliceParameter(SliceParameter::HEVC(param)));
            slice_data_chunks.push(data);
        }
        for data in slice_data_chunks {
            buffers.push(BufferType::SliceData(data));
        }

        self.timestamp = self.timestamp.wrapping_add(1);
        let context = pool.context.clone();
        let decoded = submit_hevc(
            &context,
            surface,
            buffers,
            self.timestamp,
            pool.nv12,
            (pool.width, pool.height),
            visible,
        );
        let (frame, surface) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                self.pool = None;
                return Err(error);
            }
        };
        let pool = self.pool.as_mut().ok_or(DecodeError::Backend)?;
        pool.put(index, surface);

        if nal.temporal_id == 0 && !(kind.is_rasl() || kind.is_radl() || kind.is_slnr()) {
            self.poc.update_prev_tid0(poc);
        }
        self.fresh = false;
        self.refs.push(Reference {
            poc: poc.val,
            long: false,
            index,
        });
        let reserved: Vec<usize> = self.refs.iter().map(|r| r.index).collect();
        self.pool
            .as_mut()
            .ok_or(DecodeError::Backend)?
            .reserve_only(&reserved);
        Ok(frame)
    }

    fn ensure_pool(
        &mut self,
        sps: &SeqParameterSet,
        width: u32,
        height: u32,
        dpb: usize,
    ) -> Result<(), DecodeError> {
        let profile = profile_for(sps)?;
        let matches = self.pool.as_ref().is_some_and(|pool| {
            pool.width == width && pool.height == height && self.profile == profile
        });
        if matches {
            return Ok(());
        }
        if !self.refs.is_empty() {
            return Err(DecodeError::NeedKeyframe);
        }
        if !display::supports(profile, width, height) {
            return Err(DecodeError::Unsupported);
        }
        self.pool = Some(Pool::new(profile, width, height, dpb + 1)?);
        self.profile = profile;
        tracing::info!(profile, width, height, dpb, "VA-API HEVC session created");
        Ok(())
    }
}

fn map_pocs_to_ref_indices(
    pocs: &[i32],
    refs: &[Reference],
) -> Result<Vec<Option<usize>>, DecodeError> {
    pocs.iter()
        .map(|poc| {
            refs.iter()
                .position(|r| r.poc == *poc)
                .map(Some)
                .ok_or(DecodeError::NeedKeyframe)
        })
        .collect()
}

fn profile_for(sps: &SeqParameterSet) -> Result<VAProfile::Type, DecodeError> {
    match (sps.bit_depth_luma_minus8, sps.chroma_format_idc) {
        (0, 1) => Ok(VAProfile::VAProfileHEVCMain),
        (2, 1) => Ok(VAProfile::VAProfileHEVCMain10),
        _ => Err(DecodeError::Unsupported),
    }
}

fn invalid_picture() -> PictureHEVC {
    PictureHEVC::new(cros_libva::VA_INVALID_ID, 0, PICTURE_INVALID)
}

fn reference_frames(
    refs: &[Reference],
    pool: &Pool,
    before: &[i32],
    after: &[i32],
    lt: &[i32],
) -> [PictureHEVC; 15] {
    let mut frames = std::array::from_fn(|_| invalid_picture());
    for (slot, entry) in frames.iter_mut().zip(refs.iter()) {
        let mut flags = 0u32;
        if entry.long {
            flags |= PICTURE_LONG_TERM;
        }
        if before.contains(&entry.poc) {
            flags |= PICTURE_ST_BEFORE;
        } else if after.contains(&entry.poc) {
            flags |= PICTURE_ST_AFTER;
        } else if lt.contains(&entry.poc) {
            flags |= PICTURE_LT_CURR;
        }
        *slot = PictureHEVC::new(pool.id(entry.index), entry.poc, flags);
    }
    frames
}

fn picture_parameter(
    s: &SeqParameterSet,
    p: &PicParameterSet,
    nal_type: u8,
    poc: i32,
    surface: VASurfaceID,
    reference_frames: [PictureHEVC; 15],
    rps_bits: u16,
) -> Result<PictureParameterBufferHEVC, DecodeError> {
    let curr = PictureHEVC::new(surface, poc, 0);
    let pcm = s.pcm.as_ref();
    let pic_fields = HevcPicFields::new(
        u32::from(s.chroma_format_idc),
        u32::from(s.separate_colour_plane_flag),
        u32::from(s.pcm_enabled_flag),
        u32::from(s.scaling_list_enabled_flag),
        u32::from(p.transform_skip_enabled_flag),
        u32::from(s.amp_enabled_flag),
        u32::from(s.strong_intra_smoothing_enabled_flag),
        u32::from(p.sign_data_hiding_enabled_flag),
        u32::from(p.constrained_intra_pred_flag),
        u32::from(p.cu_qp_delta_enabled_flag),
        u32::from(p.weighted_pred_flag),
        u32::from(p.weighted_bipred_flag),
        u32::from(p.transquant_bypass_enabled_flag),
        u32::from(p.tiles_enabled_flag),
        u32::from(p.entropy_coding_sync_enabled_flag),
        u32::from(p.pps_loop_filter_across_slices_enabled_flag),
        u32::from(p.tiles_enabled_flag && p.loop_filter_across_tiles_enabled_flag),
        u32::from(pcm.is_some_and(|pcm| pcm.loop_filter_disabled_flag)),
        0,
        0,
    );
    let rap = (16..=23).contains(&nal_type);
    let idr = nal_type == 19 || nal_type == 20;
    let intra = (16..=23).contains(&nal_type);
    let slice_fields = HevcSliceParsingFields::new(
        u32::from(p.lists_modification_present_flag),
        u32::from(s.long_term_ref_pics_present_flag),
        u32::from(s.sps_temporal_mvp_enabled_flag),
        u32::from(p.cabac_init_present_flag),
        u32::from(p.output_flag_present_flag),
        u32::from(p.dependent_slice_segments_enabled_flag),
        u32::from(p.pps_slice_chroma_qp_offsets_present_flag),
        u32::from(s.sample_adaptive_offset_enabled_flag),
        u32::from(p.deblocking.override_enabled_flag),
        u32::from(p.deblocking.disabled_flag),
        u32::from(p.slice_segment_header_extension_present_flag),
        u32::from(rap),
        u32::from(idr),
        u32::from(intra),
    );
    let mut column_width = [0u16; 19];
    let mut row_height = [0u16; 21];
    if p.tiles_enabled_flag {
        if p.tiles.num_tile_columns_minus1 > 19 || p.tiles.num_tile_rows_minus1 > 21 {
            return Err(DecodeError::Unsupported);
        }
        for (i, x) in p.tiles.column_width_minus1.iter().enumerate() {
            column_width[i] = u16::try_from(*x).map_err(|_| DecodeError::InvalidInput)?;
        }
        for (i, x) in p.tiles.row_height_minus1.iter().enumerate() {
            row_height[i] = u16::try_from(*x).map_err(|_| DecodeError::InvalidInput)?;
        }
    }
    Ok(PictureParameterBufferHEVC::new(
        curr,
        reference_frames,
        s.pic_width_in_luma_samples as u16,
        s.pic_height_in_luma_samples as u16,
        &pic_fields,
        s.sub_layer_ordering_info[s.max_sub_layers_minus1 as usize].max_dec_pic_buffering_minus1
            as u8,
        s.bit_depth_luma_minus8,
        s.bit_depth_chroma_minus8,
        pcm.map_or(0, |pcm| pcm.bit_depth_luma_minus1),
        pcm.map_or(0, |pcm| pcm.bit_depth_chroma_minus1),
        s.log2_min_luma_coding_block_size_minus3,
        s.log2_diff_max_min_luma_coding_block_size,
        s.log2_min_luma_transform_block_size_minus2,
        s.log2_diff_max_min_luma_transform_block_size,
        pcm.map_or(0, |pcm| pcm.log2_min_pcm_luma_coding_block_size_minus3),
        pcm.map_or(0, |pcm| pcm.log2_diff_max_min_pcm_luma_coding_block_size),
        s.max_transform_hierarchy_depth_intra,
        s.max_transform_hierarchy_depth_inter,
        p.init_qp_minus26 as i8,
        p.diff_cu_qp_delta_depth as u8,
        p.pps_cb_qp_offset,
        p.pps_cr_qp_offset,
        p.log2_parallel_merge_level_minus2 as u8,
        if p.tiles_enabled_flag {
            p.tiles.num_tile_columns_minus1 as u8
        } else {
            0
        },
        if p.tiles_enabled_flag {
            p.tiles.num_tile_rows_minus1 as u8
        } else {
            0
        },
        column_width,
        row_height,
        &slice_fields,
        s.log2_max_pic_order_cnt_lsb_minus4,
        s.num_short_term_ref_pic_sets as u8,
        s.num_long_term_ref_pics_sps as u8,
        p.num_ref_idx_l0_default_active_minus1,
        p.num_ref_idx_l1_default_active_minus1,
        p.deblocking.beta_offset_div2,
        p.deblocking.tc_offset_div2,
        p.num_extra_slice_header_bits,
        u32::from(rps_bits),
    ))
}

fn iq_matrix(s: &SeqParameterSet, p: &PicParameterSet) -> IQMatrixBufferHEVC {
    let fallback = ScalingListData::all_default();
    let m = p
        .scaling_list_data
        .as_ref()
        .or(s.scaling_list_data.as_ref())
        .unwrap_or(&fallback);
    let mut list4 = [[0u8; 16]; 6];
    let mut list8 = [[0u8; 64]; 6];
    let mut list16 = [[0u8; 64]; 6];
    let mut list32 = [[0u8; 64]; 2];
    let mut dc16 = [0u8; 6];
    let mut dc32 = [0u8; 2];
    for matrix in 0..6 {
        diagonal_to_raster_4(&m.lists[0][matrix].coef, &mut list4[matrix]);
        diagonal_to_raster_8(&m.lists[1][matrix].coef, &mut list8[matrix]);
        diagonal_to_raster_8(&m.lists[2][matrix].coef, &mut list16[matrix]);
        dc16[matrix] = m.lists[2][matrix].dc_coef as u8;
    }
    for (out, matrix) in [(0usize, 0usize), (1usize, 3usize)] {
        diagonal_to_raster_8(&m.lists[3][matrix].coef, &mut list32[out]);
        dc32[out] = m.lists[3][matrix].dc_coef as u8;
    }
    IQMatrixBufferHEVC::new(list4, list8, list16, list32, dc16, dc32)
}

fn diagonal_to_raster_4(src: &[u16], dst: &mut [u8; 16]) {
    const SCAN: [usize; 16] = [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15];
    // coef[i] is the i-th up-right-diagonal sample; place it at raster SCAN[i].
    for (i, value) in src.iter().take(16).enumerate() {
        dst[SCAN[i]] = *value as u8;
    }
}

fn diagonal_to_raster_8(src: &[u16], dst: &mut [u8; 64]) {
    // Build §6.5.3 up-right diagonal scan order for 8x8, then inverse-map.
    let mut scan = [0usize; 64];
    let mut idx = 0usize;
    for sum in 0usize..15 {
        let mut diagonal: Vec<usize> = (0usize..8)
            .filter_map(|x| sum.checked_sub(x).filter(|y| *y < 8).map(|y| y * 8 + x))
            .collect();
        if sum % 2 == 1 {
            diagonal.reverse();
        }
        for pos in diagonal {
            scan[idx] = pos;
            idx += 1;
        }
    }
    for (i, value) in src.iter().take(64).enumerate() {
        dst[scan[i]] = *value as u8;
    }
}

fn slice_parameter(
    slice: &Slice<'_>,
    s: &SeqParameterSet,
    p: &PicParameterSet,
    resolved: &ResolvedRps,
    refs: &[Reference],
    va_refs: &[PictureHEVC; 15],
    last: bool,
) -> Result<(SliceParameterBufferHEVC, Vec<u8>), DecodeError> {
    let sh = &slice.header;
    let slice_type = if sh.dependent_slice_segment_flag {
        slice.slice_type
    } else {
        sh.slice_type.unwrap_or(slice.slice_type)
    };
    let active_l0 = sh
        .num_ref_idx_l0_active_minus1
        .unwrap_or(p.num_ref_idx_l0_default_active_minus1);
    let active_l1 = sh
        .num_ref_idx_l1_active_minus1
        .unwrap_or(p.num_ref_idx_l1_default_active_minus1);
    let num_pic_total_curr = (resolved.st_curr_before.len()
        + resolved.st_curr_after.len()
        + resolved.lt_curr.len()) as u32;
    let list_entry_l0 = sh
        .ref_pic_lists_modification
        .as_ref()
        .filter(|m| m.ref_pic_list_modification_flag_l0)
        .map(|m| m.list_entry_l0.as_slice());
    let list_entry_l1 = sh.ref_pic_lists_modification.as_ref().and_then(|m| {
        if m.ref_pic_list_modification_flag_l1.unwrap_or(false) {
            Some(m.list_entry_l1.as_slice())
        } else {
            None
        }
    });
    let params = RefPicListParams {
        num_ref_idx_l0_active_minus1: u32::from(active_l0),
        num_ref_idx_l1_active_minus1: u32::from(active_l1),
        num_pic_total_curr,
        is_b: slice_type == SliceType::B,
        list_entry_l0,
        list_entry_l1,
    };
    let lists = build_ref_pic_lists(&resolved, &params);
    let to_va = |list: &[Option<usize>]| -> [u8; 15] {
        let mut out = [0xffu8; 15];
        for (i, entry) in list.iter().take(15).enumerate() {
            let Some(ref_index) = entry else {
                continue;
            };
            let surface = refs.get(*ref_index).map(|r| r.index);
            let Some(surface) = surface else {
                continue;
            };
            // Index into the VA ReferenceFrames array (same order as refs).
            if let Some(va_index) = refs.iter().position(|r| r.index == surface) {
                // Prefer matching by POC against va_refs for safety.
                let poc = refs[va_index].poc;
                if let Some(pos) = va_refs.iter().position(|pic| {
                    pic.picture_id() != cros_libva::VA_INVALID_ID && pic.pic_order_cnt() == poc
                }) {
                    out[i] = pos as u8;
                }
            }
        }
        out
    };
    let ref_pic_list0 = to_va(&lists.list0);
    let ref_pic_list1 = lists
        .list1
        .as_ref()
        .map(|list| to_va(list))
        .unwrap_or([0xff; 15]);

    let deblocking = sh.deblocking.as_ref();
    let long_flags = HevcLongSliceFlags::new(
        u32::from(last),
        u32::from(sh.dependent_slice_segment_flag),
        match slice_type {
            SliceType::B => 0,
            SliceType::P => 1,
            SliceType::I => 2,
        },
        u32::from(sh.colour_plane_id.unwrap_or(0)),
        u32::from(sh.slice_sao_luma_flag),
        u32::from(sh.slice_sao_chroma_flag),
        u32::from(sh.mvd_l1_zero_flag.unwrap_or(false)),
        u32::from(sh.cabac_init_flag.unwrap_or(false)),
        u32::from(sh.slice_temporal_mvp_enabled_flag),
        u32::from(deblocking.is_some_and(|d| d.disabled_flag)),
        u32::from(sh.collocated_from_l0_flag.unwrap_or(true)),
        u32::from(
            sh.slice_loop_filter_across_slices_enabled_flag
                .unwrap_or(p.pps_loop_filter_across_slices_enabled_flag),
        ),
    );
    let collocated = if sh.slice_temporal_mvp_enabled_flag {
        sh.collocated_ref_idx.unwrap_or(0) as u8
    } else {
        0xff
    };
    let weights = Weights::from(sh, s, active_l0 as usize, active_l1 as usize, slice_type);
    let rbsp_offset = sh
        .byte_offset_to_slice_data
        .ok_or(DecodeError::Unsupported)?;
    let (byte_offset, emu) = slice_data_offsets(slice.nal, rbsp_offset)?;
    let size = u32::try_from(slice.nal.len()).map_err(|_| DecodeError::InvalidInput)?;
    let five_minus = sh.five_minus_max_num_merge_cand.unwrap_or(0).min(4) as u8;
    let num_entry = sh
        .entry_point_offsets
        .as_ref()
        .map(|e| e.num_entry_point_offsets)
        .unwrap_or(0)
        .min(u32::from(u16::MAX)) as u16;
    let mut param = SliceParameterBufferHEVC::new(
        size,
        0,
        SLICE_DATA_FLAG_ALL,
        byte_offset,
        sh.slice_segment_address,
        [ref_pic_list0, ref_pic_list1],
        &long_flags,
        collocated,
        active_l0,
        active_l1,
        sh.slice_qp_delta.unwrap_or(0) as i8,
        sh.slice_cb_qp_offset,
        sh.slice_cr_qp_offset,
        deblocking.map_or(p.deblocking.beta_offset_div2, |d| d.beta_offset_div2),
        deblocking.map_or(p.deblocking.tc_offset_div2, |d| d.tc_offset_div2),
        weights.luma_log2_denom,
        weights.delta_chroma_log2_denom,
        weights.delta_luma_weight_l0,
        weights.luma_offset_l0,
        weights.delta_chroma_weight_l0,
        weights.chroma_offset_l0,
        weights.delta_luma_weight_l1,
        weights.luma_offset_l1,
        weights.delta_chroma_weight_l1,
        weights.chroma_offset_l1,
        five_minus,
        num_entry,
        0,
        emu,
    );
    if last {
        param.set_as_last();
    }
    Ok((param, slice.nal.to_vec()))
}

struct Weights {
    luma_log2_denom: u8,
    delta_chroma_log2_denom: i8,
    delta_luma_weight_l0: [i8; 15],
    luma_offset_l0: [i8; 15],
    delta_chroma_weight_l0: [[i8; 2]; 15],
    chroma_offset_l0: [[i8; 2]; 15],
    delta_luma_weight_l1: [i8; 15],
    luma_offset_l1: [i8; 15],
    delta_chroma_weight_l1: [[i8; 2]; 15],
    chroma_offset_l1: [[i8; 2]; 15],
}

impl Weights {
    fn from(
        sh: &SliceSegmentHeader,
        s: &SeqParameterSet,
        active_l0: usize,
        active_l1: usize,
        slice_type: SliceType,
    ) -> Self {
        let mut w = Self {
            luma_log2_denom: 0,
            delta_chroma_log2_denom: 0,
            delta_luma_weight_l0: [0; 15],
            luma_offset_l0: [0; 15],
            delta_chroma_weight_l0: [[0; 2]; 15],
            chroma_offset_l0: [[0; 2]; 15],
            delta_luma_weight_l1: [0; 15],
            luma_offset_l1: [0; 15],
            delta_chroma_weight_l1: [[0; 2]; 15],
            chroma_offset_l1: [[0; 2]; 15],
        };
        let Some(table) = sh.pred_weight_table.as_ref() else {
            return w;
        };
        w.luma_log2_denom = table.luma_log2_weight_denom;
        w.delta_chroma_log2_denom = table.delta_chroma_log2_weight_denom as i8;
        let half_c = 128i32; // high_precision_offsets off (Main)
        let chroma_denom = (i32::from(table.luma_log2_weight_denom)
            + table.delta_chroma_log2_weight_denom)
            .clamp(0, 7) as u32;
        let fill = |entries: &[openuuyc_codec::syntax::hevc::slice::PredWeightEntry],
                    active: usize,
                    delta_luma: &mut [i8; 15],
                    luma_off: &mut [i8; 15],
                    delta_chroma: &mut [[i8; 2]; 15],
                    chroma_off: &mut [[i8; 2]; 15]| {
            for i in 0..active.min(15) {
                let Some(entry) = entries.get(i) else {
                    break;
                };
                delta_luma[i] = entry.delta_luma_weight as i8;
                luma_off[i] = entry.luma_offset as i8;
                for j in 0..2 {
                    delta_chroma[i][j] = entry.delta_chroma_weight[j] as i8;
                    let chroma_weight = (1i32 << chroma_denom) + entry.delta_chroma_weight[j];
                    let offset = half_c + entry.delta_chroma_offset[j]
                        - ((half_c * chroma_weight) >> chroma_denom);
                    chroma_off[i][j] = offset.clamp(-half_c, half_c - 1) as i8;
                }
            }
        };
        fill(
            &table.entries_l0,
            active_l0,
            &mut w.delta_luma_weight_l0,
            &mut w.luma_offset_l0,
            &mut w.delta_chroma_weight_l0,
            &mut w.chroma_offset_l0,
        );
        if slice_type == SliceType::B {
            fill(
                &table.entries_l1,
                active_l1,
                &mut w.delta_luma_weight_l1,
                &mut w.luma_offset_l1,
                &mut w.delta_chroma_weight_l1,
                &mut w.chroma_offset_l1,
            );
        }
        let _ = s;
        w
    }
}

/// Translate an RBSP byte offset into a raw-NAL byte offset and count the
/// emulation-prevention bytes the driver needs to skip in the header.
fn slice_data_offsets(nal: &[u8], rbsp_byte_offset: usize) -> Result<(u32, u16), DecodeError> {
    if nal.len() < 2 {
        return Err(DecodeError::InvalidInput);
    }
    let mut rbsp = 0usize;
    let mut emu = 0u16;
    let mut index = 2usize;
    let mut zeroes = 0u32;
    while rbsp < rbsp_byte_offset {
        if index >= nal.len() {
            return Err(DecodeError::InvalidInput);
        }
        let value = nal[index];
        if zeroes >= 2 && value == 3 {
            emu = emu.saturating_add(1);
            zeroes = 0;
            index += 1;
            continue;
        }
        zeroes = if value == 0 { zeroes + 1 } else { 0 };
        rbsp += 1;
        index += 1;
    }
    Ok((
        u32::try_from(index).map_err(|_| DecodeError::InvalidInput)?,
        emu,
    ))
}

fn inline_rps_bits(
    rbsp: &[u8],
    kind: u8,
    s: &SeqParameterSet,
    p: &PicParameterSet,
) -> Result<u16, DecodeError> {
    if kind == 19 || kind == 20 {
        return Ok(0);
    }
    let mut b = BitReader::new(rbsp);
    if b.u1().map_err(|_| DecodeError::InvalidInput)? == 0 {
        return Err(DecodeError::InvalidInput);
    }
    if (16..=23).contains(&kind) {
        let _ = b.u1().map_err(|_| DecodeError::InvalidInput)?;
    }
    let _ = b.ue().map_err(|_| DecodeError::InvalidInput)?;
    b.skip(p.num_extra_slice_header_bits as usize)
        .map_err(|_| DecodeError::InvalidInput)?;
    let _ = b.ue().map_err(|_| DecodeError::InvalidInput)?;
    if p.output_flag_present_flag {
        let _ = b.u1().map_err(|_| DecodeError::InvalidInput)?;
    }
    if s.separate_colour_plane_flag {
        let _ = b.u(2).map_err(|_| DecodeError::InvalidInput)?;
    }
    let _ = b
        .u(s.log2_max_pic_order_cnt_lsb_minus4 + 4)
        .map_err(|_| DecodeError::InvalidInput)?;
    if b.u1().map_err(|_| DecodeError::InvalidInput)? != 0 {
        return Ok(0);
    }
    let start = b.bit_pos();
    ShortTermRefPicSet::parse_slice_inline(&mut b, s).map_err(|_| DecodeError::InvalidInput)?;
    u16::try_from(b.bit_pos() - start).map_err(|_| DecodeError::InvalidInput)
}

type Surface = cros_libva::Surface<()>;

fn submit_hevc(
    context: &std::rc::Rc<cros_libva::Context>,
    surface: Surface,
    buffers: Vec<BufferType>,
    timestamp: u64,
    format: cros_libva::VAImageFormat,
    coded: (u32, u32),
    visible: output::VisibleRect,
) -> Result<(Frame, Surface), DecodeError> {
    let backend = |stage: &'static str| {
        move |error: cros_libva::VaError| {
            tracing::debug!(%error, stage, "VA-API HEVC decode failed");
            DecodeError::HardwareFailure
        }
    };
    let mut picture = Picture::new(timestamp, std::rc::Rc::clone(context), surface);
    for buffer in buffers {
        let buffer = context
            .create_buffer(buffer)
            .map_err(backend("create_buffer"))?;
        picture.add_buffer(buffer);
    }
    let picture = picture
        .begin()
        .map_err(backend("begin"))?
        .render()
        .map_err(backend("render"))?
        .end()
        .map_err(backend("end"))?;
    let picture = picture
        .sync()
        .map_err(|(error, _)| backend("sync")(error))?;
    // 读回整幅 coded surface，裁剪交给 pack_nv12 按原点取，这样左边/上边的
    // 裁剪也能还原；vaCreateImage 表达不了原点。
    let output::VisibleRect { width, height, .. } = visible;
    let frame = picture
        .create_image(format, coded, coded)
        .map_err(backend("create_image"))
        .and_then(|image| output::pack_nv12(&image, visible))?;
    let surface = picture
        .take_surface()
        .unwrap_or_else(|_| unreachable!("the picture owns its surface alone"));
    Ok((
        Frame {
            width,
            height,
            data: frame,
        },
        surface,
    ))
}
