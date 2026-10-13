//! The H.264 parameter sets the VA-API driver leaves to the application.
//!
//! libva only writes sequence and picture parameter sets when the pipeline is
//! configured for packed headers, and even then it expects the application to
//! supply the bits, so a stream from `vaEndPicture` carries slices alone. The
//! values below mirror what [`super::Encoder`] hands the driver: any mismatch
//! decodes to a different picture than the one that was encoded.
//!
//! Only the constrained baseline shape is emitted (CAVLC, no B frames, no
//! scaling matrices), which is what the encoder is configured for.
use super::{FRAME_NUM_MODULO, MB, POC_MODULO};

/// `log2_max_frame_num_minus4` the driver is told about, in the minus-4 form.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = FRAME_NUM_MODULO.trailing_zeros() - 4;
/// `log2_max_pic_order_cnt_lsb_minus4`, likewise.
const LOG2_MAX_POC_LSB_MINUS4: u32 = POC_MODULO.trailing_zeros() - 4;
/// One reference frame, matching the reference list the encoder keeps.
const MAX_NUM_REF_FRAMES: u32 = 1;
const PROFILE_IDC_CONSTRAINED_BASELINE: u32 = 66;

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits already occupied in the last byte of `bytes`.
    used: u8,
}

impl BitWriter {
    fn bit(&mut self, value: u32) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if value & 1 != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    fn bits(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            self.bit((value >> shift) & 1);
        }
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self, value: u32) {
        let leading = 31 - value.saturating_add(1).leading_zeros();
        self.bits(0, leading);
        self.bits(value + 1, leading + 1);
    }

    /// Signed Exp-Golomb.
    fn se(&mut self, value: i32) {
        self.ue(if value <= 0 {
            (-value * 2) as u32
        } else {
            (value * 2 - 1) as u32
        });
    }

    /// Closes the RBSP and turns it into an EBSP: a 0x03 is inserted after any
    /// two zero bytes that a following byte would otherwise turn into a start
    /// code, which is what a decoder strips back out.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        while self.used != 0 {
            self.bit(0);
        }
        let mut escaped = Vec::with_capacity(self.bytes.len() + 8);
        let mut zeros = 0u8;
        for byte in self.bytes {
            if zeros >= 2 && byte <= 3 {
                escaped.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            escaped.push(byte);
        }
        escaped
    }
}

fn start_code() -> [u8; 4] {
    [0, 0, 0, 1]
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
pub(super) fn parameter_sets(width: u32, height: u32, level_idc: u8) -> Vec<u8> {
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
    writer.ue(LOG2_MAX_FRAME_NUM_MINUS4);
    writer.ue(0);
    writer.ue(LOG2_MAX_POC_LSB_MINUS4);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::VideoCodec;

    #[test]
    fn parameter_sets_parse_back_as_the_requested_format() {
        for (width, height) in [(1280, 720), (1920, 1080), (1600, 2560), (1282, 722)] {
            let stream = parameter_sets(width, height, 31);
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
        // Level 5.1 keeps the payload opaque here; whatever comes out must
        // stay exactly two units.
        let stream = parameter_sets(1920, 1080, 51);
        let units = crate::media::video_format::annex_b_units(&stream);
        assert_eq!(units.len(), 2, "应恰好是 SPS 与 PPS 两个单元");
        assert_eq!(units[0][0] & 0x1f, 7);
        assert_eq!(units[1][0] & 0x1f, 8);
    }
}
