//! The H.264 parameter sets the VA-API driver leaves to the application, and
//! the frame parameters that go with them.
//!
//! libva only writes sequence and picture parameter sets when the pipeline is
//! configured for packed headers, and even then it expects the application to
//! supply the bits, so a stream from `vaEndPicture` carries slices alone. The
//! values below mirror what the driver is configured with: any mismatch decodes
//! to a different picture than the one that was encoded.
//!
//! Only the constrained baseline shape is emitted (CAVLC, no B frames, no
//! scaling matrices), which is what the encoder is configured for.
use super::{BitWriter, MB, start_code, start_code_len};
use crate::media::encoding::{Codec, Format, Rate};
use anyhow::{Result, ensure};
use cros_libva::{
    Buffer, BufferType, Context, EncPictureParameter, EncPictureParameterBufferH264,
    EncSequenceParameter, EncSequenceParameterBufferH264, EncSliceParameter,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, PictureH264, VA_INVALID_ID,
    VA_INVALID_SURFACE, VA_PICTURE_H264_SHORT_TERM_REFERENCE,
};
use std::rc::Rc;

/// `log2_max_frame_num_minus4` and `log2_max_pic_order_cnt_lsb_minus4` from the
/// sequence header, as the wrap points of the two counters that go into the
/// picture and slice headers.
const FRAME_NUM_MODULO: u16 = 1 << 5;
const POC_MODULO: u16 = 1 << 6;
const SLICE_REFS: usize = 32;
const PROFILE_IDC_CONSTRAINED_BASELINE: u32 = 66;
/// One reference frame, matching the reference list the encoder keeps.
const MAX_NUM_REF_FRAMES: u32 = 1;

pub(super) fn accepts(format: Format) -> bool {
    format.codec == Codec::H264 && format.chroma == 1 && format.depth == 8
}

pub(super) fn profile() -> i32 {
    cros_libva::VAProfile::VAProfileH264ConstrainedBaseline
}

/// The level whose limits cover this picture size.
pub(super) fn level(size: (u32, u32)) -> u8 {
    match size.0.div_ceil(MB) * size.1.div_ceil(MB) {
        0..=99 => 10,
        100..=396 => 20,
        397..=1620 => 30,
        1621..=3600 => 31,
        3601..=8192 => 40,
        8193..=22080 => 41,
        22081..=87040 => 50,
        _ => 51,
    }
}

/// The coded picture size H.264 rounds up to, and the crop that brings it back
/// to the visible size. Both offsets are in the two-sample units that 4:2:0
/// frame coding uses.
fn coded_size(width: u32, height: u32) -> (u32, u32, u32, u32) {
    let coded_width = width.div_ceil(MB) * MB;
    let coded_height = height.div_ceil(MB) * MB;
    (
        coded_width,
        coded_height,
        (coded_width - width) / 2,
        (coded_height - height) / 2,
    )
}

/// A sequence plus picture parameter set, in Annex B order. Emitted ahead of
/// every IDR.
///
/// `pic_init_qp_minus26` is fixed at zero rather than carrying the frame QP:
/// the driver puts the whole QP into the slice header's `slice_qp_delta`, so
/// the decoder reaches the encoder's QP through 0 + 26 + (qp - 26). Declaring
/// the QP here as well would apply it twice.
pub(super) fn parameter_sets(size: (u32, u32), level_idc: u8) -> Vec<u8> {
    let (width, height) = size;
    let (coded_width, coded_height, crop_right, crop_bottom) = coded_size(width, height);
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&start_code());
    out.push(0x67);
    let mut writer = BitWriter::default();
    writer.bits(PROFILE_IDC_CONSTRAINED_BASELINE, 8);
    // Constrained baseline: constraint_set0 and constraint_set1, no others.
    writer.bits(0b1100_0000, 8);
    writer.bits(u32::from(level_idc), 8);
    writer.ue(0);
    writer.ue(FRAME_NUM_MODULO.trailing_zeros() - 4);
    writer.ue(0);
    writer.ue(POC_MODULO.trailing_zeros() - 4);
    writer.ue(MAX_NUM_REF_FRAMES);
    writer.bit(0);
    writer.ue(coded_width / MB - 1);
    writer.ue(coded_height / MB - 1);
    writer.bit(1);
    writer.bit(1);
    writer.bit(u32::from(crop_right > 0 || crop_bottom > 0));
    if crop_right > 0 || crop_bottom > 0 {
        writer.ue(0);
        writer.ue(crop_right);
        writer.ue(0);
        writer.ue(crop_bottom);
    }
    writer.bit(0);
    out.extend_from_slice(&writer.finish());

    out.extend_from_slice(&start_code());
    out.push(0x68);
    let mut writer = BitWriter::default();
    writer.ue(0);
    writer.ue(0);
    writer.bit(0);
    writer.bit(0);
    writer.ue(0);
    writer.ue(0);
    writer.ue(0);
    writer.bit(0);
    writer.bits(0, 2);
    writer.se(0);
    writer.se(0);
    writer.se(0);
    writer.bit(0);
    writer.bit(0);
    writer.bit(0);
    out.extend_from_slice(&writer.finish());
    out
}

fn invalid_picture() -> PictureH264 {
    PictureH264::new(VA_INVALID_ID, 0, VA_INVALID_SURFACE, 0, 0)
}

fn reference_picture(surface: u32, frame_idx: u32, poc: i32) -> PictureH264 {
    PictureH264::new(
        surface,
        frame_idx,
        VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        poc,
        poc,
    )
}

/// One reconstructed picture, kept so the next P frame can point at it.
#[derive(Clone, Copy)]
struct Reference {
    frame_idx: u32,
    poc: i32,
}

/// Per-stream counters.
pub(super) struct State {
    frame_num: u16,
    frame_idx: u32,
    idr_pic_id: u16,
    poc_lsb: u16,
    reference: Option<Reference>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            frame_num: 0,
            frame_idx: 0,
            idr_pic_id: 0,
            poc_lsb: 0,
            reference: None,
        }
    }

    /// An IDR resets the counter cycle it opens, so the reset happens before
    /// the headers are built rather than after.
    pub(super) fn reset(&mut self, keyframe: bool) {
        if keyframe {
            self.frame_num = 0;
            self.frame_idx = 0;
            self.poc_lsb = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1) & 0xffff;
            self.reference = None;
        }
    }

    /// Records this frame's identity for the next one to reference, then moves
    /// the counters on. What gets recorded is the identity the frame was
    /// encoded with, which is why this runs after the headers were built.
    pub(super) fn advance(&mut self) {
        self.reference = Some(Reference {
            frame_idx: self.frame_idx,
            poc: i32::from(self.poc_lsb),
        });
        self.frame_idx += 1;
        self.frame_num = (self.frame_num + 1) % FRAME_NUM_MODULO;
        self.poc_lsb = (self.poc_lsb + 2) % POC_MODULO;
    }

    /// The three parameter buffers one picture needs. They are rebuilt per
    /// frame because a libva picture owns the buffers it renders.
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
        let (width, height) = size;
        let (frame_idx, frame_num, poc_lsb) = (self.frame_idx, self.frame_num, self.poc_lsb);
        let poc = i32::from(poc_lsb);
        let mut references: [PictureH264; 16] = std::array::from_fn(|_| invalid_picture());
        if !keyframe && let Some(reference) = self.reference {
            references[0] = reference_picture(reconstructed, reference.frame_idx, reference.poc);
        }

        let sequence = BufferType::EncSequenceParameter(EncSequenceParameter::H264(
            EncSequenceParameterBufferH264::new(
                0,
                level(size),
                rate.fps.max(1),
                rate.fps.max(1),
                1,
                rate.target,
                1,
                width.div_ceil(MB) as u16,
                height.div_ceil(MB) as u16,
                &H264EncSeqFields::new(1, 1, 0, 0, 1, 1, 0, 2, 0),
                0,
                0,
                0,
                0,
                0,
                [0; 256],
                None,
                None,
                255,
                1,
                1,
                1,
                rate.fps.max(1) * 2,
            ),
        ));

        let picture = BufferType::EncPictureParameter(EncPictureParameter::H264(
            EncPictureParameterBufferH264::new(
                reference_picture(reconstructed, frame_idx, poc),
                references,
                coded,
                0,
                0,
                0,
                frame_num,
                qp,
                0,
                0,
                0,
                0,
                &H264EncPicFields::new(u32::from(keyframe), 1, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            ),
        ));

        let mut slice_refs: [PictureH264; SLICE_REFS] = std::array::from_fn(|_| invalid_picture());
        if !keyframe && let Some(reference) = self.reference {
            slice_refs[0] = reference_picture(reconstructed, reference.frame_idx, reference.poc);
        }
        let slice = BufferType::EncSliceParameter(EncSliceParameter::H264(
            EncSliceParameterBufferH264::new(
                0,
                width.div_ceil(MB) * height.div_ceil(MB),
                VA_INVALID_ID,
                if keyframe { 2 } else { 0 },
                0,
                self.idr_pic_id,
                poc_lsb,
                0,
                [0, 0],
                1,
                0,
                0,
                0,
                slice_refs,
                std::array::from_fn(|_| invalid_picture()),
                0,
                0,
                0,
                [0; 32],
                [0; 32],
                0,
                [[0; 2]; 32],
                [[0; 2]; 32],
                0,
                [0; 32],
                [0; 32],
                0,
                [[0; 2]; 32],
                [[0; 2]; 32],
                0,
                0,
                0,
                2,
                2,
            ),
        ));

        let mut buffers = Vec::with_capacity(3);
        buffers.push(context.create_buffer(sequence)?);
        buffers.push(context.create_buffer(picture)?);
        buffers.push(context.create_buffer(slice)?);
        Ok(buffers)
    }

    /// The driver emits a start code and then leaves the slice's NAL header to
    /// the application: a zero placeholder byte sits where the header belongs,
    /// and the slice header follows it. Every picture here is a reference for
    /// the next one, so the importance is always 2.
    pub(super) fn patch_nal(payload: &mut [u8], keyframe: bool) -> Result<()> {
        let prefix = start_code_len(payload)?;
        ensure!(payload.len() > prefix, "编码器输出不足以容纳 NAL 头");
        payload[prefix] = if keyframe { 0x65 } else { 0x41 };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::VideoCodec;

    #[test]
    fn parameter_sets_parse_back_as_the_requested_format() {
        for (width, height) in [(1280, 720), (1920, 1080), (1600, 2560), (1282, 722)] {
            let stream = parameter_sets((width, height), 31);
            let parsed = crate::media::video_format::parse_stream_format(VideoCodec::H264, &stream)
                .expect("SPS 解析失败");
            assert_eq!(parsed.profile_idc, 66);
            assert_eq!(parsed.chroma_format_idc, 1);
            assert_eq!(parsed.bit_depth_luma, 8);
            assert_eq!(
                (parsed.visible_width, parsed.visible_height),
                (width, height)
            );
        }
    }

    #[test]
    fn emulation_prevention_keeps_start_codes_out_of_the_payload() {
        let stream = parameter_sets((1920, 1080), 51);
        let units = crate::media::video_format::annex_b_units(&stream);
        assert_eq!(units.len(), 2, "应恰好是 SPS 与 PPS 两个单元");
        assert_eq!(units[0][0] & 0x1f, 7);
        assert_eq!(units[1][0] & 0x1f, 8);
    }
}
