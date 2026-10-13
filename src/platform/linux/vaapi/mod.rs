//! VA-API encoding on the GPU that drives the display: the counterpart of
//! NVENC on NVIDIA, and the only hardware encoder an AMD or Intel host has.
//!
//! SPS/PPS come from the driver. Packed headers are deliberately not requested
//! through the config, so the driver writes the parameter sets into the coded
//! buffer itself, which is also what the probe in `encoder.rs` parses back.
//!
//! Capture hands over tightly packed BGRA while every encode entrypoint takes
//! NV12, so each frame converts on the CPU into the mapped NV12 image of the
//! input surface. `yuv` does the colour work with the same BT.601 limited range
//! the software encoder declares.
mod h264;

use super::capture::Image as Captured;
use crate::media::encoding::rate::Controller;
use crate::media::encoding::{Backend, Codec, Color, Encoded, Format, Rate};
use anyhow::{Context as _, Result, ensure};
use cros_libva::{
    BufferType, Context, Display, EncPictureParameter, EncPictureParameterBufferH264,
    EncSequenceParameter, EncSequenceParameterBufferH264, EncSliceParameter,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, Image as VaImage,
    MappedCodedBuffer, Picture, PictureH264, Surface, UsageHint, VA_INVALID_ID, VA_INVALID_SURFACE,
    VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RT_FORMAT_YUV420, VAConfigAttrib, VAConfigAttribType,
    VAEntrypoint, VAProfile,
};
use std::cell::RefCell;
use std::rc::Rc;

/// H.264 codes whole macroblocks.
const MB: u32 = 16;
/// The sequence header below uses `log2_max_frame_num_minus4 = 1` and
/// `log2_max_pic_order_cnt_lsb_minus4 = 2`, so these are the wrap points of the
/// two counters that go into the picture and slice headers.
const FRAME_NUM_MODULO: u16 = 1 << 5;
const POC_MODULO: u16 = 1 << 6;
const SLICE_REFS: usize = 32;

/// The PCI vendors whose VA-API encoder is what the protocol already calls
/// `Amd` and `Intel`. Any other vendor stays unclaimed rather than mislabelled.
const PCI_VENDOR_AMD: u16 = 0x1002;
const PCI_VENDOR_INTEL: u16 = 0x8086;

thread_local! {
    /// `vaInitialize` is expensive and a display is bound to the thread that
    /// opened it, so every thread that encodes keeps its own. The error case is
    /// cached too: a machine without VA-API must not retry per candidate.
    static DEVICE: RefCell<Option<Result<(Rc<Display>, u16), String>>> = const { RefCell::new(None) };
}

/// The PCI slots of the DRM cards with a connected output.
fn connected_slots() -> Vec<String> {
    std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let card = name.split('-').next()?.to_owned();
            let uevent =
                std::fs::read_to_string(format!("/sys/class/drm/{card}/device/uevent")).ok()?;
            uevent
                .lines()
                .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                .map(|slot| slot.to_ascii_lowercase())
        })
        .collect()
}

/// The render node of the GPU that drives the display, with its PCI vendor.
/// Matched through the PCI address of the card whose output is connected, so a
/// machine with both an integrated and a discrete GPU encodes on the one whose
/// picture is being captured. The first node that answers is used when sysfs
/// cannot tell.
fn display_node() -> Option<(String, u16)> {
    let connected = connected_slots();
    let mut fallback = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("renderD") {
            continue;
        }
        let device = entry.path().join("device");
        let vendor = std::fs::read_to_string(device.join("vendor"))
            .ok()
            .and_then(|value| u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok());
        let Some(vendor) = vendor else {
            continue;
        };
        let candidate = (format!("/dev/dri/{name}"), vendor);
        let slot = std::fs::read_to_string(device.join("uevent"))
            .ok()
            .and_then(|uevent| {
                uevent
                    .lines()
                    .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                    .map(|slot| slot.to_ascii_lowercase())
            });
        if slot.is_some_and(|slot| connected.contains(&slot)) {
            return Some(candidate);
        }
        fallback.get_or_insert(candidate);
    }
    fallback
}

fn open_display() -> Option<Rc<Display>> {
    display_node()
        .and_then(|(path, _)| Display::open_drm_display(path).ok())
        .or_else(Display::open)
}

fn device() -> Result<(Rc<Display>, u16)> {
    DEVICE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let opened = slot.get_or_insert_with(|| {
            let vendor = display_node().map_or(0, |(_, vendor)| vendor);
            match open_display() {
                Some(display) => Ok((display, vendor)),
                None => Err("没有可用的 VA-API 设备".to_owned()),
            }
        });
        match opened {
            Ok((display, vendor)) => Ok((Rc::clone(display), *vendor)),
            Err(message) => anyhow::bail!("{message}"),
        }
    })
}

fn display() -> Result<Rc<Display>> {
    device().map(|(display, _)| display)
}

/// The wire backend for this machine's VA-API encoder, or `None` when the GPU
/// is from a vendor the protocol has no id for.
pub(crate) fn hardware_backend() -> Option<Backend> {
    match device().ok()?.1 {
        PCI_VENDOR_AMD => Some(Backend::Amd),
        PCI_VENDOR_INTEL => Some(Backend::Intel),
        _ => None,
    }
}

/// The profile the emitted parameter sets declare. Constrained baseline is
/// what the stream is: CAVLC, no B frames, no scaling matrices.
fn profile(format: Format) -> Option<i32> {
    match format.codec {
        Codec::H264 => Some(VAProfile::VAProfileH264ConstrainedBaseline),
        _ => None,
    }
}

/// The slice entrypoint for a profile. Both are accepted when the driver
/// offers them; `EncSliceLP` alone is Intel's low-power path and AMD rejects
/// it, so the ordinary entrypoint is preferred.
fn entrypoint(display: &Display, profile: i32) -> Option<u32> {
    let entrypoints = display.query_config_entrypoints(profile).ok()?;
    [
        VAEntrypoint::VAEntrypointEncSlice,
        VAEntrypoint::VAEntrypointEncSliceLP,
    ]
    .into_iter()
    .find(|candidate| entrypoints.contains(candidate))
}

fn profile_entrypoint(format: Format) -> Option<(Rc<Display>, i32, u32)> {
    let display = display().ok()?;
    let profile = profile(format)?;
    let entrypoint = entrypoint(&display, profile)?;
    Some((display, profile, entrypoint))
}

/// Whether this GPU declares support for `format`. Only the profile and
/// entrypoint are checked; whether a usable stream comes out is decided by the
/// probe in `encoder.rs`, which encodes a frame and parses its SPS.
pub(crate) fn accepts(format: Format) -> bool {
    // The capture backends deliver 8-bit BGRA; any other input format would
    // need a different surface type and conversion here.
    format.chroma == 1 && format.depth == 8 && profile_entrypoint(format).is_some()
}

/// The largest picture this profile accepts, used to cap the negotiated size.
pub(crate) fn maximum_size(format: Format) -> (u32, u32) {
    let query = |attribute: u32| -> Option<u32> {
        let (display, profile, entrypoint) = profile_entrypoint(format)?;
        let mut attrs = [VAConfigAttrib {
            type_: attribute,
            value: 0,
        }];
        display
            .get_config_attributes(profile, entrypoint, &mut attrs)
            .ok()?;
        (attrs[0].value > 0).then_some(attrs[0].value)
    };
    (
        query(VAConfigAttribType::VAConfigAttribMaxPictureWidth).unwrap_or(4096),
        query(VAConfigAttribType::VAConfigAttribMaxPictureHeight).unwrap_or(4096),
    )
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

pub(crate) struct Encoder {
    context: Rc<Context>,
    /// The source picture. libva pictures own their render target, so this is
    /// taken for the span of a frame and put back after `sync`.
    input: Option<Surface<()>>,
    /// The scratch surface the driver reconstructs into. `CurrPic` must name a
    /// reconstruction surface, never the source; keeping it alive is what makes
    /// it valid.
    _reconstructed: Surface<()>,
    reconstructed_id: u32,
    coded: cros_libva::EncCodedBuffer,
    image_format: cros_libva::VAImageFormat,
    size: (u32, u32),
    format: Format,
    rate: Rate,
    frame_rate: Controller,
    /// Reused BGRA to planar conversion output: Y, then U, then V.
    planar: Vec<u8>,
    frame_num: u16,
    frame_idx: u32,
    idr_pic_id: u16,
    poc_lsb: u16,
    reference: Option<Reference>,
    force_keyframe: bool,
}

impl Encoder {
    pub(crate) fn new(size: (u32, u32), format: Format, rate: Rate) -> Result<Self> {
        ensure!(size.0 >= 2 * MB && size.1 >= 2 * MB, "编码尺寸过小");
        ensure!(
            format.chroma == 1 && format.depth == 8,
            "VA-API 输入只支持 8 位 4:2:0"
        );
        let (display, profile, entrypoint) =
            profile_entrypoint(format).context("VA-API 不支持该编码格式")?;

        let mut attrs = vec![VAConfigAttrib {
            type_: VAConfigAttribType::VAConfigAttribRTFormat,
            value: 0,
        }];
        display
            .get_config_attributes(profile, entrypoint, &mut attrs)
            .context("查询编码配置属性失败")?;
        let config = display
            .create_config(attrs, profile, entrypoint)
            .context("创建编码配置失败")?;

        let surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                None,
                size.0,
                size.1,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![(), ()],
            )
            .context("创建编码 surface 失败")?;
        let context = display
            .create_context(&config, size.0, size.1, Some(&surfaces), true)
            .context("创建编码上下文失败")?;

        let image_format = display
            .query_image_formats()
            .context("查询图像格式失败")?
            .into_iter()
            .find(|candidate| candidate.fourcc == cros_libva::VA_FOURCC_NV12)
            .context("该驱动不提供 NV12 图像格式")?;

        let coded = context
            .create_enc_coded(size.0 as usize * size.1 as usize * 3 / 2)
            .context("创建码流缓冲失败")?;

        let mut surfaces = surfaces.into_iter();
        let input = surfaces.next().context("没有编码输入 surface")?;
        let reconstructed = surfaces.next().context("没有编码重建 surface")?;
        let reconstructed_id = reconstructed.id();

        Ok(Self {
            context,
            input: Some(input),
            _reconstructed: reconstructed,
            reconstructed_id,
            coded,
            image_format,
            size,
            format,
            rate,
            frame_rate: Controller::new(rate),
            planar: vec![0; size.0 as usize * size.1 as usize * 3 / 2],
            frame_num: 0,
            frame_idx: 0,
            idr_pic_id: 0,
            poc_lsb: 0,
            reference: None,
            force_keyframe: true,
        })
    }

    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        maximum_size(self.format)
    }

    /// Same contract as the other encoders: `true` means the quality setting
    /// changed and the caller must send a keyframe.
    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let key = self.rate.quality != rate.quality;
        self.rate = rate;
        self.frame_rate.commit(update);
        Ok(key)
    }

    fn convert(&mut self, bgra: &[u8]) -> Result<()> {
        let (width, height) = self.size;
        let luma = width as usize * height as usize;
        let (y, uv) = self.planar.split_at_mut(luma);
        let (u, v) = uv.split_at_mut(luma / 4);
        yuv::bgra_to_yuv420(
            &mut yuv::YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(y),
                y_stride: width,
                u_plane: yuv::BufferStoreMut::Borrowed(u),
                u_stride: width / 2,
                v_plane: yuv::BufferStoreMut::Borrowed(v),
                v_stride: width / 2,
                width,
                height,
            },
            bgra,
            width * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )
        .context("BGRA 转 NV12 失败")
    }

    /// Uploads the converted frame into the picture's source surface. An NV12
    /// image is a copy in and out, but it is the layout every driver
    /// guarantees; deriving the image instead would hand back the driver's own,
    /// possibly tiled, layout.
    fn upload(&mut self, picture: &Picture<cros_libva::PictureNew, Surface<()>>) -> Result<()> {
        let (width, height) = self.size;
        let mut image = VaImage::create_from(
            picture.surface::<()>(),
            self.image_format,
            self.size,
            self.size,
        )
        .context("映射编码输入图像失败")?;
        let layout = *image.image();
        let pitches = [layout.pitches[0] as usize, layout.pitches[1] as usize];
        let offsets = [layout.offsets[0] as usize, layout.offsets[1] as usize];
        let destination = image.as_mut();
        let luma = width as usize * height as usize;
        let (y, uv) = self.planar.split_at(luma);
        let (u, v) = uv.split_at(luma / 4);

        for row in 0..height as usize {
            let start = offsets[0] + row * pitches[0];
            destination[start..start + width as usize]
                .copy_from_slice(&y[row * width as usize..][..width as usize]);
        }
        let chroma_width = (width / 2) as usize;
        for row in 0..(height / 2) as usize {
            let start = offsets[1] + row * pitches[1];
            let line = &mut destination[start..start + chroma_width * 2];
            for column in 0..chroma_width {
                line[column * 2] = u[row * chroma_width + column];
                line[column * 2 + 1] = v[row * chroma_width + column];
            }
        }
        Ok(())
    }

    fn encode_frame(&mut self, timestamp_100ns: i64, keyframe: bool) -> Result<Vec<Encoded>> {
        // An IDR resets the counter cycle it opens, so the reset happens before
        // the headers are built rather than after.
        if keyframe {
            self.frame_num = 0;
            self.frame_idx = 0;
            self.poc_lsb = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1) & 0xffff;
            self.reference = None;
        }
        let (frame_idx, frame_num, poc_lsb) = (self.frame_idx, self.frame_num, self.poc_lsb);
        let poc = i32::from(poc_lsb);
        let reference = self.reference;
        let (width, height) = self.size;
        let level = level_idc(width, height);
        let quality_qp = qp(self.rate.quality);
        let source = self.input.take().context("编码输入 surface 已被取走")?;
        let mut picture = Picture::new(
            timestamp_100ns.max(0) as u64,
            Rc::clone(&self.context),
            source,
        );
        self.upload(&picture)?;

        let mut references: [PictureH264; 16] = std::array::from_fn(|_| invalid_picture());
        if let Some(reference) = reference {
            references[0] =
                reference_picture(self.reconstructed_id, reference.frame_idx, reference.poc);
        }

        let sequence = BufferType::EncSequenceParameter(EncSequenceParameter::H264(
            EncSequenceParameterBufferH264::new(
                0,
                level,
                self.rate.fps.max(1),
                self.rate.fps.max(1),
                1,
                self.rate.target,
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
                self.rate.fps.max(1) * 2,
            ),
        ));

        let picture_parameter = BufferType::EncPictureParameter(EncPictureParameter::H264(
            EncPictureParameterBufferH264::new(
                reference_picture(self.reconstructed_id, frame_idx, poc),
                references,
                self.coded.id(),
                0,
                0,
                0,
                frame_num,
                quality_qp,
                0,
                0,
                0,
                0,
                &H264EncPicFields::new(u32::from(keyframe), 1, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            ),
        ));

        let mut slice_refs: [PictureH264; SLICE_REFS] = std::array::from_fn(|_| invalid_picture());
        if let Some(reference) = reference {
            slice_refs[0] =
                reference_picture(self.reconstructed_id, reference.frame_idx, reference.poc);
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

        picture.add_buffer(self.context.create_buffer(sequence)?);
        picture.add_buffer(self.context.create_buffer(picture_parameter)?);
        picture.add_buffer(self.context.create_buffer(slice)?);

        let picture = picture.begin()?.render()?.end()?;
        let picture = picture.sync().map_err(|(error, _)| error)?;
        self.input = Some(
            picture
                .take_surface()
                .map_err(|_| anyhow::anyhow!("编码输入 surface 仍被占用"))?,
        );

        // The reconstruction of this frame is what the next one references, so
        // what gets recorded is the identity this frame was encoded with.
        self.reference = Some(Reference { frame_idx, poc });
        self.frame_idx = frame_idx + 1;
        self.frame_num = (frame_num + 1) % FRAME_NUM_MODULO;
        self.poc_lsb = (poc_lsb + 2) % POC_MODULO;

        let mut payload = Vec::new();
        let mut segments = 0;
        {
            let mapped = MappedCodedBuffer::new(&self.coded)?;
            for segment in mapped.iter() {
                segments += 1;
                payload.extend_from_slice(segment.buf);
            }
        }
        // The driver emits a start code and then leaves the slice's NAL header
        // to the application: a zero placeholder byte sits where the header
        // belongs, and the slice header follows it. Every picture here is a
        // reference for the next one, so the importance is always 2.
        ensure!(segments == 1, "编码器返回了 {segments} 段码流");
        let prefix = if payload.starts_with(&[0, 0, 0, 1]) {
            4
        } else if payload.starts_with(&[0, 0, 1]) {
            3
        } else {
            anyhow::bail!("编码器输出没有起始码");
        };
        let header = if keyframe { 0x65 } else { 0x41 };
        payload[prefix] = header;

        let mut data = if keyframe {
            h264::parameter_sets(width, height, level)
        } else {
            Vec::new()
        };
        data.extend_from_slice(&payload);
        Ok(vec![Encoded {
            data,
            keyframe,
            timestamp_100ns,
            is_new: true,
            timing: None,
            format: self.format,
            color: Color::Sdr.space(None),
        }])
    }

    pub(crate) fn encode(
        &mut self,
        image: &Captured,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        ensure!(image.size() == self.size, "采集画面尺寸与编码器不一致");
        let pixels = image.pixels()?;
        self.convert(&pixels)?;
        let keyframe = keyframe || self.force_keyframe;
        self.force_keyframe = false;
        self.encode_frame(timestamp_100ns, keyframe)
    }
}

/// The H.264 level whose limits cover this picture size.
fn level_idc(width: u32, height: u32) -> u8 {
    match width.div_ceil(MB) * height.div_ceil(MB) {
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

/// The frame QP for a quality setting. Quality rises with the value, so the QP
/// falls.
fn qp(quality: i32) -> u8 {
    (51 - 4 * quality.clamp(0, 5)).clamp(1, 51) as u8
}

/// Encodes a synthetic frame and parses the SPS back, which is the same check
/// `encoder::probe` applies. Needs a GPU with a VA-API encoder:
/// `cargo test -- --ignored encodes_h264_with_a_parseable_sps`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encoding::QualityTarget;

    fn rate() -> Rate {
        Rate {
            target: 8_000_000,
            peak: 8_000_000,
            fps: 60,
            quality: 3,
            quality_target: QualityTarget {
                bitrate: 8_000_000,
                fps: 60,
            },
        }
    }

    fn frame(width: u32, height: u32) -> Captured {
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        for (index, pixel) in bgra.chunks_exact_mut(4).enumerate() {
            let column = (index as u32 % width) as u8;
            let row = (index as u32 / width) as u8;
            pixel.copy_from_slice(&[row, column, row.wrapping_add(column), 0xff]);
        }
        Captured::Cpu {
            pixels: std::sync::Arc::new(bgra),
            width,
            height,
        }
    }

    #[test]
    #[ignore]
    fn encodes_h264_with_a_parseable_sps() {
        let format = Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        };
        assert!(accepts(format), "VA-API 不接受 H.264 4:2:0 8 位");
        println!("device: {:?}", display_node());
        println!("backend: {:?}", hardware_backend());
        println!("maximum {:?}", maximum_size(format));

        let size = (1280, 720);
        let mut encoder = Encoder::new(size, format, rate()).unwrap();
        let source = frame(size.0, size.1);

        let mut stream = Vec::new();
        for index in 0..4 {
            let encoded = encoder
                .encode(&source, i64::from(index) * 166_666, index == 0)
                .unwrap();
            let parsed = crate::media::video_format::parse_stream_format(
                crate::media::VideoCodec::H264,
                &encoded[0].data,
            );
            stream.extend_from_slice(&encoded[0].data);
            let units = crate::media::video_format::annex_b_units(&encoded[0].data);
            println!(
                "frame {index}: {} bytes, keyframe {}, {} units, NAL types {:?}",
                encoded[0].data.len(),
                encoded[0].keyframe,
                units.len(),
                units
                    .iter()
                    .map(|unit| unit.first().map_or(255, |header| header & 0x1f))
                    .collect::<Vec<_>>()
            );
            if index == 0 {
                println!(
                    "head: {:02x?}",
                    &encoded[0].data[..encoded[0].data.len().min(48)]
                );
                let parsed = parsed.expect("首个 IDR 里没有 SPS");
                assert_eq!(parsed.chroma_format_idc, 1);
                assert_eq!(parsed.bit_depth_luma, 8);
                assert_eq!(
                    (parsed.visible_width, parsed.visible_height),
                    size,
                    "SPS 里的画面尺寸与请求不一致"
                );
            }
        }
        std::fs::write("/tmp/openuuyc-vaapi-test.h264", &stream).unwrap();
        println!(
            "wrote /tmp/openuuyc-vaapi-test.h264 ({} bytes)",
            stream.len()
        );
    }
}
