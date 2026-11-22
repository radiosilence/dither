//! Metadata stripped from uploads before anything is stored.
//!
//! A photo straight off a phone carries where it was taken and on what device,
//! and often a second image after the first (a depth map or an HDR gain map)
//! with metadata of its own. None of that is served. Each format is parsed down
//! to its container and rebuilt from the parts that draw the picture: the image
//! data, its colour profile, animation control, and the orientation, which is
//! written back as an EXIF block holding that one tag so viewers still turn the
//! picture the right way up.
//!
//! Working on the container rather than re-encoding keeps the stored file the
//! uploaded image bit for bit, less its metadata: no generation loss, and
//! animations survive. Whatever a parser does not recognise is dropped, so a
//! new kind of metadata is removed by default rather than kept by default.

use std::fmt;

/// The formats dither stores. Anything else is refused at the door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl Kind {
    pub fn mime(self) -> &'static str {
        match self {
            Kind::Jpeg => "image/jpeg",
            Kind::Png => "image/png",
            Kind::Gif => "image/gif",
            Kind::Webp => "image/webp",
        }
    }

    /// The extension in an image's address. Reddit and most chat clients decide
    /// whether a link is an image by it.
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpg",
            Kind::Png => "png",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
        }
    }

    /// How the database stores it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpeg",
            Kind::Png => "png",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "jpeg" => Kind::Jpeg,
            "png" => Kind::Png,
            "gif" => Kind::Gif,
            "webp" => Kind::Webp,
            _ => return None,
        })
    }

    /// Whether `ext` names this format in an address (`.jpg` and `.jpeg` both do).
    pub fn matches_ext(self, ext: &str) -> bool {
        let ext = ext.to_ascii_lowercase();
        ext == self.ext() || (self == Kind::Jpeg && ext == "jpeg")
    }

    pub fn image_format(self) -> image::ImageFormat {
        match self {
            Kind::Jpeg => image::ImageFormat::Jpeg,
            Kind::Png => image::ImageFormat::Png,
            Kind::Gif => image::ImageFormat::Gif,
            Kind::Webp => image::ImageFormat::WebP,
        }
    }
}

/// What the bytes are, from their signature alone: the name a file arrives
/// with and the type its browser claims are never trusted. A refusal says why
/// in words a person can act on.
pub fn sniff(bytes: &[u8]) -> Result<Kind, &'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok(Kind::Jpeg);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(Kind::Png);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok(Kind::Gif);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Ok(Kind::Webp);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Err(match &bytes[8..12] {
            b"avif" | b"avis" => "AVIF isn't supported yet: export it as JPEG or PNG",
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                "HEIC isn't supported yet: export it as JPEG (on an iPhone, sharing from Photos does this)"
            }
            _ => "that looks like a video, and dither only hosts images",
        });
    }
    let text = bytes
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(bytes)
        .trim_ascii_start();
    if text.starts_with(b"<") {
        return Err("SVG isn't accepted: an SVG can carry scripts");
    }
    if bytes.starts_with(b"BM") || bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Err("BMP and TIFF aren't supported: convert it to PNG or JPEG");
    }
    Err("that isn't an image dither can host: JPEG, PNG, GIF and WebP are")
}

/// A container that could not be parsed. Browsers forgive a great deal; this
/// does not, since a file it cannot take apart is a file it cannot clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Malformed {}

#[derive(Debug)]
pub struct Clean {
    pub bytes: Vec<u8>,
    /// More than one frame: a GIF, an animated WebP or an APNG.
    pub animated: bool,
}

/// `bytes` rebuilt without its metadata. `orientation` is the EXIF value (1
/// to 8) the original carried; anything but 1 is written back on its own.
pub fn clean(kind: Kind, bytes: &[u8], orientation: u8) -> Result<Clean, Malformed> {
    match kind {
        Kind::Jpeg => jpeg(bytes, orientation).map(|bytes| Clean {
            bytes,
            animated: false,
        }),
        Kind::Png => png(bytes, orientation),
        Kind::Gif => gif(bytes),
        Kind::Webp => webp(bytes, orientation),
    }
}

/// A big-endian TIFF structure holding one tag, Orientation, in IFD0: what an
/// EXIF block shrinks to once everything else is gone.
fn tiff_orientation(orientation: u8) -> [u8; 26] {
    let mut t = [0u8; 26];
    t[0..4].copy_from_slice(b"MM\0*");
    t[4..8].copy_from_slice(&8u32.to_be_bytes());
    t[8..10].copy_from_slice(&1u16.to_be_bytes());
    t[10..12].copy_from_slice(&0x0112u16.to_be_bytes());
    t[12..14].copy_from_slice(&3u16.to_be_bytes());
    t[14..18].copy_from_slice(&1u32.to_be_bytes());
    t[18..20].copy_from_slice(&u16::from(orientation).to_be_bytes());
    // t[20..22] pads the value; t[22..26] says there is no next IFD.
    t
}

fn take(input: &[u8], at: usize, len: usize) -> Result<&[u8], Malformed> {
    input
        .get(at..at.checked_add(len).ok_or(Malformed("a length overflows"))?)
        .ok_or(Malformed("the file ends early"))
}

fn jpeg(input: &[u8], orientation: u8) -> Result<Vec<u8>, Malformed> {
    if !input.starts_with(&[0xFF, 0xD8]) {
        return Err(Malformed("not a JPEG"));
    }
    let mut jfif: Option<Vec<u8>> = None;
    let mut body = Vec::with_capacity(input.len());
    let mut i = 2;
    loop {
        if input.get(i) != Some(&0xFF) {
            return Err(Malformed("a JPEG marker is missing"));
        }
        while input.get(i) == Some(&0xFF) {
            i += 1;
        }
        let marker = *input.get(i).ok_or(Malformed("the file ends early"))?;
        i += 1;
        match marker {
            // End of image: anything after it (a second picture, a trailer
            // some apps append) goes with the metadata.
            0xD9 => {
                body.extend_from_slice(&[0xFF, 0xD9]);
                break;
            }
            0xD8 => return Err(Malformed("a second JPEG starts inside the first")),
            0x01 | 0xD0..=0xD7 => {
                body.extend_from_slice(&[0xFF, marker]);
                continue;
            }
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes(
            take(input, i, 2)?.try_into().expect("two bytes"),
        ));
        if len < 2 {
            return Err(Malformed("a JPEG segment has a bad length"));
        }
        let segment = take(input, i, len)?;
        let payload = &segment[2..];
        i += len;
        let keep = match marker {
            // JFIF's density is kept and its thumbnail, a second copy of the
            // picture, is not.
            0xE0 => {
                if jfif.is_none() && payload.starts_with(b"JFIF\0") && payload.len() >= 12 {
                    let mut seg = vec![0xFF, 0xE0, 0x00, 0x10];
                    seg.extend_from_slice(&payload[..12]);
                    seg.extend_from_slice(&[0, 0]);
                    jfif = Some(seg);
                }
                false
            }
            // ICC profiles keep the colours right; Adobe's segment says how
            // the colour channels are encoded, and decoders need it.
            0xE2 => payload.starts_with(b"ICC_PROFILE\0"),
            0xEE => payload.starts_with(b"Adobe"),
            // What decoding needs: frame headers (every SOFn; 0xC4, 0xC8
            // and 0xCC are not frames), Huffman and arithmetic tables,
            // quantisation tables, scans, restart interval, line count and
            // the hierarchical markers. Nothing else, so EXIF, XMP, IPTC,
            // comments, other APPn and reserved markers all go, as does
            // whatever a future scheme puts in one.
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => true,
            0xC4 | 0xCC | 0xDB | 0xDA | 0xDD | 0xDC | 0xDE | 0xDF => true,
            _ => false,
        };
        if keep {
            body.extend_from_slice(&[0xFF, marker]);
            body.extend_from_slice(segment);
        }
        if marker == 0xDA {
            // Entropy-coded data runs to the next marker that is neither a
            // stuffed zero nor a restart.
            let start = i;
            loop {
                let at = input[i..]
                    .iter()
                    .position(|&b| b == 0xFF)
                    .map(|p| i + p)
                    .ok_or(Malformed("the JPEG ends inside its image data"))?;
                match input.get(at + 1) {
                    Some(0x00 | 0xD0..=0xD7) => i = at + 2,
                    Some(0xFF) => i = at + 1,
                    Some(_) => {
                        body.extend_from_slice(&input[start..at]);
                        i = at;
                        break;
                    }
                    None => return Err(Malformed("the JPEG ends inside its image data")),
                }
            }
        }
    }
    let mut out = Vec::with_capacity(body.len() + 64);
    out.extend_from_slice(&[0xFF, 0xD8]);
    if let Some(seg) = jfif {
        out.extend_from_slice(&seg);
    }
    if orientation != 1 {
        out.extend_from_slice(&[0xFF, 0xE1, 0x00, 34]);
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff_orientation(orientation));
    }
    out.extend_from_slice(&body);
    Ok(out)
}

/// Chunks that draw the picture or say how to: everything else (text, XMP,
/// EXIF, timestamps, private chunks) is dropped.
const PNG_KEEP: &[&[u8; 4]] = &[
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB",
    b"cICP", b"mDCV", b"cLLI", b"bKGD", b"hIST", b"pHYs", b"acTL", b"fcTL", b"fdAT",
];

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
    out
}

fn png(input: &[u8], orientation: u8) -> Result<Clean, Malformed> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !input.starts_with(SIGNATURE) {
        return Err(Malformed("not a PNG"));
    }
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(SIGNATURE);
    let mut animated = false;
    let mut i = SIGNATURE.len();
    loop {
        let len = u32::from_be_bytes(take(input, i, 4)?.try_into().expect("four bytes")) as usize;
        let kind: [u8; 4] = take(input, i + 4, 4)?.try_into().expect("four bytes");
        let whole = take(
            input,
            i,
            len.checked_add(12).ok_or(Malformed("a length overflows"))?,
        )?;
        let data = &whole[8..8 + len];
        i += whole.len();
        if PNG_KEEP.contains(&&kind) {
            out.extend_from_slice(whole);
        }
        match &kind {
            b"IHDR" if orientation != 1 => {
                out.extend_from_slice(&png_chunk(b"eXIf", &tiff_orientation(orientation)));
            }
            b"acTL" if data.len() >= 4 => {
                animated = u32::from_be_bytes(data[..4].try_into().expect("four bytes")) > 1;
            }
            b"IEND" => break,
            _ => {}
        }
    }
    Ok(Clean {
        bytes: out,
        animated,
    })
}

/// The end of a run of GIF sub-blocks starting at `i`.
fn gif_blocks_end(input: &[u8], mut i: usize) -> Result<usize, Malformed> {
    loop {
        let size = usize::from(*input.get(i).ok_or(Malformed("the GIF ends early"))?);
        i += 1;
        if size == 0 {
            return Ok(i);
        }
        i += size;
        if i > input.len() {
            return Err(Malformed("the GIF ends early"));
        }
    }
}

fn gif(input: &[u8]) -> Result<Clean, Malformed> {
    let screen = take(input, 0, 13)?;
    let packed = screen[10];
    let palette = if packed & 0x80 != 0 {
        3 << ((packed & 7) + 1)
    } else {
        0
    };
    let mut i = 13 + palette;
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(take(input, 0, i)?);
    let mut frames = 0u32;
    loop {
        match input.get(i) {
            Some(0x2C) => {
                let descriptor = take(input, i, 10)?;
                let packed = descriptor[9];
                let palette = if packed & 0x80 != 0 {
                    3 << ((packed & 7) + 1)
                } else {
                    0
                };
                // The descriptor, its palette and the LZW code size, then data.
                let data = i + 10 + palette + 1;
                take(input, i, data - i)?;
                let end = gif_blocks_end(input, data)?;
                out.extend_from_slice(&input[i..end]);
                frames += 1;
                i = end;
            }
            Some(0x21) => {
                let label = *input.get(i + 1).ok_or(Malformed("the GIF ends early"))?;
                let end = gif_blocks_end(input, i + 2)?;
                let keep = match label {
                    // Frame timing and transparency, and plain text, which is
                    // drawn as part of the picture.
                    0xF9 | 0x01 => true,
                    // Looping is an application extension; so are XMP and
                    // whatever an editor stored, which are not kept.
                    0xFF => matches!(
                        input.get(i + 3..i + 14),
                        Some(b"NETSCAPE2.0" | b"ANIMEXTS1.0")
                    ),
                    _ => false,
                };
                if keep {
                    out.extend_from_slice(&input[i..end]);
                }
                i = end;
            }
            // The trailer, or the end of a file that left it off, which
            // browsers draw anyway.
            Some(0x3B) | None if frames > 0 => {
                out.push(0x3B);
                break;
            }
            _ => return Err(Malformed("the GIF has an unknown block")),
        }
    }
    Ok(Clean {
        bytes: out,
        animated: frames > 1,
    })
}

/// A RIFF chunk: its four-character code and its payload.
pub(crate) type Chunk<'a> = ([u8; 4], &'a [u8]);

/// A WebP's chunks, in order.
pub(crate) fn riff_chunks(input: &[u8]) -> Result<Vec<Chunk<'_>>, Malformed> {
    if input.len() < 12 || &input[..4] != b"RIFF" || &input[8..12] != b"WEBP" {
        return Err(Malformed("not a WebP"));
    }
    let declared = u32::from_le_bytes(input[4..8].try_into().expect("four bytes")) as usize;
    let end = input.len().min(declared.saturating_add(8));
    let mut chunks = Vec::new();
    let mut i = 12;
    while i + 8 <= end {
        let fourcc: [u8; 4] = input[i..i + 4].try_into().expect("four bytes");
        let len = u32::from_le_bytes(input[i + 4..i + 8].try_into().expect("four bytes")) as usize;
        let data = take(input, i + 8, len)?;
        chunks.push((fourcc, data));
        i += 8 + len + (len & 1);
    }
    if chunks.is_empty() {
        return Err(Malformed("the WebP has no image"));
    }
    Ok(chunks)
}

/// A WebP file of `chunks`, in order.
pub(crate) fn riff_write(chunks: &[Chunk<'_>]) -> Vec<u8> {
    let size: usize = chunks
        .iter()
        .map(|(_, d)| 8 + d.len() + (d.len() & 1))
        .sum();
    let mut out = Vec::with_capacity(size + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((size + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for (fourcc, data) in chunks {
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() & 1 == 1 {
            out.push(0);
        }
    }
    out
}

fn webp(input: &[u8], orientation: u8) -> Result<Clean, Malformed> {
    const KEEP: &[&[u8; 4]] = &[
        b"VP8 ", b"VP8L", b"VP8X", b"ALPH", b"ANIM", b"ANMF", b"ICCP",
    ];
    let chunks = riff_chunks(input)?;
    let mut animated = false;
    let mut header: Option<Vec<u8>> = None;
    let mut kept: Vec<Chunk<'_>> = Vec::new();
    for (fourcc, data) in &chunks {
        if !KEEP.contains(&fourcc) {
            continue;
        }
        if fourcc == b"VP8X" {
            let mut x = data.to_vec();
            if let Some(flags) = x.first_mut() {
                animated = *flags & 0x02 != 0;
                // EXIF and XMP are gone; EXIF comes back below if it carried
                // an orientation.
                *flags &= !0x0C;
                if orientation != 1 {
                    *flags |= 0x08;
                }
            }
            header = Some(x);
            continue;
        }
        kept.push((*fourcc, data));
    }
    let tiff = tiff_orientation(orientation);
    let mut all: Vec<Chunk<'_>> = Vec::with_capacity(kept.len() + 2);
    if let Some(h) = &header {
        all.push((*b"VP8X", h));
    }
    all.extend(kept);
    // Only the extended format can say it carries EXIF.
    if header.is_some() && orientation != 1 {
        all.push((*b"EXIF", &tiff));
    }
    Ok(Clean {
        bytes: riff_write(&all),
        animated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageDecoder, ImageEncoder, ImageReader};
    use std::io::Cursor;

    fn decode(bytes: &[u8]) -> image::DynamicImage {
        image::load_from_memory(bytes).expect("decodes")
    }

    fn orientation(bytes: &[u8]) -> image::metadata::Orientation {
        let mut decoder = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        decoder.orientation().unwrap()
    }

    fn gradient(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 16) as u8, (y * 16) as u8, 128])
        })
    }

    /// EXIF in the shape a phone writes it: little-endian, IFD0 holding the
    /// orientation and a description standing in for GPS coordinates.
    fn phone_exif(orientation: u16, secret: &str) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"II*\0");
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        // ImageDescription, ASCII, pointing past the IFD.
        let text_at = 8 + 2 + 2 * 12 + 4;
        t.extend_from_slice(&0x010Eu16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&((secret.len() + 1) as u32).to_le_bytes());
        t.extend_from_slice(&(text_at as u32).to_le_bytes());
        t.extend_from_slice(&0x0112u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&orientation.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(secret.as_bytes());
        t.push(0);
        t
    }

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn plain_jpeg() -> Vec<u8> {
        let mut jpeg = Vec::new();
        let img = gradient(16, 8);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .write_image(img.as_raw(), 16, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    /// A JPEG dressed as a phone's: EXIF with a location and the orientation,
    /// XMP, IPTC, a comment, an ICC profile, and a second image after the end.
    fn phone_jpeg(orientation: u16) -> Vec<u8> {
        let plain = plain_jpeg();
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend(phone_exif(orientation, "GPS 51.5072N 0.1276W secret"));
        let mut out = vec![0xFF, 0xD8];
        out.extend(segment(0xE1, &exif));
        out.extend(segment(
            0xE1,
            b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>secret</x:xmpmeta>",
        ));
        out.extend(segment(0xED, b"Photoshop 3.0\0secret IPTC"));
        out.extend(segment(0xFE, b"a secret comment"));
        out.extend(segment(0xE2, b"MPF\0secret multi-picture index"));
        out.extend(segment(0xE2, b"ICC_PROFILE\0\x01\x01not really a profile"));
        // The encoder's own segments, after its SOI.
        out.extend_from_slice(&plain[2..]);
        out.extend_from_slice(b"\xFF\xD8secret second image\xFF\xD9");
        out
    }

    #[test]
    fn sniffs_by_signature_and_says_why_it_refuses() {
        assert_eq!(sniff(&plain_jpeg()), Ok(Kind::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Ok(Kind::Png));
        assert_eq!(sniff(b"GIF89a......"), Ok(Kind::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Ok(Kind::Webp));
        assert!(
            sniff(b"\0\0\0\x18ftypheic\0\0\0\0")
                .unwrap_err()
                .contains("HEIC")
        );
        assert!(
            sniff(b"\0\0\0\x18ftypavif\0\0\0\0")
                .unwrap_err()
                .contains("AVIF")
        );
        assert!(
            sniff(b"\0\0\0\x18ftypisom\0\0\0\0")
                .unwrap_err()
                .contains("video")
        );
        assert!(
            sniff(b"  <svg onload=alert(1)>")
                .unwrap_err()
                .contains("SVG")
        );
        assert!(sniff(b"\xEF\xBB\xBF<?xml").unwrap_err().contains("SVG"));
        assert!(sniff(b"BM....").is_err());
        assert!(sniff(b"").is_err());
    }

    #[test]
    fn a_jpeg_keeps_its_picture_profile_and_orientation_and_nothing_else() {
        let input = phone_jpeg(6);
        let cleaned = clean(Kind::Jpeg, &input, 6).unwrap().bytes;
        for leak in [&b"secret"[..], b"GPS", b"xmpmeta", b"Photoshop", b"MPF"] {
            assert!(
                !contains(&cleaned, leak),
                "{} survived",
                String::from_utf8_lossy(leak)
            );
        }
        assert!(contains(&cleaned, b"ICC_PROFILE\0"));
        assert_eq!(
            orientation(&cleaned),
            image::metadata::Orientation::Rotate90
        );
        assert_eq!(decode(&cleaned).to_rgb8(), decode(&plain_jpeg()).to_rgb8());
        assert!(cleaned.ends_with(&[0xFF, 0xD9]));
    }

    #[test]
    fn a_jpeg_segment_nobody_recognises_is_dropped() {
        let plain = plain_jpeg();
        let mut input = vec![0xFF, 0xD8];
        // JPG7 (reserved for extensions) and RES 0x30, both before the frame.
        input.extend(segment(0xF7, b"secret in an extension marker"));
        input.extend(segment(0x30, b"secret in a reserved marker"));
        input.extend_from_slice(&plain[2..]);
        let cleaned = clean(Kind::Jpeg, &input, 1).unwrap().bytes;
        assert!(!contains(&cleaned, b"secret"));
        assert_eq!(decode(&cleaned).to_rgb8(), decode(&plain).to_rgb8());
    }

    #[test]
    fn an_upright_jpeg_gets_no_exif_at_all() {
        let cleaned = clean(Kind::Jpeg, &phone_jpeg(1), 1).unwrap().bytes;
        assert!(!contains(&cleaned, b"Exif"));
        assert_eq!(
            orientation(&cleaned),
            image::metadata::Orientation::NoTransforms
        );
    }

    #[test]
    fn a_jfif_thumbnail_is_dropped_and_its_density_kept() {
        let plain = plain_jpeg();
        let mut jfif = b"JFIF\0\x01\x02\x01\x00\x48\x00\x48\x02\x02".to_vec();
        jfif.extend_from_slice(&[0xAA; 12]);
        let mut input = vec![0xFF, 0xD8];
        input.extend(segment(0xE0, &jfif));
        input.extend_from_slice(&plain[2..]);
        let cleaned = clean(Kind::Jpeg, &input, 1).unwrap().bytes;
        assert!(!contains(&cleaned, &[0xAA; 12]));
        assert!(contains(
            &cleaned,
            b"JFIF\0\x01\x02\x01\x00\x48\x00\x48\x00\x00"
        ));
        decode(&cleaned);
    }

    #[test]
    fn fill_bytes_before_a_marker_are_tolerated() {
        let plain = plain_jpeg();
        let mut input = vec![0xFF, 0xD8, 0xFF, 0xFF];
        input.extend_from_slice(&plain[2..]);
        let cleaned = clean(Kind::Jpeg, &input, 1).unwrap().bytes;
        assert_eq!(decode(&cleaned).to_rgb8(), decode(&plain).to_rgb8());
    }

    #[test]
    fn a_truncated_jpeg_is_refused() {
        let plain = plain_jpeg();
        assert!(clean(Kind::Jpeg, &plain[..plain.len() / 2], 1).is_err());
        assert!(clean(Kind::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE1, 0x00], 1).is_err());
        assert!(clean(Kind::Jpeg, &[0xFF, 0xD8, 0x00], 1).is_err());
    }

    fn plain_png() -> Vec<u8> {
        let mut png = Vec::new();
        let img = gradient(16, 8);
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 16, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        png
    }

    #[test]
    fn a_png_loses_its_text_and_exif_and_keeps_its_orientation() {
        let plain = plain_png();
        // After IHDR: the signature is 8 bytes and IHDR's chunk 25.
        let mut input = plain[..33].to_vec();
        input.extend(png_chunk(b"tEXt", b"Comment\0secret"));
        input.extend(png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0secret"));
        input.extend(png_chunk(b"eXIf", &phone_exif(3, "secret location")));
        input.extend(png_chunk(b"tIME", &[7, 234, 11, 22, 12, 0, 0]));
        input.extend(png_chunk(b"prVt", b"secret private chunk"));
        input.extend_from_slice(&plain[33..]);
        input.extend_from_slice(b"secret trailer");
        let cleaned = clean(Kind::Png, &input, 3).unwrap();
        assert!(!contains(&cleaned.bytes, b"secret"));
        assert!(!contains(&cleaned.bytes, b"tIME"));
        assert!(!cleaned.animated);
        assert_eq!(
            orientation(&cleaned.bytes),
            image::metadata::Orientation::Rotate180
        );
        assert_eq!(decode(&cleaned.bytes).to_rgb8(), decode(&plain).to_rgb8());
    }

    #[test]
    fn a_png_with_a_bad_chunk_length_is_refused() {
        let mut input = plain_png();
        input[33..37].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(clean(Kind::Png, &input, 1).is_err());
    }

    fn two_frame_gif() -> Vec<u8> {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for shade in [0u8, 255] {
                let frame = image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 0, 0, 255]));
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        gif
    }

    #[test]
    fn a_gif_keeps_its_frames_and_loop_and_drops_comments_and_xmp() {
        let plain = two_frame_gif();
        // Extensions go straight after the screen descriptor and palette.
        let packed = plain[10];
        let palette = if packed & 0x80 != 0 {
            3 << ((packed & 7) + 1)
        } else {
            0
        };
        let at = 13 + palette;
        let mut input = plain[..at].to_vec();
        input.extend_from_slice(b"\x21\xFE\x0Da secret note\0");
        input.extend_from_slice(b"\x21\xFF\x0BXMP DataXMP\x06secret\0");
        input.extend_from_slice(&plain[at..]);
        let cleaned = clean(Kind::Gif, &input, 1).unwrap();
        assert!(!contains(&cleaned.bytes, b"secret"));
        assert!(contains(&cleaned.bytes, b"NETSCAPE2.0"));
        assert!(cleaned.animated);
        decode(&cleaned.bytes);
    }

    #[test]
    fn a_gif_missing_its_trailer_is_finished_for_it() {
        let mut input = two_frame_gif();
        assert_eq!(input.pop(), Some(0x3B));
        let cleaned = clean(Kind::Gif, &input, 1).unwrap();
        assert_eq!(cleaned.bytes.last(), Some(&0x3B));
        decode(&cleaned.bytes);
    }

    fn lossy_webp() -> Vec<u8> {
        let img = gradient(16, 8);
        webp::Encoder::from_rgb(img.as_raw(), 16, 8)
            .encode(80.0)
            .to_vec()
    }

    #[test]
    fn a_webp_loses_exif_and_xmp_and_its_flags_say_so() {
        let plain = lossy_webp();
        let chunks = riff_chunks(&plain).unwrap();
        let mut vp8x = vec![0x0C, 0, 0, 0];
        vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
        vp8x.extend_from_slice(&7u32.to_le_bytes()[..3]);
        let exif = phone_exif(1, "secret location");
        let mut all: Vec<([u8; 4], &[u8])> = vec![(*b"VP8X", &vp8x)];
        all.extend(chunks.iter().copied());
        all.push((*b"EXIF", &exif));
        all.push((*b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"));
        let input = riff_write(&all);

        let cleaned = clean(Kind::Webp, &input, 1).unwrap();
        assert!(!contains(&cleaned.bytes, b"secret"));
        let out = riff_chunks(&cleaned.bytes).unwrap();
        assert_eq!(out[0].0, *b"VP8X");
        assert_eq!(out[0].1[0] & 0x0C, 0, "EXIF and XMP flags cleared");
        assert!(!cleaned.animated);
        assert_eq!(decode(&cleaned.bytes).to_rgb8(), decode(&plain).to_rgb8());
    }

    #[test]
    fn a_webp_keeps_an_orientation_as_its_only_exif() {
        let plain = lossy_webp();
        let chunks = riff_chunks(&plain).unwrap();
        let mut vp8x = vec![0x08, 0, 0, 0];
        vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
        vp8x.extend_from_slice(&7u32.to_le_bytes()[..3]);
        let exif = phone_exif(8, "secret location");
        let mut all: Vec<([u8; 4], &[u8])> = vec![(*b"VP8X", &vp8x)];
        all.extend(chunks.iter().copied());
        all.push((*b"EXIF", &exif));
        let cleaned = clean(Kind::Webp, &riff_write(&all), 8).unwrap();
        assert!(!contains(&cleaned.bytes, b"secret"));
        assert_eq!(
            orientation(&cleaned.bytes),
            image::metadata::Orientation::Rotate270
        );
    }

    #[test]
    fn the_minimal_exif_reads_back_as_every_orientation() {
        for o in 1..=8u8 {
            assert_eq!(
                image::metadata::Orientation::from_exif_chunk(&tiff_orientation(o))
                    .map(|o| o.to_exif()),
                Some(o)
            );
        }
    }
}
