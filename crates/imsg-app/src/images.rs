//! Decoding pictures (JPEG, PNG, GIF first frame, WebP) into scaled-down
//! RGBA buffers, off the UI thread. HEIC and everything else is shown as
//! a file row instead.

use image::{ImageReader, Limits};

/// A decoded picture, ready for `slint::Image::from_rgba8`.
#[derive(Clone)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for Rgba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rgba({}x{})", self.width, self.height)
    }
}

/// Inline pictures fit in this box (logical px; the transcript scales
/// them further if the window is narrow).
pub const INLINE_MAX: (u32, u32) = (360, 360);
pub const AVATAR: u32 = 80;

/// Corner radius baked into inline pictures (px of the decoded picture;
/// kmessages rounds them like bubbles).
pub const PICTURE_RADIUS: f32 = 16.0;

/// Which corners of a picture to round (the software renderer cannot clip
/// images, so the corners are made transparent in the pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corners {
    All,
    /// A link card's preview picture: the card continues below it.
    Top,
}

/// Decode and shrink to fit `max` (never enlarged), corners rounded.
pub fn decode_picture(data: &[u8], max: (u32, u32), corners: Corners) -> Option<Rgba> {
    let mut r = decode_fit(data, max)?;
    round_corners(&mut r, PICTURE_RADIUS, corners);
    Some(r)
}

/// Make the corners outside a quarter circle of `radius` transparent
/// (anti-aliased over one pixel).
pub fn round_corners(img: &mut Rgba, radius: f32, corners: Corners) {
    let (w, h) = (img.width as f32, img.height as f32);
    let r = radius.min(w / 2.0).min(h / 2.0);
    if r < 1.0 {
        return;
    }
    for y in 0..img.height {
        let py = y as f32 + 0.5;
        // Distance into the corner zone vertically (0 = outside it).
        let dy = if py < r {
            r - py
        } else if corners == Corners::All && py > h - r {
            py - (h - r)
        } else {
            continue;
        };
        for x in 0..img.width {
            let px = x as f32 + 0.5;
            let dx = if px < r {
                r - px
            } else if px > w - r {
                px - (w - r)
            } else {
                continue;
            };
            let cover = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if cover < 1.0 {
                scale_alpha(img, x, y, cover);
            }
        }
    }
}

fn scale_alpha(img: &mut Rgba, x: u32, y: u32, f: f32) {
    let i = ((y * img.width + x) * 4 + 3) as usize;
    if let Some(a) = img.pixels.get_mut(i) {
        *a = (f32::from(*a) * f).round() as u8;
    }
}

/// Clip to a circle (anti-aliased edge): avatars.
pub fn circle_mask(img: &mut Rgba) {
    let r = img.width.min(img.height) as f32 / 2.0;
    let (cx, cy) = (img.width as f32 / 2.0, img.height as f32 / 2.0);
    for y in 0..img.height {
        for x in 0..img.width {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let cover = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if cover < 1.0 {
                scale_alpha(img, x, y, cover);
            }
        }
    }
}

/// Decode and shrink to fit `max` (never enlarged). `None` if the bytes
/// are not a supported picture or too large to decode safely.
pub fn decode_fit(data: &[u8], max: (u32, u32)) -> Option<Rgba> {
    let mut r = ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    r.limits(limits);
    let img = r.decode().ok()?;
    let img = if img.width() > max.0 || img.height() > max.1 { img.thumbnail(max.0, max.1) } else { img };
    let rgba = img.into_rgba8();
    Some(Rgba { width: rgba.width(), height: rgba.height(), pixels: rgba.into_raw() })
}

/// A round avatar: centre-cropped to a square, scaled, then clipped to a
/// circle (transparent outside).
pub fn decode_avatar(data: &[u8]) -> Option<Rgba> {
    let mut r = ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8_192);
    limits.max_image_height = Some(8_192);
    r.limits(limits);
    let img = r.decode().ok()?;
    let side = img.width().min(img.height());
    let img = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side).thumbnail(AVATAR, AVATAR);
    let rgba = img.into_rgba8();
    let mut out = Rgba { width: rgba.width(), height: rgba.height(), pixels: rgba.into_raw() };
    circle_mask(&mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([240, 155, 120, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decodes_and_scales() {
        let r = decode_fit(&png(1200, 600), INLINE_MAX).unwrap();
        assert_eq!((r.width, r.height), (360, 180));
        assert_eq!(r.pixels.len(), 360 * 180 * 4);
        let small = decode_fit(&png(10, 20), INLINE_MAX).unwrap();
        assert_eq!((small.width, small.height), (10, 20));
        let a = decode_avatar(&png(300, 200)).unwrap();
        assert_eq!((a.width, a.height), (AVATAR, AVATAR));
        // Round: corners transparent, centre and edge midpoints opaque.
        let alpha = |r: &Rgba, x: u32, y: u32| r.pixels[((y * r.width + x) * 4 + 3) as usize];
        assert_eq!(alpha(&a, 0, 0), 0);
        assert_eq!(alpha(&a, AVATAR - 1, AVATAR - 1), 0);
        assert_eq!(alpha(&a, AVATAR / 2, AVATAR / 2), 255);
        assert_eq!(alpha(&a, AVATAR / 2, 1), 255);
        assert_eq!(alpha(&a, 1, AVATAR / 2), 255);
        assert!(decode_fit(b"not a picture", INLINE_MAX).is_none());
        // HEIC-ish bytes: unsupported, no panic.
        assert!(decode_fit(b"\0\0\0\x18ftypheic\0\0\0\0", INLINE_MAX).is_none());
    }

    #[test]
    fn rounded_picture_corners() {
        let alpha = |r: &Rgba, x: u32, y: u32| r.pixels[((y * r.width + x) * 4 + 3) as usize];
        let all = decode_picture(&png(200, 100), INLINE_MAX, Corners::All).unwrap();
        for (x, y) in [(0, 0), (199, 0), (0, 99), (199, 99)] {
            assert_eq!(alpha(&all, x, y), 0, "corner {x},{y}");
        }
        assert_eq!(alpha(&all, 100, 0), 255);
        assert_eq!(alpha(&all, 0, 50), 255);
        assert_eq!(alpha(&all, 100, 50), 255);
        let top = decode_picture(&png(200, 100), INLINE_MAX, Corners::Top).unwrap();
        assert_eq!((alpha(&top, 0, 0), alpha(&top, 199, 0)), (0, 0));
        assert_eq!((alpha(&top, 0, 99), alpha(&top, 199, 99)), (255, 255));
        // Tiny pictures: no panic, nothing to round.
        let mut tiny = decode_fit(&png(1, 1), INLINE_MAX).unwrap();
        round_corners(&mut tiny, PICTURE_RADIUS, Corners::All);
        assert_eq!(alpha(&tiny, 0, 0), 255);
    }
}
