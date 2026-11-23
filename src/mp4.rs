//! An edit ffmpeg cannot make to the MP4s it writes: SEI messages out of an
//! HEVC track's decoder configuration (`hvcC`).
//!
//! `-bsf:v filter_units` drops SEI from the stream's frames as a remux copies
//! them, but not from the parameter sets the configuration record carries,
//! where an encoder (or a camera) can put SEI that applies to the whole
//! stream. This takes them out of the file ffmpeg wrote, in place: the movie
//! box shrinks by what was removed, and a `free` box of zeros takes up the
//! difference, so the media data and every offset into it stay where they
//! are and nothing removed survives in the gap.

use std::fs::OpenOptions;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

/// HEVC's prefix and suffix SEI NAL unit types.
const SEI: [u8; 2] = [39, 40];

/// The movie box is read whole; one larger than this is not one ffmpeg wrote
/// for a video dither takes.
const MOOV_MAX: u64 = 256 << 20;

fn malformed(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the MP4 {what}"))
}

/// A box's header: its type, and where its body starts and the box ends,
/// relative to the start of `data`.
fn header(data: &[u8], at: usize) -> io::Result<([u8; 4], usize, usize)> {
    let head = data
        .get(at..at + 8)
        .ok_or_else(|| malformed("ends inside a box"))?;
    let size = u64::from(u32::from_be_bytes(
        head[..4].try_into().expect("four bytes"),
    ));
    let kind: [u8; 4] = head[4..8].try_into().expect("four bytes");
    let (body, end) = match size {
        0 => (at + 8, data.len() as u64),
        1 => {
            let large = data
                .get(at + 8..at + 16)
                .ok_or_else(|| malformed("ends inside a box"))?;
            let large = u64::from_be_bytes(large.try_into().expect("eight bytes"));
            (at + 16, at as u64 + large)
        }
        n => (at + 8, at as u64 + n),
    };
    let end = usize::try_from(end).map_err(|_| malformed("has a box too large"))?;
    if end > data.len() || end < body {
        return Err(malformed("has a box of the wrong size"));
    }
    Ok((kind, body, end))
}

/// `kind` with `body`, its size in the header.
fn boxed(kind: [u8; 4], body: &[u8]) -> io::Result<Vec<u8>> {
    let size = u32::try_from(body.len() + 8).map_err(|_| malformed("box grew too large"))?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    Ok(out)
}

/// What comes before a box's children: nothing for a plain container, the
/// version and entry count of `stsd`, the fixed fields of a visual sample
/// entry.
fn preamble(kind: &[u8; 4]) -> Option<usize> {
    match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => Some(0),
        b"stsd" => Some(8),
        b"hvc1" | b"hev1" => Some(78),
        _ => None,
    }
}

/// The boxes in `data`, each rewritten: containers on the way to an `hvcC`
/// rebuilt from their rewritten children, an `hvcC` without its SEI, the
/// rest copied.
fn rewrite(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    while at < data.len() {
        let (kind, body, end) = header(data, at)?;
        if &kind == b"hvcC" {
            out.extend(boxed(kind, &without_sei(&data[body..end])?)?);
        } else if let Some(skip) = preamble(&kind) {
            let fixed = data
                .get(body..body + skip)
                .ok_or_else(|| malformed("ends inside a box"))?;
            let mut inner = fixed.to_vec();
            inner.extend(rewrite(&data[body + skip..end])?);
            out.extend(boxed(kind, &inner)?);
        } else {
            out.extend_from_slice(&data[at..end]);
        }
        at = end;
    }
    Ok(out)
}

/// An HEVC decoder configuration record without SEI: each array keeps its
/// NAL units but those whose own header says SEI, and an array left with
/// none goes.
fn without_sei(record: &[u8]) -> io::Result<Vec<u8>> {
    const FIXED: usize = 22;
    let short = || malformed("has a short hvcC");
    let count = *record.get(FIXED).ok_or_else(short)?;
    let mut arrays = Vec::new();
    let mut at = FIXED + 1;
    for _ in 0..count {
        let kind = *record.get(at).ok_or_else(short)?;
        let n = u16::from_be_bytes(
            record
                .get(at + 1..at + 3)
                .ok_or_else(short)?
                .try_into()
                .expect("two bytes"),
        );
        at += 3;
        let mut kept = Vec::new();
        for _ in 0..n {
            let len = usize::from(u16::from_be_bytes(
                record
                    .get(at..at + 2)
                    .ok_or_else(short)?
                    .try_into()
                    .expect("two bytes"),
            ));
            let unit = record.get(at + 2..at + 2 + len).ok_or_else(short)?;
            at += 2 + len;
            let sei = unit
                .first()
                .is_some_and(|h| SEI.contains(&((h >> 1) & 0x3F)));
            if !sei {
                kept.push(unit);
            }
        }
        if !kept.is_empty() && !SEI.contains(&(kind & 0x3F)) {
            arrays.push((kind, kept));
        }
    }
    let mut out = record[..FIXED].to_vec();
    out.push(u8::try_from(arrays.len()).expect("no more arrays than there were"));
    for (kind, units) in arrays {
        out.push(kind);
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for unit in units {
            out.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            out.extend_from_slice(unit);
        }
    }
    // Whatever followed the arrays (nothing, in a well-formed record) goes too.
    Ok(out)
}

/// Remove SEI from the HEVC decoder configuration of the MP4 at `path`, in
/// place. True when there was any.
pub fn strip_sei(path: &Path) -> io::Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    // The movie box, by walking the top-level boxes' headers.
    let mut at = 0u64;
    let (start, size) = loop {
        let mut head = Vec::with_capacity(16);
        file.seek(SeekFrom::Start(at))?;
        (&mut file).take(16).read_to_end(&mut head)?;
        if head.len() < 8 {
            return Err(malformed("has no movie box"));
        }
        let size = match u32::from_be_bytes(head[..4].try_into().expect("four bytes")) {
            0 => len - at,
            1 => u64::from_be_bytes(
                head.get(8..16)
                    .ok_or_else(|| malformed("ends inside a box"))?
                    .try_into()
                    .expect("eight bytes"),
            ),
            n => u64::from(n),
        };
        if size < 8 || at.saturating_add(size) > len {
            return Err(malformed("has a box of the wrong size"));
        }
        if &head[4..8] == b"moov" {
            break (at, size);
        }
        at += size;
    };
    if size > MOOV_MAX {
        return Err(malformed("has a movie box too large"));
    }
    let mut moov = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut moov)?;
    let rewritten = rewrite(&moov)?;
    if rewritten == moov {
        return Ok(false);
    }
    let gap = moov
        .len()
        .checked_sub(rewritten.len())
        .ok_or_else(|| malformed("movie box grew"))?;
    if gap < 8 {
        return Err(malformed("movie box shrank by less than a box"));
    }
    let mut region = rewritten;
    region.extend_from_slice(&(gap as u32).to_be_bytes());
    region.extend_from_slice(b"free");
    region.resize(moov.len(), 0);
    file.seek(SeekFrom::Start(start))?;
    file.write_all(&region)?;
    file.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An HEVC decoder configuration record of `units`, an array each.
    fn record(units: &[&[u8]]) -> Vec<u8> {
        let mut r = vec![1; 22];
        r.push(units.len() as u8);
        for unit in units {
            r.push(0x80 | ((unit[0] >> 1) & 0x3F));
            r.extend_from_slice(&1u16.to_be_bytes());
            r.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            r.extend_from_slice(unit);
        }
        r
    }

    const VPS: &[u8] = &[32 << 1, 1, 0xAA];
    const SPS: &[u8] = &[33 << 1, 1, 0xBB];
    const SEI_PREFIX: &[u8] = &[39 << 1, 1, 5, 3, b'c', b'a', b'm'];

    #[test]
    fn a_record_keeps_its_parameter_sets_and_loses_its_sei() {
        let stripped = without_sei(&record(&[VPS, SPS, SEI_PREFIX])).unwrap();
        assert_eq!(stripped, record(&[VPS, SPS]));
        assert_eq!(
            without_sei(&record(&[VPS, SPS])).unwrap(),
            record(&[VPS, SPS])
        );
        assert!(without_sei(&[1; 10]).is_err());
    }

    /// A file of `ftyp`, a movie box holding a track whose sample entry has
    /// `record` as its `hvcC`, and the media data after it.
    fn mp4(record: &[u8]) -> Vec<u8> {
        let hvcc = boxed(*b"hvcC", record).unwrap();
        let mut entry = vec![0u8; 78];
        entry.extend(hvcc);
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(boxed(*b"hvc1", &entry).unwrap());
        let stbl = boxed(*b"stbl", &boxed(*b"stsd", &stsd).unwrap()).unwrap();
        let minf = boxed(*b"minf", &stbl).unwrap();
        let mdia = boxed(*b"mdia", &minf).unwrap();
        let trak = boxed(*b"trak", &mdia).unwrap();
        let moov = boxed(*b"moov", &trak).unwrap();
        let mut file = boxed(*b"ftyp", b"isom").unwrap();
        file.extend(moov);
        file.extend(boxed(*b"mdat", b"the pictures").unwrap());
        file
    }

    #[test]
    fn the_file_loses_its_sei_in_place_and_the_media_stays_put() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        let before = mp4(&record(&[VPS, SPS, SEI_PREFIX]));
        std::fs::write(&path, &before).unwrap();
        assert!(strip_sei(&path).unwrap());
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len(), before.len());
        assert!(!after.windows(3).any(|w| w == b"cam"), "the SEI survived");
        let mdat = |f: &[u8]| f.windows(4).position(|w| w == b"mdat").unwrap();
        assert_eq!(mdat(&after), mdat(&before), "the media data moved");
        // What it now is: the record without SEI, then the free box.
        let expected = {
            let mut f = mp4(&record(&[VPS, SPS]));
            let at = mdat(&f) - 4;
            let gap = before.len() - f.len();
            let mut free = (gap as u32).to_be_bytes().to_vec();
            free.extend_from_slice(b"free");
            free.resize(gap, 0);
            f.splice(at..at, free);
            f
        };
        assert_eq!(after, expected);
        // Again, there is nothing to take.
        assert!(!strip_sei(&path).unwrap());
    }

    #[test]
    fn a_file_without_hevc_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        let mut file = boxed(*b"ftyp", b"isom").unwrap();
        file.extend(boxed(*b"moov", &boxed(*b"mvhd", &[0; 100]).unwrap()).unwrap());
        std::fs::write(&path, &file).unwrap();
        assert!(!strip_sei(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), file);
        std::fs::write(&path, b"not an mp4").unwrap();
        assert!(strip_sei(&path).is_err());
    }
}
