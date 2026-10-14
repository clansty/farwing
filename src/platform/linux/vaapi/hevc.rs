//! The H.265 parameter sets the VA-API driver leaves to the application, and
//! the frame parameters that go with them.
//!
//! Same division of labour as the H.264 side: libva only writes parameter sets
//! when the pipeline is configured for packed headers and even then expects the
//! application to supply the bits, so a stream from `vaEndPicture` carries
//! slices alone. Everything the driver is configured with here is mirrored in
//! the parameter sets below; any mismatch decodes to a different picture than
//! the one that was encoded.
//!
//! Only the Main profile shape is emitted: 4:2:0 8-bit, one slice per picture,
//! no tiles, no wavefront, no SAO, no scaling lists, no PCM.
use super::{BitWriter, start_code, start_code_len};
use crate::media::encoding::{Codec, Format, Rate};
use anyhow::{Result, ensure};
use cros_libva::{
    Buffer, BufferType, Context, EncPictureParameter, EncPictureParameterBufferHEVC,
    EncSequenceParameter, EncSequenceParameterBufferHEVC, EncSliceParameter,
    EncSliceParameterBufferHEVC, HEVCEncPicFields, HEVCEncSeqFields, HevcEncPicSccFields,
    HevcEncSeqSccFields, HevcEncSliceFields, PictureHEVC, VA_INVALID_SURFACE,
};
use std::rc::Rc;

/// The coding tree block HEVC codes in, and the sequence header's
/// `log2_diff_max_min_luma_coding_block_size` for it: 8 * 2^3.
const CTU: u32 = 64;
const LOG2_DIFF_MAX_MIN_CB: u8 = 3;
/// `log2_diff_max_min_luma_transform_block_size`, i.e. 32-sample transforms.
const LOG2_DIFF_MAX_MIN_TB: u8 = 3;
const POC_MODULO: u16 = 1 << 8;
const MAIN_PROFILE: u8 = 1;
/// HEVC signals a level as thirty times its number.
const fn coded_level(level: u8) -> u8 {
    level * 30
}
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
/// `IDR_W_RADL`, and the trailing picture type every other frame uses.
const NAL_IDR: u8 = 19;
const NAL_TRAIL_R: u8 = 1;

pub(super) fn accepts(format: Format) -> bool {
    format.codec == Codec::H265 && format.chroma == 1 && format.depth == 8
}

pub(super) fn profile() -> i32 {
    cros_libva::VAProfile::VAProfileHEVCMain
}

/// The level whose limits cover this picture size.
pub(super) fn level(size: (u32, u32)) -> u8 {
    match size.0.saturating_mul(size.1) {
        0..=2_073_600 => 4,
        2_073_601..=3_686_400 => 5,
        _ => 5,
    }
}

/// A two-byte NAL unit header: no layer, temporal id 1.
fn nal(type_: u8) -> [u8; 2] {
    [type_ << 1, 1]
}

/// `profile_tier_level` for a single sub-layer: Main profile, no tier, no
/// sub-layer flags, and the given level.
fn profile_tier_level(writer: &mut BitWriter, level_idc: u8) {
    writer.bits(0, 2);
    writer.bit(0);
    writer.bits(u32::from(MAIN_PROFILE), 5);
    // general_profile_compatibility_flag, with the bit for this profile set.
    writer.bits(1 << (31 - u32::from(MAIN_PROFILE)), 32);
    // general_progressive_source_flag, general_interlaced_source_flag,
    // general_non_packed_constraint_flag, general_frame_only_constraint_flag.
    writer.bits(0b1001, 4);
    // The 44 constraint flags, in two calls because a u32 cannot be shifted
    // by more than 31.
    writer.bits(0, 32);
    writer.bits(0, 12);
    writer.bits(u32::from(coded_level(level_idc)), 8);
}

/// One entry of the sub-layer ordering, for the single sub-layer there is.
fn sub_layer_ordering(writer: &mut BitWriter) {
    writer.ue(1);
    writer.ue(0);
    writer.ue(0);
}

/// Video, sequence and picture parameter sets, in Annex B order. Emitted ahead
/// of every IDR.
///
/// `init_qp_minus26` is fixed at zero rather than carrying the frame QP: the
/// driver puts the whole QP into the slice header's `slice_qp_delta`.
pub(super) fn parameter_sets(size: (u32, u32), level_idc: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);

    out.extend_from_slice(&start_code());
    out.extend_from_slice(&nal(NAL_VPS));
    let mut writer = BitWriter::default();
    writer.bits(0, 4);
    writer.bit(1);
    writer.bit(1);
    writer.bits(0, 6);
    writer.bits(0, 3);
    writer.bit(1);
    writer.bits(0xffff, 16);
    profile_tier_level(&mut writer, level_idc);
    writer.bit(0);
    sub_layer_ordering(&mut writer);
    writer.bits(0, 6);
    writer.ue(0);
    writer.bit(0);
    writer.bit(0);
    out.extend_from_slice(&writer.finish());

    out.extend_from_slice(&start_code());
    out.extend_from_slice(&nal(NAL_SPS));
    let mut writer = BitWriter::default();
    writer.bits(0, 4);
    writer.bits(0, 3);
    writer.bit(1);
    profile_tier_level(&mut writer, level_idc);
    writer.ue(0);
    writer.ue(1);
    writer.ue(size.0);
    writer.ue(size.1);
    writer.bit(0);
    writer.ue(0);
    writer.ue(0);
    writer.ue(POC_MODULO.trailing_zeros() - 4);
    writer.bit(0);
    sub_layer_ordering(&mut writer);
    writer.ue(0);
    writer.ue(u32::from(LOG2_DIFF_MAX_MIN_CB));
    writer.ue(0);
    writer.ue(u32::from(LOG2_DIFF_MAX_MIN_TB));
    writer.ue(0);
    writer.ue(0);
    writer.bit(0);
    writer.bit(1);
    writer.bit(0);
    writer.bit(0);
    writer.ue(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(1);
    writer.bit(0);
    writer.bit(0);
    out.extend_from_slice(&writer.finish());

    out.extend_from_slice(&start_code());
    out.extend_from_slice(&nal(NAL_PPS));
    let mut writer = BitWriter::default();
    writer.ue(0);
    writer.ue(0);
    writer.bit(0);
    writer.bit(0);
    writer.bits(0, 3);
    writer.bit(0);
    writer.bit(0);
    writer.ue(0);
    writer.ue(0);
    writer.se(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.se(0);
    writer.se(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(1);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    writer.ue(0);
    writer.bit(0);
    writer.bit(0);
    out.extend_from_slice(&writer.finish());

    out
}

/// One reconstructed picture, kept so the next P frame can point at it.
#[derive(Clone, Copy)]
struct Reference {
    poc: i32,
}

fn picture(surface: u32, poc: i32) -> PictureHEVC {
    PictureHEVC::new(surface, poc, 0)
}

fn invalid_picture() -> PictureHEVC {
    PictureHEVC::new(VA_INVALID_SURFACE, 0, cros_libva::VA_PICTURE_HEVC_INVALID)
}

/// Per-stream counters. HEVC carries no frame number in the slice header, so
/// the picture order count is the whole of the state.
pub(super) struct State {
    poc_lsb: u16,
    reference: Option<Reference>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            poc_lsb: 0,
            reference: None,
        }
    }

    /// An IDR resets the counter cycle it opens, so the reset happens before
    /// the headers are built rather than after.
    pub(super) fn reset(&mut self, keyframe: bool) {
        if keyframe {
            self.poc_lsb = 0;
            self.reference = None;
        }
    }

    pub(super) fn advance(&mut self) {
        self.reference = Some(Reference {
            poc: i32::from(self.poc_lsb),
        });
        self.poc_lsb = (self.poc_lsb + 1) % POC_MODULO;
    }

    /// Whether the next frame would wrap the picture order count back to zero.
    /// See `h264::State::wrapping` for why that must not happen.
    pub(super) fn wrapping(&self) -> bool {
        self.poc_lsb + 1 >= POC_MODULO
    }

    /// The three parameter buffers one picture needs.
    pub(super) fn buffers(
        &self,
        context: &Rc<Context>,
        coded: u32,
        reconstructed: u32,
        size: (u32, u32),
        rate: Rate,
        qp: u8,
        keyframe: bool,
    ) -> Result<Vec<Buffer>> {
        let poc = i32::from(self.poc_lsb);
        let reference = self.reference.filter(|_| !keyframe);
        let mut references: [PictureHEVC; 15] = std::array::from_fn(|_| invalid_picture());
        if let Some(reference) = reference {
            references[0] = picture(reconstructed, reference.poc);
        }

        let sequence = BufferType::EncSequenceParameter(EncSequenceParameter::HEVC(
            EncSequenceParameterBufferHEVC::new(
                MAIN_PROFILE,
                coded_level(level(size)),
                0,
                rate.fps.max(1),
                rate.fps.max(1),
                1,
                rate.target,
                size.0 as u16,
                size.1 as u16,
                &HEVCEncSeqFields::new(1, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 1, 0),
                0,
                LOG2_DIFF_MAX_MIN_CB,
                0,
                LOG2_DIFF_MAX_MIN_TB,
                0,
                0,
                0,
                0,
                0,
                0,
                None,
                255,
                1,
                1,
                1,
                rate.fps.max(1) * 2,
                0,
                0,
                0,
                &HevcEncSeqSccFields::new(0),
            ),
        ));

        let picture_parameter = BufferType::EncPictureParameter(EncPictureParameter::HEVC(
            EncPictureParameterBufferHEVC::new(
                picture(reconstructed, poc),
                references,
                coded,
                0,
                0,
                qp,
                0,
                0,
                0,
                0,
                0,
                [0; 19],
                [0; 21],
                0,
                0,
                0,
                0,
                0,
                if keyframe { NAL_IDR } else { NAL_TRAIL_R },
                &HEVCEncPicFields::new(
                    u32::from(keyframe),
                    if keyframe { 1 } else { 2 },
                    1,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                ),
                0,
                0,
                &HevcEncPicSccFields::new(0),
            ),
        ));

        let mut slice_refs: [PictureHEVC; 15] = std::array::from_fn(|_| invalid_picture());
        if let Some(reference) = reference {
            slice_refs[0] = picture(reconstructed, reference.poc);
        }
        let slice = BufferType::EncSliceParameter(EncSliceParameter::HEVC(
            EncSliceParameterBufferHEVC::new(
                0,
                size.0.div_ceil(CTU) * size.1.div_ceil(CTU),
                if keyframe { 2 } else { 1 },
                0,
                0,
                0,
                slice_refs,
                std::array::from_fn(|_| invalid_picture()),
                0,
                0,
                [0; 15],
                [0; 15],
                [[0; 2]; 15],
                [[0; 2]; 15],
                [0; 15],
                [0; 15],
                [[0; 2]; 15],
                [[0; 2]; 15],
                5,
                0,
                0,
                0,
                0,
                0,
                &HevcEncSliceFields::new(1, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1, 1),
                0,
                0,
            ),
        ));

        let mut buffers = Vec::with_capacity(3);
        buffers.push(context.create_buffer(sequence)?);
        buffers.push(context.create_buffer(picture_parameter)?);
        buffers.push(context.create_buffer(slice)?);
        Ok(buffers)
    }

    /// The driver emits a start code and then leaves the slice's two-byte NAL
    /// header to the application.
    pub(super) fn patch_nal(payload: &mut [u8], keyframe: bool) -> Result<()> {
        let prefix = start_code_len(payload)?;
        ensure!(payload.len() >= prefix + 2, "编码器输出不足以容纳 NAL 头");
        payload[prefix..prefix + 2].copy_from_slice(&nal(if keyframe {
            NAL_IDR
        } else {
            NAL_TRAIL_R
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_sets_parse_back_as_the_requested_format() {
        for size in [(1280, 720), (1920, 1080), (1600, 2560), (1282, 722)] {
            let stream = parameter_sets(size, level(size));
            let parsed = crate::media::video_format::parse_stream_format(
                crate::media::VideoCodec::H265,
                &stream,
            )
            .expect("SPS 解析失败");
            assert_eq!(parsed.profile_idc, MAIN_PROFILE);
            assert_eq!(parsed.chroma_format_idc, 1);
            assert_eq!(parsed.bit_depth_luma, 8);
            assert_eq!(
                (parsed.visible_width, parsed.visible_height),
                size,
                "SPS 里的画面尺寸与请求不一致"
            );
        }
    }

    #[test]
    fn parameter_sets_are_three_start_code_units() {
        let stream = parameter_sets((1920, 1080), 5);
        let units = crate::media::video_format::annex_b_units(&stream);
        let types = units
            .iter()
            .map(|unit| (unit[0] >> 1) & 0x3f)
            .collect::<Vec<_>>();
        assert_eq!(types, vec![NAL_VPS, NAL_SPS, NAL_PPS]);
    }
}
