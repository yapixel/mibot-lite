use fast_image_resize::images::{Image as FirImage, ImageRef as FirImageRef};
use fast_image_resize::{FilterType as FirFilter, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{ImageEncoder, ImageFormat, ImageReader, Limits};
use std::io::{Cursor, Write};
use thiserror::Error;
use tiny_skia::{
    BlendMode, Color, FilterQuality, Mask, MaskType, Pixmap, PixmapPaint, Transform,
};

/// Maximum allowed image width or height (matches Go `imaging.MaxDimension`).
pub const MAX_DIMENSION: u32 = 2048;

#[derive(Debug, Error)]
pub enum ImagingError {
    #[error("read image header: {0}")]
    Header(String),
    #[error("image is {0}x{1}, outside the supported range")]
    DimensionsOutOfRange(u32, u32),
    #[error("decode image: {0}")]
    Decode(#[from] image::ImageError),
    #[error("encode PNG: {0}")]
    Encode(String),
    #[error("invalid pixel buffer")]
    InvalidBuffer,
    #[error("not a WebP image")]
    NotWebP,
}

/// Premultiplied RGBA8888 image buffer, matching Go `*image.RGBA` and `tiny_skia::Pixmap`.
#[derive(Debug, Clone)]
pub struct PremulImage {
    pixmap: Pixmap,
}

impl PremulImage {
    pub fn new(width: u32, height: u32) -> Result<Self, ImagingError> {
        let pixmap = Pixmap::new(width.max(1), height.max(1)).ok_or(ImagingError::InvalidBuffer)?;
        Ok(Self { pixmap })
    }

    pub fn from_solid(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> Result<Self, ImagingError> {
        let mut img = Self::new(width, height)?;
        let pr = ((u16::from(r) * u16::from(a) + 127) / 255) as u8;
        let pg = ((u16::from(g) * u16::from(a) + 127) / 255) as u8;
        let pb = ((u16::from(b) * u16::from(a) + 127) / 255) as u8;
        for px in img.pixmap.data_mut().as_chunks_mut::<4>().0 {
            px[0] = pr;
            px[1] = pg;
            px[2] = pb;
            px[3] = a;
        }
        Ok(img)
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        self.pixmap.data_mut()
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let idx = ((y * self.width() + x) * 4) as usize;
        let d = self.pixmap.data();
        Some([d[idx], d[idx + 1], d[idx + 2], d[idx + 3]])
    }

    pub fn set_premul_pixel(&mut self, x: u32, y: u32, px: [u8; 4]) {
        if x < self.width() && y < self.height() {
            let idx = ((y * self.width() + x) * 4) as usize;
            self.pixmap.data_mut()[idx..idx + 4].copy_from_slice(&px);
        }
    }
}

fn straight_to_premul(rgba: &image::RgbaImage) -> Result<PremulImage, ImagingError> {
    let mut out = PremulImage::new(rgba.width(), rgba.height())?;
    for (dst, src) in out
        .pixmap
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(rgba.as_raw().as_chunks::<4>().0.iter())
    {
        let a = u16::from(src[3]);
        if a == 255 {
            dst.copy_from_slice(src);
        } else if a == 0 {
            dst.fill(0);
        } else {
            dst[0] = ((u16::from(src[0]) * a + 127) / 255) as u8;
            dst[1] = ((u16::from(src[1]) * a + 127) / 255) as u8;
            dst[2] = ((u16::from(src[2]) * a + 127) / 255) as u8;
            dst[3] = src[3];
        }
    }
    Ok(out)
}

fn premul_to_straight(img: &PremulImage) -> Vec<u8> {
    let mut straight = Vec::with_capacity(img.data().len());
    for px in img.data().as_chunks::<4>().0 {
        let a = px[3];
        if a == 0 {
            straight.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            straight.extend_from_slice(px);
        } else {
            let af = f32::from(a);
            let r = (f32::from(px[0]) * 255.0 / af).round().clamp(0.0, 255.0) as u8;
            let g = (f32::from(px[1]) * 255.0 / af).round().clamp(0.0, 255.0) as u8;
            let b = (f32::from(px[2]) * 255.0 / af).round().clamp(0.0, 255.0) as u8;
            straight.extend_from_slice(&[r, g, b, a]);
        }
    }
    straight
}

fn header_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits
}

pub fn decode_png(data: &[u8]) -> Result<PremulImage, ImagingError> {
    let mut reader = ImageReader::new(Cursor::new(data));
    reader.set_format(ImageFormat::Png);
    reader.limits(header_limits());
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| ImagingError::Header(e.to_string()))?;
    if w < 1 || h < 1 || w > MAX_DIMENSION || h > MAX_DIMENSION {
        return Err(ImagingError::DimensionsOutOfRange(w, h));
    }
    let mut reader = ImageReader::new(Cursor::new(data));
    reader.set_format(ImageFormat::Png);
    reader.limits(header_limits());
    let dyn_img = reader.decode()?;
    straight_to_premul(&dyn_img.to_rgba8())
}

pub fn decode(data: &[u8]) -> Result<PremulImage, ImagingError> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| ImagingError::Header(e.to_string()))?;
    let format = reader
        .format()
        .ok_or_else(|| ImagingError::Header("unknown image format".into()))?;
    let mut dim_reader = ImageReader::new(Cursor::new(data));
    dim_reader.set_format(format);
    dim_reader.limits(header_limits());
    let (w, h) = dim_reader
        .into_dimensions()
        .map_err(|e| ImagingError::Header(e.to_string()))?;
    if w < 1 || h < 1 || w > MAX_DIMENSION || h > MAX_DIMENSION {
        return Err(ImagingError::DimensionsOutOfRange(w, h));
    }
    let mut dec_reader = ImageReader::new(Cursor::new(data));
    dec_reader.set_format(format);
    dec_reader.limits(header_limits());
    let dyn_img = dec_reader.decode()?;
    straight_to_premul(&dyn_img.to_rgba8())
}

pub fn write_png<W: Write>(writer: W, img: &PremulImage) -> Result<(), ImagingError> {
    let straight = premul_to_straight(img);
    let encoder = PngEncoder::new_with_quality(writer, CompressionType::Default, PngFilter::Adaptive);
    encoder
        .write_image(&straight, img.width(), img.height(), image::ExtendedColorType::Rgba8)
        .map_err(|e| ImagingError::Encode(e.to_string()))
}

pub fn encode_png(img: &PremulImage) -> Result<Vec<u8>, ImagingError> {
    let mut buf = Vec::new();
    write_png(&mut buf, img)?;
    Ok(buf)
}

fn clamp_premul(data: &mut [u8]) {
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3];
        px[0] = px[0].min(a);
        px[1] = px[1].min(a);
        px[2] = px[2].min(a);
    }
}

pub fn resize(source: &PremulImage, width: u32, height: u32) -> Result<PremulImage, ImagingError> {
    let w = width.max(1);
    let h = height.max(1);
    let src = FirImageRef::new(source.width(), source.height(), source.data(), PixelType::U8x4)
        .map_err(|_| ImagingError::InvalidBuffer)?;
    let mut dst_buf = vec![0u8; (w * h * 4) as usize];
    let mut dst = FirImage::from_slice_u8(w, h, &mut dst_buf, PixelType::U8x4)
        .map_err(|_| ImagingError::InvalidBuffer)?;
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FirFilter::CatmullRom))
        .use_alpha(false);
    resizer
        .resize(&src, &mut dst, Some(&opts))
        .map_err(|_| ImagingError::InvalidBuffer)?;
    clamp_premul(&mut dst_buf);
    let mut out = PremulImage::new(w, h)?;
    out.data_mut().copy_from_slice(&dst_buf);
    Ok(out)
}

pub fn resize_cover(
    source: &PremulImage,
    width: u32,
    height: u32,
) -> Result<PremulImage, ImagingError> {
    let w = width.max(1);
    let h = height.max(1);
    let sw = source.width();
    let sh = source.height();
    let scale = (f64::from(w) / f64::from(sw)).max(f64::from(h) / f64::from(sh));
    let crop_w = ((f64::from(w) / scale).round() as u32).clamp(1, sw);
    let crop_h = ((f64::from(h) / scale).round() as u32).clamp(1, sh);
    let crop_x = (sw - crop_w) / 2;
    let crop_y = (sh - crop_h) / 2;

    let src = FirImageRef::new(sw, sh, source.data(), PixelType::U8x4)
        .map_err(|_| ImagingError::InvalidBuffer)?;
    let mut dst_buf = vec![0u8; (w * h * 4) as usize];
    let mut dst = FirImage::from_slice_u8(w, h, &mut dst_buf, PixelType::U8x4)
        .map_err(|_| ImagingError::InvalidBuffer)?;
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FirFilter::CatmullRom))
        .crop(
            f64::from(crop_x),
            f64::from(crop_y),
            f64::from(crop_w),
            f64::from(crop_h),
        )
        .use_alpha(false);
    resizer
        .resize(&src, &mut dst, Some(&opts))
        .map_err(|_| ImagingError::InvalidBuffer)?;
    clamp_premul(&mut dst_buf);
    let mut out = PremulImage::new(w, h)?;
    out.data_mut().copy_from_slice(&dst_buf);
    Ok(out)
}

pub fn rotate(source: &PremulImage, degrees: f64) -> Result<PremulImage, ImagingError> {
    let mut out = PremulImage::new(source.width(), source.height())?;
    let cx = source.width() as f32 / 2.0;
    let cy = source.height() as f32 / 2.0;
    let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: BlendMode::Source,
        quality: FilterQuality::Bicubic,
    };
    out.pixmap.draw_pixmap(
        0,
        0,
        source.pixmap.as_ref(),
        &paint,
        Transform::from_rotate_at(degrees as f32, cx, cy),
        None,
    );
    Ok(out)
}

pub fn rotate_expand(source: &PremulImage, degrees: f64) -> Result<PremulImage, ImagingError> {
    let radians = degrees.to_radians();
    let sin = radians.sin().abs();
    let cos = radians.cos().abs();
    let sw = f64::from(source.width());
    let sh = f64::from(source.height());
    let width = (sw * cos + sh * sin).ceil().max(1.0) as u32;
    let height = (sw * sin + sh * cos).ceil().max(1.0) as u32;

    let mut out = PremulImage::new(width, height)?;
    let dx = width as f32 / 2.0 - source.width() as f32 / 2.0;
    let dy = height as f32 / 2.0 - source.height() as f32 / 2.0;
    let transform = Transform::from_translate(dx, dy)
        .post_rotate_at(degrees as f32, width as f32 / 2.0, height as f32 / 2.0);
    let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: BlendMode::Source,
        quality: FilterQuality::Bicubic,
    };
    out.pixmap
        .draw_pixmap(0, 0, source.pixmap.as_ref(), &paint, transform, None);
    Ok(out)
}

pub fn opacity(source: &PremulImage, factor: f64) -> PremulImage {
    let mut out = source.clone();
    let f = factor.clamp(0.0, 1.0);
    for b in out.pixmap.data_mut() {
        *b = (f64::from(*b) * f).round().clamp(0.0, 255.0) as u8;
    }
    out
}

pub fn brightness(source: &PremulImage, factor: f64) -> PremulImage {
    let mut out = source.clone();
    if (factor - 1.0).abs() < f64::EPSILON {
        return out;
    }
    for px in out.pixmap.data_mut().as_chunks_mut::<4>().0 {
        let alpha = f64::from(px[3]);
        for c in &mut px[0..3] {
            *c = (f64::from(*c) * factor).clamp(0.0, alpha) as u8;
        }
    }
    out
}

pub fn apply_mask(source: &PremulImage, mask_img: &PremulImage) -> Result<PremulImage, ImagingError> {
    let mut out = PremulImage::new(mask_img.width(), mask_img.height())?;
    let mask = Mask::from_pixmap(mask_img.pixmap.as_ref(), MaskType::Alpha);
    let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: BlendMode::SourceOver,
        quality: FilterQuality::Nearest,
    };
    out.pixmap.draw_pixmap(
        0,
        0,
        source.pixmap.as_ref(),
        &paint,
        Transform::identity(),
        Some(&mask),
    );
    Ok(out)
}

pub fn composite(canvas: &mut PremulImage, overlay: &PremulImage, x: i32, y: i32) {
    let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: BlendMode::SourceOver,
        quality: FilterQuality::Nearest,
    };
    canvas.pixmap.draw_pixmap(
        x,
        y,
        overlay.pixmap.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
}

pub fn flatten(source: &PremulImage, r: u8, g: u8, b: u8) -> Result<PremulImage, ImagingError> {
    let mut out = PremulImage::new(source.width(), source.height())?;
    out.pixmap.fill(Color::from_rgba8(r, g, b, 255));
    composite(&mut out, source, 0, 0);
    Ok(out)
}

/// Reads dimensions directly from a WebP RIFF header (`VP8X`, `VP8 `, `VP8L`).
pub fn webp_size(data: &[u8]) -> Result<(u32, u32), ImagingError> {
    if data.len() < 30 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(ImagingError::NotWebP);
    }
    match &data[12..16] {
        b"VP8X" => {
            let w = u32::from(data[24]) | (u32::from(data[25]) << 8) | (u32::from(data[26]) << 16);
            let h = u32::from(data[27]) | (u32::from(data[28]) << 8) | (u32::from(data[29]) << 16);
            Ok((w + 1, h + 1))
        }
        b"VP8 " => {
            if data[23] != 0x9d || data[24] != 0x01 || data[25] != 0x2a {
                return Err(ImagingError::NotWebP);
            }
            let w = u16::from_le_bytes([data[26], data[27]]) & 0x3fff;
            let h = u16::from_le_bytes([data[28], data[29]]) & 0x3fff;
            Ok((u32::from(w), u32::from(h)))
        }
        b"VP8L" => {
            if data[20] != 0x2f {
                return Err(ImagingError::NotWebP);
            }
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let w = (bits & 0x3fff) + 1;
            let h = ((bits >> 14) & 0x3fff) + 1;
            Ok((w, h))
        }
        _ => Err(ImagingError::NotWebP),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let source = PremulImage::from_solid(8, 6, 200, 100, 50, 255).unwrap();
        let encoded = encode_png(&source).unwrap();
        let decoded = decode_png(&encoded).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 6));
        let px = decoded.pixel(3, 3).unwrap();
        assert_eq!((px[0], px[1], px[2]), (200, 100, 50));
    }

    #[test]
    fn decode_rejects_oversize_and_garbage() {
        let oversize = PremulImage::new(MAX_DIMENSION + 1, 1).unwrap();
        let encoded = encode_png(&oversize).unwrap();
        assert!(decode_png(&encoded).is_err());
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn resize_cover_crops_centre() {
        let mut source = PremulImage::from_solid(200, 100, 0, 0, 0, 255).unwrap();
        for y in 0..100 {
            for x in 50..150 {
                source.set_premul_pixel(x, y, [255, 0, 0, 255]);
            }
        }
        let result = resize_cover(&source, 50, 50).unwrap();
        assert_eq!((result.width(), result.height()), (50, 50));
        assert!(result.pixel(25, 25).unwrap()[0] >= 200);
    }

    #[test]
    fn rotate_keeps_canvas_size() {
        let src = PremulImage::from_solid(30, 20, 0, 0, 255, 255).unwrap();
        let turned = rotate(&src, 45.0).unwrap();
        assert_eq!((turned.width(), turned.height()), (30, 20));
    }

    #[test]
    fn apply_mask_uses_alpha() {
        let face = PremulImage::from_solid(10, 10, 255, 0, 0, 255).unwrap();
        let mut mask = PremulImage::new(10, 10).unwrap();
        for y in 0..10 {
            for x in 0..5 {
                mask.set_premul_pixel(x, y, [0, 0, 0, 255]);
            }
        }
        let result = apply_mask(&face, &mask).unwrap();
        assert_eq!(result.pixel(2, 2).unwrap()[3], 255);
        assert_eq!(result.pixel(8, 2).unwrap()[3], 0);
    }

    #[test]
    fn brightness_clamps_to_alpha() {
        let mut src = PremulImage::new(2, 1).unwrap();
        src.set_premul_pixel(0, 0, [100, 100, 100, 128]);
        src.set_premul_pixel(1, 0, [10, 10, 10, 255]);
        let bright = brightness(&src, 4.0);
        for px in bright.data().as_chunks::<4>().0 {
            assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
        }
        assert_eq!(brightness(&src, 0.5).pixel(0, 0).unwrap()[0], 50);
    }

    #[test]
    fn composite_and_flatten() {
        let mut canvas = PremulImage::from_solid(20, 20, 0, 0, 0, 255).unwrap();
        let overlay = PremulImage::from_solid(4, 4, 0, 255, 0, 255).unwrap();
        composite(&mut canvas, &overlay, 10, 10);
        assert_eq!(canvas.pixel(11, 11).unwrap()[1], 255);
        assert_eq!(canvas.pixel(2, 2).unwrap()[1], 0);

        let empty = PremulImage::new(2, 1).unwrap();
        let flat = flatten(&empty, 0, 0, 0).unwrap();
        assert_eq!(flat.pixel(0, 0).unwrap()[3], 255);
    }

    #[test]
    fn webp_size_parses_headers() {
        let mut lossy = vec![0u8; 30];
        lossy[0..4].copy_from_slice(b"RIFF");
        lossy[8..16].copy_from_slice(b"WEBPVP8 ");
        lossy[23..26].copy_from_slice(&[0x9d, 0x01, 0x2a]);
        lossy[26..28].copy_from_slice(&512u16.to_le_bytes());
        lossy[28..30].copy_from_slice(&768u16.to_le_bytes());
        assert_eq!(webp_size(&lossy).unwrap(), (512, 768));

        let mut extended = vec![0u8; 30];
        extended[0..4].copy_from_slice(b"RIFF");
        extended[8..16].copy_from_slice(b"WEBPVP8X");
        extended[24..27].copy_from_slice(&[0xff, 0x01, 0x00]);
        extended[27..30].copy_from_slice(&[0xff, 0x02, 0x00]);
        assert_eq!(webp_size(&extended).unwrap(), (512, 768));

        assert!(webp_size(b"short").is_err());
        assert!(webp_size(&[0u8; 40]).is_err());
    }

    #[test]
    fn rotate_expand_and_opacity() {
        let src = PremulImage::from_solid(40, 10, 0, 255, 0, 255).unwrap();
        let turned = rotate_expand(&src, 90.0).unwrap();
        assert!((10..=11).contains(&turned.width()));
        assert!((40..=41).contains(&turned.height()));

        let diag = rotate_expand(&src, 45.0).unwrap();
        assert_eq!(diag.pixel(0, 0).unwrap()[3], 0);
        assert!(diag.pixel(diag.width() / 2, diag.height() / 2).unwrap()[1] >= 250);

        let solid_red = PremulImage::from_solid(2, 2, 200, 0, 0, 255).unwrap();
        let half = opacity(&solid_red, 0.5);
        let px = half.pixel(1, 1).unwrap();
        assert_eq!((px[0], px[3]), (100, 128));
        assert_eq!(solid_red.pixel(0, 0).unwrap()[0], 200);
    }
}
