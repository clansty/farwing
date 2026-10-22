//! The device-card picture made from the desktop wallpaper, the same on every
//! platform: the platform backends only find and read the wallpaper file.
use anyhow::{Result, ensure};

/// The largest wallpaper file read for the card.
pub(crate) const MAXIMUM_FILE: u64 = 64 * 1024 * 1024;

/// Decodes a wallpaper image and composes the 16:9 JPEG device card.
/// `logo` 决定卡片上叠不叠程序图标，纯属本机偏好，与平台无关。
pub(crate) fn card(bytes: Vec<u8>, logo: bool) -> Result<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    use image::ImageDecoder;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut decoded = image::DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    compose(decoded, logo)
}

fn compose(decoded: image::DynamicImage, logo: bool) -> Result<Vec<u8>> {
    // Device cards use 16:9: crop centrally without stretching the source.
    // Use integer 16x9 units so both the crop and output have the exact ratio.
    let units = (decoded.width() / 16).min(decoded.height() / 9);
    ensure!(units > 0, "壁纸尺寸不足 16×9");
    let (width, height) = (units * 16, units * 9);
    let crop = decoded.crop_imm(
        (decoded.width() - width) / 2,
        (decoded.height() - height) / 2,
        width,
        height,
    );
    let mut wallpaper = if width > 3200 {
        crop.resize_exact(3200, 1800, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
    } else {
        crop.to_rgba8()
    };
    if logo {
        // 图标按卡片宽度的 15% 缩放，落在右上角；关掉开关就只剩壁纸本身。
        let size = (wallpaper.width() * 15 / 100).max(1);
        let icon = image::load_from_memory(include_bytes!("../../assets/icon-256.png"))?
            .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let position = (
            i64::from(wallpaper.width() - size - wallpaper.width() / 10),
            i64::from(wallpaper.height() / 8),
        );
        image::imageops::overlay(&mut wallpaper, &icon, position.0, position.1);
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88)
        .encode_image(&image::DynamicImage::ImageRgba8(wallpaper).to_rgb8())?;
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Vec<u8> {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(320, 180, image::Rgba([12, 34, 56, 255]))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        png.into_inner()
    }

    #[test]
    fn logo_is_optional() {
        let plain = card(source(), false).unwrap();
        let marked = card(source(), true).unwrap();
        let size = |bytes: &[u8]| {
            image::load_from_memory(bytes)
                .unwrap()
                .to_rgba8()
                .dimensions()
        };
        assert_eq!(size(&plain), (320, 180));
        assert_eq!(size(&marked), (320, 180));
        assert_ne!(plain, marked, "关掉 logo 的卡片不该和带 logo 的一模一样");
    }
}
