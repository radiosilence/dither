//! From uploaded bytes to what is stored: the original with its metadata
//! removed, a thumbnail for grids, a display copy for pictures too large to
//! send whole to a phone, and a JPEG for link previews.
//!
//! Derived copies are lossy WebP: a third smaller than JPEG at the same
//! quality, and they keep transparency. They carry the original's colour
//! profile, so a wide-gamut photo looks the same in its thumbnail as in the
//! original; a profile for some other colour model (CMYK) is left off, since
//! the pixels have been converted to RGB by then.

use std::io::Cursor;

use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};
use sha2::{Digest, Sha256};

use crate::sanitize::{self, Kind};

/// A thumbnail's shorter side: sharp in a grid tile on a 3× phone screen.
pub const THUMB_SHORT: u32 = 480;
/// A thumbnail's longer side at most, so a panorama's stays small.
pub const THUMB_LONG: u32 = 1440;
/// A picture longer than this on either side gets a display copy this long.
pub const DISPLAY_MAX: u32 = 2048;
const THUMB_QUALITY: f32 = 74.0;
const DISPLAY_QUALITY: f32 = 82.0;
/// A link preview's longer side: the width Facebook, Reddit and WhatsApp draw
/// a large card at.
pub const PREVIEW_MAX: u32 = 1200;
/// What a link preview is held to. WhatsApp shows no picture for an
/// `og:image` much larger than this, and says so nowhere.
pub const PREVIEW_BYTES: usize = 300_000;
/// Qualities tried in turn until a preview fits [`PREVIEW_BYTES`].
const PREVIEW_QUALITIES: [u8; 6] = [82, 72, 62, 50, 40, 30];

pub struct Processed {
    /// As shown: after the orientation is applied.
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    pub original: Vec<u8>,
    pub thumb: Vec<u8>,
    pub display: Option<Vec<u8>>,
    pub preview: Vec<u8>,
    pub sha256: [u8; 32],
}

/// Why an upload was refused, in words for the person who sent it.
#[derive(Debug, thiserror::Error)]
pub enum Reject {
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    TooBig(String),
    #[error("{0}")]
    Broken(String),
}

fn decoder(bytes: &[u8], kind: Kind) -> Result<impl ImageDecoder + '_, Reject> {
    let format = kind
        .image_format()
        .ok_or(Reject::Unsupported("a video is not an image"))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    // A 100-megapixel picture at 16 bits a channel, and a little over.
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    reader
        .into_decoder()
        .map_err(|e| Reject::Broken(format!("the image can't be read: {e}")))
}

fn check_size(w: u32, h: u32, max_pixels: u64) -> Result<(), Reject> {
    if w == 0 || h == 0 {
        return Err(Reject::Broken("the image has no pixels".into()));
    }
    let pixels = u64::from(w) * u64::from(h);
    if pixels > max_pixels {
        return Err(Reject::TooBig(format!(
            "{w}×{h} is {} megapixels; the limit is {}",
            pixels.div_ceil(1_000_000),
            max_pixels / 1_000_000
        )));
    }
    Ok(())
}

/// The most memory [`process`] takes for a file of `size` bytes, read from
/// its `head` (the first bytes, or all of them) so the budget can be held
/// before the file is: the file and its cleaned copy, the decoded pixels
/// (16-bit ones take twice the room) beside the eight-bit copy made from
/// them, as much again while a photo turns upright, the display copy with
/// the resizer's and encoder's working rows, and the preview with its
/// flattened copy. A header that runs past `head`
/// reads as broken.
pub fn cost(head: &[u8], size: u64, max_pixels: u64) -> Result<u64, Reject> {
    let kind = sanitize::sniff(head).map_err(Reject::Unsupported)?;
    let decoder = decoder(head, kind)?;
    let (w, h) = decoder.dimensions();
    check_size(w, h, max_pixels)?;
    let eight = 4 * u64::from(w) * u64::from(h);
    let copies = 4 * u64::from(DISPLAY_MAX) * (u64::from(w.max(h)) + u64::from(DISPLAY_MAX));
    let preview = 8 * u64::from(PREVIEW_MAX) * u64::from(PREVIEW_MAX);
    Ok(2 * size + decoder.total_bytes().max(eight) + eight + copies + preview)
}

/// What sizing a picture reads before anything else: every header dither
/// takes ends well within it.
pub const HEAD: u64 = 4 << 20;

/// The first `n` bytes of the file at `path`, or all of a shorter one.
pub async fn read_head(path: &std::path::Path, n: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut head = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(n)
        .read_to_end(&mut head)
        .await?;
    Ok(head)
}

/// Why a file could not be sized.
#[derive(Debug)]
pub enum Unsized {
    /// The file is not one dither takes, or is too large to.
    Refused(Reject),
    /// It could not be read whole, or is shorter than `size`.
    Unread(std::io::Error),
}

/// [`cost`] for the `size`-byte file at `path`: from its first [`HEAD`]
/// bytes, so a file waiting for the budget is not held in memory, or from
/// the whole file when its header runs past them.
pub async fn cost_of(path: &std::path::Path, size: u64, max_pixels: u64) -> Result<u64, Unsized> {
    let head = read_head(path, HEAD).await.map_err(Unsized::Unread)?;
    if (head.len() as u64) < size.min(HEAD) {
        return Err(Unsized::Unread(std::io::ErrorKind::UnexpectedEof.into()));
    }
    match cost(&head, size, max_pixels) {
        Err(Reject::Broken(_)) if (head.len() as u64) < size => {
            let all = tokio::fs::read(path).await.map_err(Unsized::Unread)?;
            cost(&all, size, max_pixels).map_err(Unsized::Refused)
        }
        other => other.map_err(Unsized::Refused),
    }
}

/// A video's thumbnail and link preview, from a frame of it: what
/// [`process`] makes of a picture, without the original or a display copy,
/// which a video has of its own.
pub struct Stills {
    pub thumb: Vec<u8>,
    pub preview: Vec<u8>,
}

pub fn stills(frame: &[u8], max_pixels: u64) -> Result<Stills, Reject> {
    let kind = sanitize::sniff(frame).map_err(Reject::Unsupported)?;
    let (img, icc) = decoded(frame, kind, max_pixels, None)?;
    let (width, height) = (img.width(), img.height());
    let thumb = resize(&img, thumb_size(width, height));
    Ok(Stills {
        preview: jpeg_preview(&img, preview_size(width, height, false), icc.as_deref()),
        thumb: webp(&thumb, THUMB_QUALITY, icc.as_deref()),
    })
}

pub fn process(bytes: &[u8], max_pixels: u64) -> Result<Processed, Reject> {
    let kind = sanitize::sniff(bytes).map_err(Reject::Unsupported)?;
    let orientation = decoder(bytes, kind)?
        .orientation()
        .unwrap_or(Orientation::NoTransforms);
    let clean = sanitize::clean(kind, bytes, orientation.to_exif())
        .map_err(|e| Reject::Broken(format!("the file is damaged: {e}")))?;

    // Everything from here reads the cleaned file, so what is stored is
    // proven to decode.
    let (img, icc) = decoded(&clean.bytes, kind, max_pixels, Some(orientation))?;
    let (width, height) = (img.width(), img.height());

    let display = (!clean.animated && width.max(height) > DISPLAY_MAX)
        .then(|| resize(&img, fit(width, height, DISPLAY_MAX)));
    let thumb = resize(display.as_ref().unwrap_or(&img), thumb_size(width, height));
    // An animated picture's preview is a still, as its thumbnail is.
    let still = if clean.animated {
        &thumb
    } else {
        display.as_ref().unwrap_or(&img)
    };
    let preview = jpeg_preview(
        still,
        preview_size(width, height, clean.animated),
        icc.as_deref(),
    );
    Ok(Processed {
        preview,
        width,
        height,
        animated: clean.animated,
        sha256: Sha256::digest(&clean.bytes).into(),
        original: clean.bytes,
        thumb: webp(&thumb, THUMB_QUALITY, icc.as_deref()),
        display: display.map(|d| webp(&d, DISPLAY_QUALITY, icc.as_deref())),
    })
}

/// A link preview's size for a `w`×`h` image. An animated one's is made from
/// its thumbnail (a still), so it is no larger than that.
pub fn preview_size(w: u32, h: u32, animated: bool) -> (u32, u32) {
    let (w, h) = if animated { thumb_size(w, h) } else { (w, h) };
    fit(w, h, PREVIEW_MAX)
}

/// A link preview of `source`, one of an image's stored copies, at `size`: a
/// JPEG, which every preview fetcher takes, with transparency laid over white
/// and the colour profile kept, at the best quality that fits
/// [`PREVIEW_BYTES`]. The source is at most [`DISPLAY_MAX`] on a side (a
/// display copy, a thumbnail, or an original too small to have a display
/// copy), so this needs little memory.
pub fn preview(source: &[u8], size: (u32, u32)) -> Result<Vec<u8>, Reject> {
    let kind = sanitize::sniff(source).map_err(Reject::Unsupported)?;
    let max = u64::from(DISPLAY_MAX) * u64::from(DISPLAY_MAX);
    let (img, icc) = decoded(source, kind, max, None)?;
    Ok(jpeg_preview(&img, size, icc.as_deref()))
}

/// `bytes` decoded at eight bits a channel and turned upright, by
/// `orientation` or else by the file's own, with its colour profile when that
/// is an RGB one.
fn decoded(
    bytes: &[u8],
    kind: Kind,
    max_pixels: u64,
    orientation: Option<Orientation>,
) -> Result<(DynamicImage, Option<Vec<u8>>), Reject> {
    let mut decoder = decoder(bytes, kind)?;
    let (w, h) = decoder.dimensions();
    check_size(w, h, max_pixels)?;
    let orientation = orientation
        .or_else(|| decoder.orientation().ok())
        .unwrap_or(Orientation::NoTransforms);
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|p| p.get(16..20) == Some(b"RGB "));
    let img = DynamicImage::from_decoder(decoder)
        .map_err(|e| Reject::Broken(format!("the image can't be decoded: {e}")))?;
    // Eight bits first, so the decoded buffer is freed before turning makes
    // a copy.
    let mut img = eight_bit(img);
    img.apply_orientation(orientation);
    Ok((img, icc))
}

fn jpeg_preview(img: &DynamicImage, size: (u32, u32), icc: Option<&[u8]>) -> Vec<u8> {
    let rgb = match resize(img, size) {
        DynamicImage::ImageRgba8(b) => {
            let mut flat = image::RgbImage::new(b.width(), b.height());
            for (to, from) in flat.pixels_mut().zip(b.pixels()) {
                let a = u16::from(from.0[3]);
                for c in 0..3 {
                    to.0[c] = ((u16::from(from.0[c]) * a + 255 * (255 - a) + 127) / 255) as u8;
                }
            }
            flat
        }
        DynamicImage::ImageRgb8(b) => b,
        _ => unreachable!("eight_bit makes every image RGB or RGBA"),
    };
    let mut out = Vec::new();
    for quality in PREVIEW_QUALITIES {
        out.clear();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        if let Some(profile) = icc {
            let _ = image::ImageEncoder::set_icc_profile(&mut encoder, profile.to_vec());
        }
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .expect("an RGB buffer of its own size encodes");
        if out.len() <= PREVIEW_BYTES {
            return out;
        }
    }
    tracing::warn!(
        bytes = out.len(),
        "a link preview is over its size at the lowest quality"
    );
    out
}

/// RGB or RGBA at eight bits a channel, which the resizer and the encoder
/// take, converted in one step so no more than the source and the result are
/// held at once. An alpha channel nothing uses is dropped: screenshots arrive
/// as RGBA with every pixel opaque, and the channel would cost bytes in every
/// copy.
fn eight_bit(img: DynamicImage) -> DynamicImage {
    let alpha = !opaque(&img);
    match img {
        DynamicImage::ImageRgb8(_) => img,
        DynamicImage::ImageRgba8(_) if alpha => img,
        other if alpha => DynamicImage::ImageRgba8(other.into_rgba8()),
        other => DynamicImage::ImageRgb8(other.into_rgb8()),
    }
}

/// Whether every pixel is fully opaque, as an image with no alpha is.
fn opaque(img: &DynamicImage) -> bool {
    match img {
        DynamicImage::ImageRgba8(b) => b.pixels().all(|p| p.0[3] == u8::MAX),
        DynamicImage::ImageLumaA8(b) => b.pixels().all(|p| p.0[1] == u8::MAX),
        DynamicImage::ImageRgba16(b) => b.pixels().all(|p| p.0[3] == u16::MAX),
        DynamicImage::ImageLumaA16(b) => b.pixels().all(|p| p.0[1] == u16::MAX),
        DynamicImage::ImageRgba32F(b) => b.pixels().all(|p| p.0[3] >= 1.0),
        other => !other.color().has_alpha(),
    }
}

/// `w`×`h` scaled down, keeping its shape, until neither side passes `max`.
pub fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w.max(h) <= max {
        return (w, h);
    }
    let scale = |a: u32, b: u32| {
        ((u64::from(a) * u64::from(max) + u64::from(b) / 2) / u64::from(b)).max(1) as u32
    };
    if w >= h {
        (max, scale(h, w))
    } else {
        (scale(w, h), max)
    }
}

/// A thumbnail's size: the shorter side `THUMB_SHORT`, so a square crop of it
/// fills a tile, with the longer side held to `THUMB_LONG`.
pub fn thumb_size(w: u32, h: u32) -> (u32, u32) {
    let short = w.min(h);
    let (tw, th) = if short > THUMB_SHORT {
        let scale = |a: u32| {
            ((u64::from(a) * u64::from(THUMB_SHORT) + u64::from(short) / 2) / u64::from(short))
                .max(1) as u32
        };
        (scale(w), scale(h))
    } else {
        (w, h)
    };
    fit(tw, th, THUMB_LONG)
}

fn resize(src: &DynamicImage, (w, h): (u32, u32)) -> DynamicImage {
    if (w, h) == (src.width(), src.height()) {
        return src.clone();
    }
    let alpha = src.color().has_alpha();
    let pixel_type = if alpha {
        PixelType::U8x4
    } else {
        PixelType::U8x3
    };
    let mut dst = fast_image_resize::images::Image::new(w, h, pixel_type);
    Resizer::new()
        .resize(
            src,
            &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .expect("an RGB or RGBA source resizes into a buffer of its own pixel type");
    let buf = dst.into_vec();
    if alpha {
        DynamicImage::ImageRgba8(image::RgbaImage::from_raw(w, h, buf).expect("w×h×4 bytes"))
    } else {
        DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, buf).expect("w×h×3 bytes"))
    }
}

fn webp(img: &DynamicImage, quality: f32, icc: Option<&[u8]>) -> Vec<u8> {
    let encoded = match img {
        DynamicImage::ImageRgba8(b) => {
            webp::Encoder::from_rgba(b.as_raw(), b.width(), b.height()).encode(quality)
        }
        DynamicImage::ImageRgb8(b) => {
            webp::Encoder::from_rgb(b.as_raw(), b.width(), b.height()).encode(quality)
        }
        _ => unreachable!("eight_bit makes every image RGB or RGBA"),
    };
    match icc {
        Some(profile) => with_icc(
            &encoded,
            profile,
            img.width(),
            img.height(),
            img.color().has_alpha(),
        ),
        None => encoded.to_vec(),
    }
}

/// `webp` with an ICC profile: the extended header, flagged as carrying one,
/// then the profile, then the image. libwebp's simple encoder writes neither.
fn with_icc(webp: &[u8], icc: &[u8], w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let Ok(chunks) = sanitize::riff_chunks(webp) else {
        return webp.to_vec();
    };
    let (mut header, rest) = match chunks.split_first() {
        Some((([b'V', b'P', b'8', b'X'], data), rest)) => (data.to_vec(), rest),
        _ => {
            let mut x = vec![if alpha { 0x10 } else { 0 }, 0, 0, 0];
            x.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
            x.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
            (x, &chunks[..])
        }
    };
    header[0] |= 0x20;
    let mut all = vec![(*b"VP8X", &header[..]), (*b"ICCP", icc)];
    all.extend(rest.iter().copied());
    sanitize::riff_write(&all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, ImageReader};

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn with_segment(jpeg: &[u8], marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, marker];
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn dims(webp: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(webp, image::ImageFormat::WebP).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn sizes_keep_their_shape() {
        assert_eq!(fit(4000, 3000, 2048), (2048, 1536));
        assert_eq!(fit(3000, 4000, 2048), (1536, 2048));
        assert_eq!(fit(100, 50, 2048), (100, 50));
        assert_eq!(fit(100_000, 10, 2048), (2048, 1));
        assert_eq!(thumb_size(4032, 3024), (640, 480));
        assert_eq!(thumb_size(3024, 4032), (480, 640));
        assert_eq!(thumb_size(300, 200), (300, 200));
        // A panorama: the long side caps before the short side reaches 480.
        assert_eq!(thumb_size(16000, 2000), (1440, 180));
    }

    #[test]
    fn a_large_photo_gets_a_display_copy_and_a_thumbnail() {
        let p = process(&jpeg(3000, 2000), 100_000_000).unwrap();
        assert_eq!((p.width, p.height), (3000, 2000));
        assert!(!p.animated);
        assert_eq!(
            dims(p.display.as_ref().expect("a display copy")),
            (2048, 1365)
        );
        assert_eq!(dims(&p.thumb), (720, 480));
        let preview = image::load_from_memory(&p.preview).unwrap();
        assert_eq!((preview.width(), preview.height()), (1200, 800));
        assert_eq!(p.sha256, <[u8; 32]>::from(Sha256::digest(&p.original)));
    }

    #[test]
    fn a_small_picture_is_its_own_display_copy() {
        let p = process(&jpeg(800, 600), 100_000_000).unwrap();
        assert!(p.display.is_none());
        assert_eq!(dims(&p.thumb), (640, 480));
    }

    #[test]
    fn orientation_turns_the_reported_size_and_the_copies() {
        // EXIF orientation 6: stored landscape, shown portrait.
        let mut exif = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
        exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
        let input = with_segment(&jpeg(3000, 2000), 0xE1, &exif);
        let p = process(&input, 100_000_000).unwrap();
        assert_eq!((p.width, p.height), (2000, 3000));
        assert_eq!(dims(p.display.as_ref().unwrap()), (1365, 2048));
        assert_eq!(dims(&p.thumb), (480, 720));
    }

    #[test]
    fn an_rgb_profile_travels_into_the_copies_and_a_cmyk_one_does_not() {
        let mut rgb = vec![0u8; 128];
        rgb[16..20].copy_from_slice(b"RGB ");
        let mut payload = b"ICC_PROFILE\0\x01\x01".to_vec();
        payload.extend_from_slice(&rgb);
        let p = process(&with_segment(&jpeg(64, 64), 0xE2, &payload), 100_000_000).unwrap();
        let chunks = sanitize::riff_chunks(&p.thumb).unwrap();
        assert_eq!(chunks[0].0, *b"VP8X");
        assert_eq!(chunks[0].1[0] & 0x20, 0x20);
        assert_eq!(chunks[1], (*b"ICCP", &rgb[..]));
        let mut decoder = ImageReader::new(Cursor::new(&p.thumb))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), Some(rgb.clone()));

        let mut cmyk = rgb.clone();
        cmyk[16..20].copy_from_slice(b"CMYK");
        let mut payload = b"ICC_PROFILE\0\x01\x01".to_vec();
        payload.extend_from_slice(&cmyk);
        let p = process(&with_segment(&jpeg(64, 64), 0xE2, &payload), 100_000_000).unwrap();
        assert!(
            sanitize::riff_chunks(&p.thumb)
                .unwrap()
                .iter()
                .all(|(c, _)| c != b"ICCP")
        );
    }

    #[test]
    fn transparency_is_kept_and_an_unused_alpha_channel_dropped() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_fn(32, 32, |x, _| {
            image::Rgba([255, 0, 0, if x < 16 { 0 } else { 255 }])
        });
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 32, 32, image::ExtendedColorType::Rgba8)
            .unwrap();
        let p = process(&png, 100_000_000).unwrap();
        let thumb = image::load_from_memory(&p.thumb).unwrap();
        assert!(thumb.color().has_alpha());

        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(32, 32, image::Rgba([0, 0, 255, 255]));
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 32, 32, image::ExtendedColorType::Rgba8)
            .unwrap();
        let p = process(&png, 100_000_000).unwrap();
        assert!(
            !image::load_from_memory(&p.thumb)
                .unwrap()
                .color()
                .has_alpha()
        );
    }

    #[test]
    fn an_animated_gif_is_kept_whole_with_a_still_thumbnail() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for shade in [0u8, 255] {
                let frame = image::RgbaImage::from_pixel(2400, 40, image::Rgba([shade, 0, 0, 255]));
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        let p = process(&gif, 100_000_000).unwrap();
        assert!(p.animated);
        assert!(
            p.display.is_none(),
            "a display copy would stop the animation"
        );
        assert_eq!(dims(&p.thumb), (1440, 24));
        let still = image::load_from_memory(&p.preview).unwrap();
        assert_eq!((still.width(), still.height()), (1200, 20));
    }

    #[test]
    fn a_preview_is_a_small_jpeg_from_any_stored_copy() {
        assert_eq!(preview_size(3000, 2000, false), (1200, 800));
        assert_eq!(preview_size(800, 600, false), (800, 600));
        // Animated: from the thumbnail, a still no larger than it.
        assert_eq!(preview_size(2400, 40, true), (1200, 20));

        // A busy photo's display copy, which would not fit at full quality.
        let noisy = image::RgbImage::from_fn(3000, 2000, |x, y| {
            let n = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
            image::Rgb([n as u8, (n >> 8) as u8, (n >> 16) as u8])
        });
        let mut source = Vec::new();
        image::codecs::png::PngEncoder::new(&mut source)
            .write_image(noisy.as_raw(), 3000, 2000, image::ExtendedColorType::Rgb8)
            .unwrap();
        let p = process(&source, 100_000_000).unwrap();
        let jpeg = preview(p.display.as_ref().unwrap(), (1200, 800)).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
        assert!(jpeg.len() <= PREVIEW_BYTES, "{} bytes", jpeg.len());
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (1200, 800));
    }

    #[test]
    fn a_preview_lays_transparency_over_white_and_turns_upright() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 0, 0]));
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 40, 20, image::ExtendedColorType::Rgba8)
            .unwrap();
        let p = preview(&process(&png, 1_000_000).unwrap().original, (40, 20)).unwrap();
        let shown = image::load_from_memory(&p).unwrap().into_rgb8();
        assert!(shown.pixels().all(|px| px.0.iter().all(|&c| c > 250)));

        let mut exif = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
        exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
        let turned = process(&with_segment(&jpeg(300, 200), 0xE1, &exif), 1_000_000).unwrap();
        assert_eq!((turned.width, turned.height), (200, 300));
        let p = preview(&turned.original, (200, 300)).unwrap();
        let shown = image::load_from_memory(&p).unwrap();
        assert_eq!((shown.width(), shown.height()), (200, 300));
    }

    #[test]
    fn too_many_pixels_is_refused_before_decoding() {
        let err = process(&jpeg(2000, 1000), 1_000_000).err().unwrap();
        assert!(matches!(err, Reject::TooBig(_)), "{err}");
        assert!(matches!(
            cost(&jpeg(2000, 1000), 0, 1_000_000),
            Err(Reject::TooBig(_))
        ));
    }

    #[test]
    fn a_photo_is_sized_from_its_first_bytes() {
        let photo = jpeg(3000, 2000);
        let size = photo.len() as u64;
        assert_eq!(
            cost(&photo[..16 << 10], size, 100_000_000).unwrap(),
            cost(&photo, size, 100_000_000).unwrap()
        );
    }

    fn png(w: u32, h: u32, color: image::ExtendedColorType) -> Vec<u8> {
        let channels = match color {
            image::ExtendedColorType::Rgb8 => 3,
            image::ExtendedColorType::Rgba16 => 8,
            _ => unreachable!(),
        };
        let raw = vec![u8::MAX; (w * h) as usize * channels];
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&raw, w, h, color)
            .unwrap();
        out
    }

    #[test]
    fn sixteen_bits_cost_more_and_come_out_as_eight() {
        let deep = png(600, 400, image::ExtendedColorType::Rgba16);
        let shallow = png(600, 400, image::ExtendedColorType::Rgb8);
        // Eight bytes a pixel decoded where the eight-bit copy takes four; RGB
        // at three is held to the four its copy takes.
        assert_eq!(
            cost(&deep, 0, 100_000_000).unwrap() - cost(&shallow, 0, 100_000_000).unwrap(),
            4 * 600 * 400
        );
        let p = process(&deep, 100_000_000).unwrap();
        // Opaque, so the alpha channel is dropped too.
        assert_eq!(
            image::load_from_memory(&p.thumb).unwrap().color(),
            image::ColorType::Rgb8
        );
        assert_eq!((p.width, p.height), (600, 400));
    }

    #[test]
    fn garbage_is_refused_with_a_reason() {
        assert!(matches!(
            process(b"hello", 1_000_000),
            Err(Reject::Unsupported(_))
        ));
        let mut truncated = jpeg(64, 64);
        truncated.truncate(200);
        assert!(matches!(
            process(&truncated, 1_000_000),
            Err(Reject::Broken(_))
        ));
    }
}
