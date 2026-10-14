//! VA-API encoding on the GPU that drives the display: the counterpart of
//! NVENC on NVIDIA, and the only hardware encoder an AMD or Intel host has.
//!
//! Parameter sets come from this crate, not the driver: libva writes them only
//! when the pipeline is configured for packed headers, so a stream from
//! `vaEndPicture` carries slices alone and each codec's module supplies the
//! rest. `h264.rs` and `hevc.rs` own everything codec-specific, including the
//! frame parameters, which have to agree with those parameter sets.
//!
//! Capture hands over tightly packed BGRA while every encode entrypoint takes
//! NV12, so each frame converts on the CPU into the mapped NV12 image of the
//! input surface. `yuv` does the colour work with the same BT.601 limited range
//! the software encoder declares.
mod h264;
mod hevc;

use super::capture::Image as Captured;
use crate::media::encoding::rate::Controller;
use crate::media::encoding::{Backend, Codec, Color, Encoded, Format, Rate};
use anyhow::{Context as _, Result, bail, ensure};
use cros_libva::{
    BufferType, Context, Display, EncMiscParameter, EncMiscParameterFrameRate,
    EncMiscParameterRateControl, Image as VaImage, MappedCodedBuffer, Picture, RcFlags, Surface,
    UsageHint, VA_RT_FORMAT_RGB32, VA_RT_FORMAT_YUV420, VA_RC_CBR, VA_RC_VBR, VAConfigAttrib,
    VAConfigAttribType, VAEntrypoint, VAProfile,
};
use std::cell::RefCell;
use std::rc::Rc;

/// H.264 codes whole macroblocks. HEVC's larger coding tree blocks live with it.
const MB: u32 = 16;
/// The PCI vendors whose VA-API encoder is what the protocol already calls
/// `Amd` and `Intel`. Any other vendor stays unclaimed rather than mislabelled.
const PCI_VENDOR_AMD: u16 = 0x1002;
const PCI_VENDOR_INTEL: u16 = 0x8086;

/// Bit-level writer for the RBSP of a parameter set, shared by the codec
/// modules.
#[derive(Default)]
pub(super) struct BitWriter {
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

    /// Writes `count` bits, most significant first. A field wider than the
    /// value it is written from has to be split by the caller.
    pub(super) fn bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 32, "位宽超过 u32 的字段要分段写入");
        for shift in (0..count).rev() {
            self.bit((value >> shift) & 1);
        }
    }

    /// Unsigned Exp-Golomb.
    pub(super) fn ue(&mut self, value: u32) {
        let leading = 31 - value.saturating_add(1).leading_zeros();
        self.bits(0, leading);
        self.bits(value + 1, leading + 1);
    }

    /// Signed Exp-Golomb.
    pub(super) fn se(&mut self, value: i32) {
        self.ue(if value <= 0 {
            (-value * 2) as u32
        } else {
            (value * 2 - 1) as u32
        });
    }

    /// Closes the RBSP and turns it into an EBSP: a 0x03 is inserted after any
    /// two zero bytes that a following byte would otherwise turn into a start
    /// code, which is what a decoder strips back out.
    pub(super) fn finish(mut self) -> Vec<u8> {
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

pub(super) fn start_code() -> [u8; 4] {
    [0, 0, 0, 1]
}

/// The length of the start code a coded buffer's NAL unit begins with.
pub(super) fn start_code_len(payload: &[u8]) -> Result<usize> {
    if payload.starts_with(&[0, 0, 0, 1]) {
        Ok(4)
    } else if payload.starts_with(&[0, 0, 1]) {
        Ok(3)
    } else {
        bail!("编码器输出没有起始码");
    }
}

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

/// The wire backend for this machine's VA-API encoder, or `None` when the GPU
/// is from a vendor the protocol has no id for.
pub(crate) fn hardware_backend() -> Option<Backend> {
    match device().ok()?.1 {
        PCI_VENDOR_AMD => Some(Backend::Amd),
        PCI_VENDOR_INTEL => Some(Backend::Intel),
        _ => None,
    }
}

/// The VA profile a format is encoded with.
fn profile(format: Format) -> Option<i32> {
    match format.codec {
        Codec::H264 => Some(h264::profile()),
        Codec::H265 => Some(hevc::profile()),
        Codec::Av1 => None,
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
    let display = device().map(|(display, _)| display).ok()?;
    let profile = profile(format)?;
    let entrypoint = entrypoint(&display, profile)?;
    Some((display, profile, entrypoint))
}

/// HEVC is written but deliberately not offered yet. The slice headers the
/// driver produces do not line up with the parameter sets in `hevc.rs` — the
/// driver decides for itself which conditional fields to write, and the ones it
/// picks do not match what those sets declare, so a decoder misparses the
/// header (`alignment_bit_equal_to_one=0`). Everything else about the codec is
/// in place; see AGENTS_QA.md for what has been ruled out. Flip this on once
/// the slice header agrees, never before: an advertised codec that produces
/// undecodable streams would be picked by negotiation.
const HEVC_ADVERTISED: bool = false;

/// Whether this GPU declares support for `format`. Only the profile and
/// entrypoint are checked; whether a usable stream comes out is decided by the
/// probe in `encoder.rs`, which encodes a frame and parses its SPS.
pub(crate) fn accepts(format: Format) -> bool {
    // The capture backends deliver 8-bit BGRA; any other input format would
    // need a different surface type and conversion here.
    let codec = h264::accepts(format) || (HEVC_ADVERTISED && hevc::accepts(format));
    codec && profile_entrypoint(format).is_some()
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

/// Which codec's headers and counters a stream uses.
enum CodecState {
    H264(h264::State),
    H265(hevc::State),
}

impl CodecState {
    /// Whether the next picture would wrap a counter the sequence parameter set
    /// declared fixed-width; the caller must send an IDR before that.
    fn wrapping(&self) -> bool {
        match self {
            Self::H264(state) => state.wrapping(),
            Self::H265(state) => state.wrapping(),
        }
    }

    fn new(format: Format) -> Option<Self> {
        match format.codec {
            Codec::H264 => Some(Self::H264(h264::State::new())),
            Codec::H265 => Some(Self::H265(hevc::State::new())),
            Codec::Av1 => None,
        }
    }

    /// The level whose limits cover this picture size.
    fn level(format: Format, size: (u32, u32)) -> u8 {
        match format.codec {
            Codec::H265 => hevc::level(size),
            _ => h264::level(size),
        }
    }

    fn reset(&mut self, keyframe: bool) {
        match self {
            Self::H264(state) => state.reset(keyframe),
            Self::H265(state) => state.reset(keyframe),
        }
    }

    fn advance(&mut self) {
        match self {
            Self::H264(state) => state.advance(),
            Self::H265(state) => state.advance(),
        }
    }

    fn parameter_sets(&self, size: (u32, u32), level: u8) -> Vec<u8> {
        match self {
            Self::H264(_) => h264::parameter_sets(size, level),
            Self::H265(_) => hevc::parameter_sets(size, level),
        }
    }

    fn patch_nal(&self, payload: &mut [u8], keyframe: bool) -> Result<()> {
        match self {
            Self::H264(_) => h264::State::patch_nal(payload, keyframe),
            Self::H265(_) => hevc::State::patch_nal(payload, keyframe),
        }
    }

    fn buffers(
        &self,
        context: &Rc<Context>,
        coded: u32,
        reconstructed: u32,
        size: (u32, u32),
        rate: Rate,
        qp: u8,
        keyframe: bool,
    ) -> Result<Vec<cros_libva::Buffer>> {
        match self {
            Self::H264(state) => {
                state.buffers(context, coded, reconstructed, size, rate, qp, keyframe)
            }
            Self::H265(state) => {
                state.buffers(context, coded, reconstructed, size, rate, qp, keyframe)
            }
        }
    }
}

/// The frame QP for a quality setting. Quality rises with the value, so the QP
/// falls. It is the starting point and the fallback: with a rate control below
/// the announced bit rate decides where a frame lands.
fn qp(quality: i32) -> u8 {
    (51 - 4 * quality.clamp(0, 5)).clamp(1, 51) as u8
}

/// What the driver is asked to do with the bandwidth the transport has already
/// reserved for this stream. Without one of these the stream is coded at the
/// quality preset's QP alone: the announced bit rate never reaches the driver,
/// so a picture that keeps moving is coded as coarsely as that one QP happens
/// to be, however much of the reservation is standing idle.
#[derive(Clone, Copy, PartialEq)]
enum RateControl {
    /// An average to keep and a peak to burst to, which is what NVENC is
    /// configured with.
    Vbr,
    /// The one mode a driver that has no VBR offers; the peak becomes the rate.
    Cbr,
}

impl RateControl {
    /// The mode this driver supports, and the attribute's value for it. A
    /// driver that offers neither keeps the QP-only behaviour rather than being
    /// asked for a mode it does not have.
    fn select(attribute: &mut VAConfigAttrib) -> Option<Self> {
        let supported = attribute.value;
        let chosen = if supported & VA_RC_VBR != 0 {
            (Self::Vbr, VA_RC_VBR)
        } else if supported & VA_RC_CBR != 0 {
            (Self::Cbr, VA_RC_CBR)
        } else {
            return None;
        };
        attribute.value = chosen.1;
        Some(chosen.0)
    }
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
    /// `None` when the driver offers no rate control at all.
    rate_control: Option<RateControl>,
    /// Reused BGRA to planar conversion output: Y, then U, then V.
    planar: Vec<u8>,
    codec: CodecState,
    force_keyframe: bool,
}

impl Encoder {
    pub(crate) fn new(size: (u32, u32), format: Format, rate: Rate) -> Result<Self> {
        ensure!(size.0 >= 2 * MB && size.1 >= 2 * MB, "编码尺寸过小");
        ensure!(
            Format { depth: 8, ..format } == format,
            "VA-API 输入只支持 8 位"
        );
        let codec = CodecState::new(format).context("VA-API 不支持该编码格式")?;
        let (display, profile, entrypoint) =
            profile_entrypoint(format).context("VA-API 不支持该编码格式")?;

        let mut attrs = vec![
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribRTFormat,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribRateControl,
                value: 0,
            },
        ];
        display
            .get_config_attributes(profile, entrypoint, &mut attrs)
            .context("查询编码配置属性失败")?;
        // The query leaves the modes the driver supports in the value; the
        // config wants the one mode that was picked, and an unsupported one is
        // refused outright, so the attribute goes away when there is no mode.
        let rate_control = RateControl::select(&mut attrs[1]);
        if rate_control.is_none() {
            attrs.pop();
            tracing::warn!(profile, "VA-API 驱动没有码率控制，画面只由质量档位的 QP 决定");
        }
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
            rate_control,
            planar: vec![0; size.0 as usize * size.1 as usize * 3 / 2],
            codec,
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
        // The settled rate, not the request: the controller answers with the
        // frame rate the input actually keeps, and the driver is told that one.
        self.rate = update.rate;
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

    /// The rate this picture is coded under. Both are per picture: the frame
    /// rate is the one the input actually keeps, and the bit rate is what the
    /// transport can carry right now, which congestion moves from frame to
    /// frame. The sequence header carries neither — it is written once, from
    /// the codec's own parameters, and a rate control that read it would fight
    /// the transport instead of following it.
    fn rate_control_buffers(&self) -> Result<Vec<cros_libva::Buffer>> {
        let Some(mode) = self.rate_control else {
            return Ok(Vec::new());
        };
        let target = self.rate.target.max(1);
        let peak = self.rate.peak.max(target);
        // VBR keeps the average and bursts to the peak; the mode without it can
        // only spend the peak throughout.
        let (bit_rate, percentage) = match mode {
            RateControl::Vbr => (peak, (target * 100 / peak).clamp(1, 100)),
            RateControl::Cbr => (peak, 100),
        };
        let framerate = EncMiscParameter::FrameRate(EncMiscParameterFrameRate::new(
            self.rate.fps.max(1),
            0,
        ));
        let control = EncMiscParameter::RateControl(EncMiscParameterRateControl::new(
            bit_rate,
            percentage,
            // Zero asks the driver for its own sliding window.
            0,
            // Zero on the QP bounds leaves the choice to the driver: it starts
            // where every other VA-API encoder starts one, instead of at this
            // preset's QP, which the rate control is about to move anyway.
            0,
            0,
            0,
            RcFlags::new(0, 0, 0, 0, 0, 0, 0, 0, 0),
            0,
            0,
            0,
            0,
        ));
        Ok(vec![
            self.context
                .create_buffer(BufferType::EncMiscParameter(framerate))?,
            self.context
                .create_buffer(BufferType::EncMiscParameter(control))?,
        ])
    }

    fn encode_frame(&mut self, timestamp_100ns: i64, keyframe: bool) -> Result<Vec<Encoded>> {
        // The controller sizes the frame rate it announces from the pictures
        // that actually arrive, so it has to see them; every other backend
        // feeds it here too.
        self.frame_rate.input(timestamp_100ns);
        self.codec.reset(keyframe);
        let (width, height) = self.size;
        let size = (width, height);
        let level = CodecState::level(self.format, size);
        let quality_qp = qp(self.rate.quality);

        let source = self.input.take().context("编码输入 surface 已被取走")?;
        let mut picture = Picture::new(
            timestamp_100ns.max(0) as u64,
            Rc::clone(&self.context),
            source,
        );
        self.upload(&picture)?;

        for buffer in self.codec.buffers(
            &self.context,
            self.coded.id(),
            self.reconstructed_id,
            size,
            self.rate,
            quality_qp,
            keyframe,
        )? {
            picture.add_buffer(buffer);
        }
        for buffer in self.rate_control_buffers()? {
            picture.add_buffer(buffer);
        }

        let picture = picture.begin()?.render()?.end()?;
        let picture = picture.sync().map_err(|(error, _)| error)?;
        self.input = Some(
            picture
                .take_surface()
                .map_err(|_| anyhow::anyhow!("编码输入 surface 仍被占用"))?,
        );
        self.codec.advance();

        let mut payload = Vec::new();
        let mut segments = 0;
        {
            let mapped = MappedCodedBuffer::new(&self.coded)?;
            for segment in mapped.iter() {
                segments += 1;
                payload.extend_from_slice(segment.buf);
            }
        }
        ensure!(segments == 1, "编码器返回了 {segments} 段码流");
        self.codec.patch_nal(&mut payload, keyframe)?;

        let mut data = if keyframe {
            self.codec.parameter_sets(size, level)
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
        if self.codec.wrapping() {
            self.force_keyframe = true;
        }
        let keyframe = keyframe || self.force_keyframe;
        self.force_keyframe = false;
        self.encode_frame(timestamp_100ns, keyframe)
    }
}

/// Encodes synthetic frames and parses the parameter sets back, then leaves the
/// stream in `/tmp` for a decoder to check. Needs a GPU with a VA-API encoder:
/// `cargo test -- --ignored encodes_with_parseable_parameter_sets`.
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
            // BGRA: blue follows the row, green the column, red the sum.
            pixel.copy_from_slice(&[row, column, row.wrapping_add(column), 0xff]);
        }
        Captured::Cpu {
            pixels: std::sync::Arc::new(bgra),
            width,
            height,
        }
    }

    fn check(codec: Codec, media: crate::media::VideoCodec, name: &str) {
        let format = Format {
            codec,
            chroma: 1,
            depth: 8,
        };
        assert!(accepts(format), "VA-API 不接受这个格式");
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
            stream.extend_from_slice(&encoded[0].data);
            let units = crate::media::video_format::annex_b_units(&encoded[0].data);
            println!(
                "frame {index}: {} bytes, keyframe {}, units {}",
                encoded[0].data.len(),
                encoded[0].keyframe,
                units.len()
            );
            if index == 0 {
                let parsed =
                    crate::media::video_format::parse_stream_format(media, &encoded[0].data)
                        .expect("首个 IDR 里没有参数集");
                assert_eq!(parsed.chroma_format_idc, 1);
                assert_eq!(parsed.bit_depth_luma, 8);
                assert_eq!(
                    (parsed.visible_width, parsed.visible_height),
                    size,
                    "参数集里的画面尺寸与请求不一致"
                );
            }
        }
        let extension = match media {
            crate::media::VideoCodec::H264 => "h264",
            crate::media::VideoCodec::H265 => "h265",
            crate::media::VideoCodec::Av1 => "obu",
        };
        let path = format!("/tmp/openuuyc-vaapi-{name}.{extension}");
        std::fs::write(&path, &stream).unwrap();
        println!("wrote {path} ({} bytes)", stream.len());
    }

    #[test]
    #[ignore]
    fn encodes_h264_with_parseable_parameter_sets() {
        check(Codec::H264, crate::media::VideoCodec::H264, "h264");
    }

    #[test]
    #[ignore]
    fn encodes_h265_with_parseable_parameter_sets() {
        check(Codec::H265, crate::media::VideoCodec::H265, "h265");
    }

    /// 用共享的软件解码器量一遍实际花掉的码率与还原质量：被控端的质量旋钮最终
    /// 只体现为司机拿到的码率与 QP，所以要能看到「协商出来的带宽到底花没花出去」。
    /// 画面是仿桌面的一块移动窗口——纯噪声或大面积细纹会把量出来的误差放大成
    /// 假象（见 AGENTS_QA.md）。
    /// `cargo test --release --lib -- --ignored reports_the_rate_it_spends --nocapture`
    #[test]
    #[ignore]
    fn reports_the_rate_it_spends() {
        let size = (1920u32, 1080u32);
        let format = Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        };
        let mut rate = rate();
        rate.target = 14_000_000;
        rate.peak = 14_000_000;
        rate.quality = 4;
        rate.quality_target = QualityTarget {
            bitrate: 14_000_000,
            fps: 30,
        };
        for fps in [30u32, 60] {
            rate.fps = fps;
            let mut encoder = Encoder::new(size, format, rate).unwrap();
            let mut decoder =
                openuuyc_codec::decoder::Decoder::new(crate::media::VideoCodec::H264, &[]).unwrap();
            let frames = 30;
            let mut pending = std::collections::HashMap::new();
            let mut stream = Vec::new();
            let (mut bytes, mut squared, mut compared) = (0usize, 0f64, 0usize);
            for index in 0..frames {
                let timestamp = i64::from(index) * 10_000_000 / i64::from(fps);
                let source = moving(size, index);
                encoder.convert(&source).unwrap();
                let luma = (size.0 * size.1) as usize;
                pending.insert(timestamp, encoder.planar[..luma].to_vec());
                let encoded = encoder.encode_frame(timestamp, index == 0).unwrap();
                bytes += encoded[0].data.len();
                stream.extend_from_slice(&encoded[0].data);
                for frame in &decoder.push(&encoded[0].data, timestamp).unwrap() {
                    let Some(reference) = pending.remove(&frame.pts) else {
                        continue;
                    };
                    let width = frame.coded_width as usize;
                    for row in 0..size.1 as usize {
                        for column in 0..size.0 as usize {
                            let difference = f64::from(
                                frame.data[row * width + column]
                                    .abs_diff(reference[row * width as usize + column]),
                            );
                            squared += difference * difference;
                            compared += 1;
                        }
                    }
                }
            }
            let seconds = f64::from(frames) / f64::from(fps);
            println!(
                "{fps} fps: {frames} frames, {bytes} bytes, {:.2} Mbps, luma RMSE {:.2} \
                 over {compared} samples",
                bytes as f64 * 8.0 / seconds / 1_000_000.0,
                (squared / compared.max(1) as f64).sqrt()
            );
            std::fs::write(format!("/tmp/openuuyc-vaapi-rate-{fps}.h264"), &stream).unwrap();
        }
    }

    /// VPP（`VAEntrypointVideoProc`）能不能吃 RGB32、最大能到多大：GPU 缩放这条路的
    /// 前提。`cargo test --release --lib -- --ignored reports_video_proc --nocapture`
    #[test]
    #[ignore]
    fn reports_video_proc() {
        let (display, _, _) = profile_entrypoint(Format {
            codec: Codec::H264,
            chroma: 1,
            depth: 8,
        })
        .unwrap();
        let Ok(entrypoints) = display.query_config_entrypoints(VAProfile::VAProfileNone) else {
            println!("没有 VAProfileNone");
            return;
        };
        println!(
            "VAProfileNone 支持：{}",
            if entrypoints.contains(&VAEntrypoint::VAEntrypointVideoProc) {
                "VideoProc"
            } else {
                "（没有 VideoProc）"
            }
        );
        let mut attrs = vec![
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribRTFormat,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType::VAConfigAttribMaxPictureHeight,
                value: 0,
            },
        ];
        match display.get_config_attributes(
            VAProfile::VAProfileNone,
            VAEntrypoint::VAEntrypointVideoProc,
            &mut attrs,
        ) {
            Ok(()) => {
                let formats = attrs[0].value;
                println!(
                    "RTFormat 位图 {formats:#x}：YUV420 {}，RGB32 {}",
                    formats & VA_RT_FORMAT_YUV420 != 0,
                    formats & VA_RT_FORMAT_RGB32 != 0
                );
                println!("最大尺寸 {}x{}", attrs[1].value, attrs[2].value);
            }
            Err(error) => println!("get_config_attributes: {error:?}"),
        }
    }

    /// 把编码路径拆开量一遍：CPU 色彩转换、上传+驱动编码各占多少。
    /// `cargo test --release --lib -- --ignored profiles_the_encode_path --nocapture`
    #[test]
    #[ignore]
    fn profiles_the_encode_path() {
        for size in [(1920u32, 1080u32), (3840, 1620), (5120, 2160)] {
            let format = Format {
                codec: Codec::H264,
                chroma: 1,
                depth: 8,
            };
            let mut rate = rate();
            rate.fps = 30;
            rate.target = 20_000_000;
            rate.peak = 20_000_000;
            let mut encoder = Encoder::new(size, format, rate).unwrap();
            let source = moving((size.0, size.1), 0);
            let (mut convert, mut frame) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
            let rounds = 10;
            for index in 0..rounds {
                let at = std::time::Instant::now();
                encoder.convert(&source).unwrap();
                convert += at.elapsed();
                let at = std::time::Instant::now();
                let encoded = encoder
                    .encode_frame(i64::from(index) * 333_333, index == 0)
                    .unwrap();
                frame += at.elapsed();
                std::hint::black_box(encoded);
            }
            println!(
                "{size:?}: convert {:?}, upload+encode {:?}",
                convert / rounds,
                frame / rounds
            );
        }
    }

    /// 仿桌面的一块移动窗口：大片平坦底子 + 若干硬边。窗口每帧平移一段距离，
    /// 编码器不能靠帧间预测把它整块省掉，但也不需要去追像素级的噪声。
    fn moving(size: (u32, u32), index: u32) -> Vec<u8> {
        let (width, height) = size;
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        let window_left = 120 + index * 11;
        let window_top = 90 + index * 3;
        for (position, pixel) in bgra.chunks_exact_mut(4).enumerate() {
            let (x, y) = (position as u32 % width, position as u32 / width);
            let inside_window = x >= window_left
                && x < window_left + 480
                && y >= window_top
                && y < window_top + 320;
            let value: u8 = if inside_window {
                let border = x < window_left + 6 || y < window_top + 6;
                // 窗口里是几行「文字」：平坦底子上的横向细线。
                let text = (y - window_top) % 40 < 3 && x > window_left + 20;
                if border {
                    40
                } else if text {
                    90
                } else {
                    230
                }
            } else {
                200
            };
            // 平坦区域带一点缓慢的横向渐变，和真实桌面的底色一样。
            let value = value.saturating_sub((x / 64 % 4) as u8);
            pixel.copy_from_slice(&[value, value, value, 0xff]);
        }
        bgra
    }

}
