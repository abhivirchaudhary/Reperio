//! Thumbnails and previews of recoverable files, so people can see what
//! they are getting back before they recover it.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader};
use serde::{Deserialize, Serialize};

use crate::extent::MappedSource;
use crate::formats::tiff;
use crate::reader::Reader;
use crate::session::Item;
use crate::source::Source;

/// Largest file read whole to make a preview.
const MAX_PREVIEW_INPUT: u64 = 96 << 20;

/// Largest embedded JPEG read out of a RAW file. Real previews are at most
/// a few tens of megabytes; a length beyond this is corrupt.
const MAX_EMBEDDED_JPEG: u64 = 64 << 20;

/// How far into a RAW file to search byte by byte for previews its
/// structure does not point to (Olympus keeps them in its maker notes, CR3
/// in a box of its own). They sit near the start, before the sensor data.
const RAW_SEARCH: u64 = 16 << 20;

/// Formats the `image` crate decodes directly.
fn decodable(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "jpe" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff"
    )
}

/// TIFF-based camera RAW formats, which embed JPEG previews.
fn tiff_raw(ext: &str) -> bool {
    matches!(
        ext,
        "cr2" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "dng" | "orf" | "rw2" | "pef"
    )
}

/// Every camera RAW format whose embedded preview [`raw_preview`] finds.
pub fn raw(ext: &str) -> bool {
    tiff_raw(ext) || matches!(ext, "cr3" | "raf")
}

/// Whether [`thumbnail`] can produce an image for this item. Others (HEIC,
/// videos) are left to the platform's own decoders.
pub fn supported(item: &Item) -> bool {
    decodable(&item.ext) || raw(&item.ext)
}

/// A JPEG no larger than `max` pixels on its longest side, upright.
pub fn thumbnail(src: &dyn Source, item: &Item, max: u32) -> Option<Vec<u8>> {
    thumbnail_with_signature(src, item, max).map(|(jpeg, _)| jpeg)
}

/// [`thumbnail`], and the photo's [`Signature`] from the same decode, so
/// grouping similar photos later costs nothing for photos already shown.
pub fn thumbnail_with_signature(
    src: &dyn Source,
    item: &Item,
    max: u32,
) -> Option<(Vec<u8>, Signature)> {
    let d = decode(src, item, max)?;
    let sig = Signature::of(&d);
    Some((encode_jpeg(&d.img.thumbnail(max, max), 82)?, sig))
}

/// A JPEG suitable for viewing large. For RAW files this is the camera's
/// embedded preview.
pub fn preview(src: &dyn Source, item: &Item, max: u32) -> Option<Vec<u8>> {
    let img = decode(src, item, max)?.img;
    let img = if img.width().max(img.height()) > max {
        img.resize(max, max, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    encode_jpeg(&img, 90)
}

/// What a photo looks like in 128 bits, for finding similar photos, and
/// its full size, for choosing the best of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Difference hash: for a 9x9 greyscale shrink of the picture, whether
    /// each pixel is brighter than the one to its right. Resizing,
    /// recompressing and small edits barely change it.
    pub hash: u64,
    /// The same, comparing each pixel with the one below it. Photos taken
    /// in the same light often share their left-to-right shading (sky over
    /// ground); with both directions, only the same picture matches.
    pub vhash: u64,
    pub width: u32,
    pub height: u32,
    /// Nearly one flat colour (a black frame, a photo of the lens cap).
    /// Such pictures all hash alike whatever they show, so they are only
    /// grouped when they are the same file.
    pub flat: bool,
}

impl Signature {
    fn of(d: &Decoded) -> Self {
        let (hash, vhash, flat) = dhash(&d.img);
        Signature {
            hash,
            vhash,
            width: d.width,
            height: d.height,
            flat,
        }
    }

    /// How many of the 128 bits differ: 0 for the same picture, a few for
    /// the same picture resized, recompressed or lightly edited, around 64
    /// for unrelated pictures.
    pub fn distance(&self, other: &Signature) -> u32 {
        (self.hash ^ other.hash).count_ones() + (self.vhash ^ other.vhash).count_ones()
    }
}

/// The photo's [`Signature`], decoding it as small as its format allows.
pub fn signature(src: &dyn Source, item: &Item) -> Option<Signature> {
    decode(src, item, 64).map(|d| Signature::of(&d))
}

/// The horizontal and vertical difference hashes of an image, and whether
/// it is nearly flat.
pub fn dhash(img: &DynamicImage) -> (u64, u64, bool) {
    // A first shrink to a fixed size makes the hash the same whatever size
    // the picture was decoded at, and averages away fine detail and JPEG
    // noise; the 9x9 shrink is taken from that.
    let grey = img.to_luma8();
    let mid = image::imageops::resize(&grey, 64, 64, image::imageops::FilterType::Triangle);
    let small = image::imageops::resize(&mid, 9, 9, image::imageops::FilterType::Triangle);
    let px = |x, y| small.get_pixel(x, y)[0];
    let (mut h, mut v) = (0u64, 0u64);
    for y in 0..8 {
        for x in 0..8 {
            h = (h << 1) | u64::from(px(x, y) > px(x + 1, y));
            v = (v << 1) | u64::from(px(x, y) > px(x, y + 1));
        }
    }
    let (lo, hi) = small
        .pixels()
        .fold((255u8, 0u8), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
    (h, v, hi - lo < 12)
}

/// A decoded picture and the full size of the original, which may have
/// been decoded smaller.
struct Decoded {
    img: DynamicImage,
    width: u32,
    height: u32,
}

impl From<DynamicImage> for Decoded {
    fn from(img: DynamicImage) -> Self {
        Decoded {
            width: img.width(),
            height: img.height(),
            img,
        }
    }
}

/// Decodes the item's image at no less than `max` pixels where possible.
fn decode(src: &dyn Source, item: &Item, max: u32) -> Option<Decoded> {
    let view = MappedSource::new(src, item.extents.clone());
    if raw(&item.ext) {
        // Only the preview is read, not the whole RAW file, so RAW files of
        // any size get one.
        let (jpeg, orientation) = raw_preview(&view, max)?;
        return decode_jpeg_full(&jpeg, max, orientation);
    }
    if item.size > MAX_PREVIEW_INPUT || !decodable(&item.ext) {
        return None;
    }
    let mut data = vec![0u8; view.len() as usize];
    let n = view.read_at(0, &mut data).ok()?;
    data.truncate(n);

    let full = if data.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg_full(&data, max, None)
    } else if tiff_counts_fit(&data) {
        decode_bytes(&data).map(Decoded::from)
    } else {
        None
    };
    // A photo cut off partway decodes once it is given an end marker: the
    // part that survived shows, the rest grey. Failing that, a damaged JPEG
    // may still carry an intact EXIF thumbnail.
    full.or_else(|| partial_jpeg(&data, max).map(Decoded::from))
        .or_else(|| {
            let j = largest_embedded_jpeg(exif_tiff(&data)?)?;
            decode_jpeg(j, max).map(Decoded::from)
        })
}

/// [`decode_jpeg`], with the full size of the JPEG (upright), and
/// `orientation` applied when the JPEG carries none of its own: previews
/// inside RAW files leave it to the RAW's first IFD.
fn decode_jpeg_full(data: &[u8], max: u32, orientation: Option<u16>) -> Option<Decoded> {
    let mut img = decode_jpeg(data, max)?;
    // The header is at the start; a copy of the first megabyte holds it.
    let head = data[..data.len().min(1 << 20)].to_vec();
    let (mut width, mut height) = jpeg_frame(&head, 0)
        .map(|f| (f.width, f.height))
        .unwrap_or((img.width(), img.height()));
    if exif_tiff(data).and_then(exif_orientation).is_none() {
        if let Some(o) = orientation
            .and_then(|o| u8::try_from(o).ok())
            .and_then(image::metadata::Orientation::from_exif)
        {
            img.apply_orientation(o);
        }
    }
    // The decoded picture is upright; give the full size the same way up.
    if img.width() != img.height() && (img.width() > img.height()) != (width > height) {
        std::mem::swap(&mut width, &mut height);
    }
    Some(Decoded { img, width, height })
}

/// A JPEG's frame header: its size, and where its compressed data starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JpegFrame {
    width: u32,
    height: u32,
    /// Offset of the first scan's data, relative to the JPEG's start.
    scan: u64,
}

/// Reads the header of the JPEG at `at` in `src`, without reading its
/// image data. `None` unless it is a picture this decoder and WebKit show
/// (baseline or progressive): RAW sensor data is often stored as lossless
/// JPEG, which is not a picture to look at.
fn jpeg_frame(src: &dyn Source, at: u64) -> Option<JpegFrame> {
    let mut r = Reader::with_capacity(src, at, 16 << 10);
    if r.u16be()? != 0xFFD8 {
        return None;
    }
    let mut size = None;
    // Real JPEGs have a few dozen segments before their first scan.
    for _ in 0..256 {
        if r.u8()? != 0xFF {
            return None;
        }
        // Fill bytes: 0xFF may repeat before the marker code.
        let mut marker = 0xFF;
        for _ in 0..64 {
            marker = r.u8()?;
            if marker != 0xFF {
                break;
            }
        }
        let len = u64::from(r.u16be()?);
        if len < 2 {
            return None;
        }
        let body = r.pos();
        match marker {
            // Baseline, extended and progressive Huffman-coded frames.
            0xC0..=0xC2 => {
                r.skip(1);
                let h = u32::from(r.u16be()?);
                let w = u32::from(r.u16be()?);
                if w == 0 || h == 0 {
                    return None;
                }
                size = Some((w, h));
            }
            // Lossless, hierarchical and arithmetic-coded frames.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
            0xDA => {
                let (width, height) = size?;
                return Some(JpegFrame {
                    width,
                    height,
                    scan: body + len - 2 - at,
                });
            }
            _ => {}
        }
        r.seek(body + len - 2);
    }
    None
}

/// The length of the JPEG at `at`, found by reading its compressed data to
/// the end marker; at most [`MAX_EMBEDDED_JPEG`].
fn jpeg_len(src: &dyn Source, at: u64, frame: &JpegFrame) -> Option<u64> {
    let limit = src.len().min(at.saturating_add(MAX_EMBEDDED_JPEG));
    let mut r = Reader::new(src, at + frame.scan);
    // In compressed data a 0xFF byte is followed by 0x00 (a stuffed byte),
    // a restart marker or, between progressive scans, a table or scan
    // header. Only the end marker ends the picture.
    loop {
        r.skip_past(0xFF, limit)?;
        if r.u8()? == 0xD9 {
            return Some(r.pos() - at);
        }
    }
}

/// The largest viewable JPEG embedded in a RAW file, read out, with the
/// orientation the RAW gives for it. The walk of the file's structure is
/// cheap; the search byte by byte through its start is not, so it is only
/// made while no preview at least `want` pixels across (or 1600, plenty
/// for a screen) has been found.
pub fn raw_preview(view: &dyn Source, want: u32) -> Option<(Vec<u8>, Option<u16>)> {
    let enough = want.min(1600);
    let (mut found, orientation) = raw_structure(view);
    let mut best = best_frame(view, &found);
    if best.is_none_or(|(_, _, f)| f.width.max(f.height) < enough) {
        for at in jpeg_starts(view, RAW_SEARCH) {
            if !found.iter().any(|&(o, _)| o == at) {
                found.push((at, 0));
            }
        }
        best = best_frame(view, &found);
    }
    let (at, len, frame) = best?;
    // A length the structure gives may be missing or corrupt; the end
    // marker decides then.
    let len = if len < frame.scan || len > MAX_EMBEDDED_JPEG {
        jpeg_len(view, at, &frame)?
    } else {
        len
    };
    let mut jpeg = vec![0u8; len as usize];
    let n = view.read_at(at, &mut jpeg).ok()?;
    jpeg.truncate(n);
    Some((jpeg, orientation))
}

/// Of the candidate JPEGs, the viewable one with the most pixels.
fn best_frame(view: &dyn Source, found: &[(u64, u64)]) -> Option<(u64, u64, JpegFrame)> {
    found
        .iter()
        .filter_map(|&(at, len)| Some((at, len, jpeg_frame(view, at)?)))
        .max_by_key(|(at, _, f)| (u64::from(f.width) * u64::from(f.height), u64::MAX - at))
}

/// Previews a RAW file's structure points to, as (offset, length; 0 when
/// unknown), and its orientation.
fn raw_structure(view: &dyn Source) -> (Vec<(u64, u64)>, Option<u16>) {
    let mut head = [0u8; 16];
    if view.read_at(0, &mut head).ok() != Some(16) {
        return (Vec::new(), None);
    }
    if &head == b"FUJIFILMCCD-RAW " {
        // The header gives the JPEG's offset and length at 84.
        let mut r = Reader::new(view, 84);
        let (Some(off), Some(len)) = (r.u32be(), r.u32be()) else {
            return (Vec::new(), None);
        };
        return (vec![(u64::from(off), u64::from(len))], None);
    }
    if &head[4..12] == b"ftypcrx " {
        return cr3_structure(view);
    }
    match tiff::walk(&mut Reader::new(view, 0), 0, view.len()) {
        Some(info) => (info.previews, info.orientation),
        None => (Vec::new(), None),
    }
}

/// Canon CR3 is an ISO media file. Its `moov` box holds a track per image:
/// the first is the full-size JPEG, the others sensor data. Its metadata,
/// in a Canon box inside `moov`, is TIFF (`CMT1` is IFD0, with the
/// orientation). A 1620-pixel preview also sits in a box of its own, which
/// the byte search finds.
fn cr3_structure(view: &dyn Source) -> (Vec<(u64, u64)>, Option<u16>) {
    let mut r = Reader::new(view, 0);
    let mut pos = 0u64;
    let mut out = Vec::new();
    let mut orientation = None;
    for _ in 0..64 {
        r.seek(pos);
        let (Some(size), Some(typ)) = (r.u32be(), r.array::<4>()) else {
            break;
        };
        let size = match size {
            1 => r.u64be().unwrap_or(0),
            0 => view.len().saturating_sub(pos),
            n => u64::from(n),
        };
        if size < 8 || size > view.len() - pos {
            break;
        }
        if &typ == b"moov" {
            for t in crate::formats::bmff::tracks(&mut r, pos, size) {
                if let Some(&(at, len)) = t.samples.first() {
                    out.push((at, u64::from(len)));
                }
            }
            // CMT1 comes early in the box.
            let mut head = vec![0u8; size.min(256 << 10) as usize];
            let n = view.read_at(pos, &mut head).unwrap_or(0);
            if let Some(i) = memchr::memmem::find(&head[..n], b"CMT1") {
                let base = pos + i as u64 + 4;
                orientation = tiff::walk(&mut Reader::new(view, base), base, size)
                    .and_then(|t| t.orientation);
            }
        }
        pos += size;
    }
    (out, orientation)
}

/// Where JPEGs may start in the first `limit` bytes: an SOI marker
/// followed by another marker.
fn jpeg_starts(view: &dyn Source, limit: u64) -> Vec<u64> {
    let limit = limit.min(view.len());
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < limit {
        let want = buf.len().min((limit - pos) as usize);
        let n = view.read_at(pos, &mut buf[..want]).unwrap_or(0);
        if n < 4 {
            break;
        }
        for i in memchr::memmem::find_iter(&buf[..n], &[0xFF, 0xD8, 0xFF]) {
            // Real files have a handful; a file of 0xFFD8FF repeated must
            // not make a list of millions to check.
            if out.len() < 64 {
                out.push(pos + i as u64);
            }
        }
        // Overlap so a marker split by the buffer edge is still seen.
        pos += n as u64 - 2;
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The surviving part of a JPEG that ends early.
fn partial_jpeg(data: &[u8], max: u32) -> Option<DynamicImage> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // Trailing zeros are unused space after the data, not image.
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let mut fixed = data[..end].to_vec();
    fixed.extend([0xFF, 0xD9]);
    decode_jpeg(&fixed, max)
}

/// Decodes a JPEG, letting the decoder scale it down by up to 8x during
/// decoding. For thumbnails of camera photos that skips most of the work.
fn decode_jpeg(data: &[u8], max: u32) -> Option<DynamicImage> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(data));
    decoder.set_max_decoding_buffer_size(pixel_budget(data) as usize);
    decoder.read_info().ok()?;
    let target = max.min(u32::from(u16::MAX)) as u16;
    let (w, h) = decoder.scale(target, target).ok()?;
    let pixels = decoder.decode().ok()?;
    let format = decoder.info()?.pixel_format;
    let (w, h) = (u32::from(w), u32::from(h));
    let mut img = match format {
        jpeg_decoder::PixelFormat::RGB24 => {
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, pixels)?)
        }
        jpeg_decoder::PixelFormat::L8 => {
            DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, pixels)?)
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            let rgb = pixels
                .chunks_exact(4)
                .flat_map(|p| {
                    let k = u16::from(p[3]);
                    [p[0], p[1], p[2]].map(|c| (u16::from(c) * k / 255) as u8)
                })
                .collect();
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, rgb)?)
        }
        _ => return decode_bytes(data),
    };
    if let Some(o) = exif_tiff(data)
        .and_then(exif_orientation)
        .and_then(image::metadata::Orientation::from_exif)
    {
        img.apply_orientation(o);
    }
    Some(img)
}

/// For a TIFF, whether every tag in its first IFD has no more values than
/// the file has bytes. The TIFF decoder reserves memory for a tag's value
/// count before reading it, so a corrupt count costs 128 MB. Not a TIFF:
/// true.
fn tiff_counts_fit(data: &[u8]) -> bool {
    let le = match data.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return true,
    };
    let num = |o: usize, n: usize| {
        data.get(o..o.checked_add(n)?).map(|b| {
            b.iter()
                .enumerate()
                .map(|(i, &x)| u64::from(x) << (8 * if le { i } else { n - 1 - i }))
                .sum::<u64>()
        })
    };
    let Some(ifd) = num(4, 4) else { return false };
    let Some(count) = num(ifd as usize, 2) else {
        return false;
    };
    (0..count as usize).all(|i| {
        let e = ifd as usize + 2 + i * 12;
        let (Some(typ), Some(n)) = (num(e + 2, 2), num(e + 4, 4)) else {
            return false;
        };
        let size = match typ {
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 | 16..=18 => 8,
            _ => 1,
        };
        n * size <= data.len() as u64
    })
}

/// The EXIF orientation (tag 0x0112 in IFD0) from a TIFF block.
fn exif_orientation(tiff: &[u8]) -> Option<u8> {
    let le = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| {
        tiff.get(o..o + 2).map(|b| {
            if le {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            }
        })
    };
    let u32_at = |o: usize| {
        tiff.get(o..o + 4).map(|b| {
            let a = [b[0], b[1], b[2], b[3]];
            if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count.min(256)).find_map(|i| {
        let e = ifd + 2 + i * 12;
        (u16_at(e)? == 0x0112)
            .then(|| u16_at(e + 8).map(|v| v as u8))
            .flatten()
    })
}

/// Most bytes of decoded pixels worth allocating for `data`. Real images
/// compress at most about 1000:1, so a header claiming far more pixels
/// than its file could hold is corrupt.
fn pixel_budget(data: &[u8]) -> u64 {
    (data.len() as u64)
        .saturating_mul(1024)
        .clamp(64 << 20, 512 << 20)
}

fn decode_bytes(data: &[u8]) -> Option<DynamicImage> {
    let budget = pixel_budget(data);
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    // Decoders size their working buffers (TIFF strips) from the header
    // too; hold them to the same budget.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(budget);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    // from_decoder allocates the whole canvas the header claims, with no
    // limit, before reading a pixel: a damaged GIF header asked for 6 GB.
    if decoder.total_bytes() > budget {
        return None;
    }
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Some(img)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.to_rgb8().write_with_encoder(enc).ok()?;
    Some(out)
}

/// The biggest JPEG referenced from a TIFF structure (EXIF thumbnails).
fn largest_embedded_jpeg(tiff_data: &[u8]) -> Option<&[u8]> {
    let owned = tiff_data.to_vec();
    let info = tiff::walk(&mut Reader::new(&owned, 0), 0, owned.len() as u64)?;
    let (off, len) = info
        .jpegs
        .into_iter()
        .filter(|&(off, len)| {
            let s = off as usize;
            tiff_data.get(s..s + 2) == Some(&[0xFF, 0xD8][..])
                && off + len <= tiff_data.len() as u64
        })
        .max_by_key(|&(_, len)| len)?;
    tiff_data.get(off as usize..(off + len) as usize)
}

/// The TIFF block inside a JPEG's EXIF segment.
fn exif_tiff(jpeg: &[u8]) -> Option<&[u8]> {
    let mut i = 2;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if marker == 0xDA {
            return None;
        }
        let body = jpeg.get(i + 4..i + 2 + len)?;
        if marker == 0xE1 {
            if let Some(t) = body.strip_prefix(b"Exif\0\0") {
                return Some(t);
            }
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64x32 JPEG whose EXIF says "rotate 90° clockwise to view".
    fn rotated_jpeg() -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            32,
            image::Rgb([200, 30, 30]),
        ));
        let plain = encode_jpeg(&img, 90).unwrap();
        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        tiff.extend(0x0112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend([6, 0, 0, 0]);
        tiff.extend(0u32.to_le_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        out.extend(&plain[2..]);
        out
    }

    #[test]
    fn shows_the_surviving_part_of_a_cut_off_photo() {
        let img = image::RgbImage::from_fn(320, 240, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img)
            .unwrap();
        let mut cut = jpeg[..jpeg.len() / 2].to_vec();
        cut.extend(vec![0u8; 4096]);
        assert!(decode_jpeg(&cut, 1000).is_none());
        let shown = partial_jpeg(&cut, 1000).expect("partial image");
        assert_eq!((shown.width(), shown.height()), (320, 240));
    }

    #[test]
    fn applies_exif_orientation() {
        let img = decode_jpeg(&rotated_jpeg(), 256).unwrap();
        assert_eq!((img.width(), img.height()), (32, 64));
    }

    // --- RAW previews -----------------------------------------------------

    /// A baseline JPEG of the given size, with no EXIF.
    pub(crate) fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 120])
        });
        encode_jpeg(&DynamicImage::ImageRgb8(img), 85).unwrap()
    }

    /// Something that looks like a JPEG but is lossless (SOF3), as RAW
    /// sensor data is stored, and large.
    fn lossless(len: usize) -> Vec<u8> {
        let mut v = vec![
            0xFF, 0xD8, 0xFF, 0xC3, 0, 11, 14, 0x0F, 0xA0, 0x17, 0x70, 1, 1, 0x11, 0,
        ];
        v.extend([0xFF, 0xDA, 0, 8, 1, 1, 0, 1, 0, 0]);
        v.resize(len, 0x55);
        v.extend([0xFF, 0xD9]);
        v
    }

    /// Builds TIFF files: IFD `k` is written at 0x100 * (k + 1) and data
    /// blobs from 0x1000 on.
    struct TiffFile {
        data: Vec<u8>,
        le: bool,
    }

    impl TiffFile {
        fn new(le: bool, magic: &[u8; 4]) -> Self {
            let mut data = vec![0u8; 0x1000];
            data[..4].copy_from_slice(magic);
            data[4..8].copy_from_slice(&if le {
                0x100u32.to_le_bytes()
            } else {
                0x100u32.to_be_bytes()
            });
            TiffFile { data, le }
        }

        fn blob(&mut self, b: &[u8]) -> u32 {
            while !self.data.len().is_multiple_of(2) {
                self.data.push(0);
            }
            let at = self.data.len() as u32;
            self.data.extend(b);
            at
        }

        fn u16(&self, v: u16) -> [u8; 2] {
            if self.le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }

        fn u32(&self, v: u32) -> [u8; 4] {
            if self.le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }

        /// Writes IFD `k`: entries of (tag, type, count, value or offset),
        /// and the IFD that follows it in the chain (0 for none).
        fn ifd(&mut self, k: usize, entries: &[(u16, u16, u32, u32)], next: u32) {
            let mut v = self.u16(entries.len() as u16).to_vec();
            for &(tag, typ, n, val) in entries {
                v.extend(self.u16(tag));
                v.extend(self.u16(typ));
                v.extend(self.u32(n));
                if typ == 3 && n == 1 {
                    v.extend(self.u16(val as u16));
                    v.extend([0, 0]);
                } else {
                    v.extend(self.u32(val));
                }
            }
            v.extend(self.u32(next));
            let at = 0x100 * (k + 1);
            assert!(v.len() <= 0x100);
            self.data[at..at + v.len()].copy_from_slice(&v);
        }
    }

    const IFD1: u32 = 0x200;
    const IFD2: u32 = 0x300;
    const IFD3: u32 = 0x400;

    fn raw_item(ext: &str, len: usize) -> Item {
        Item {
            id: 0,
            name: format!("IMG_0001.{}", ext.to_uppercase()),
            dir: String::new(),
            ext: ext.into(),
            kind: crate::Kind::Raw,
            origin: crate::session::Origin::Deleted,
            health: crate::session::Health::Good,
            size: len as u64,
            taken: None,
            modified: None,
            extents: vec![crate::extent::Extent::new(0, len as u64)],
            volume: None,
            fix: None,
            found_by: None,
            nsrl_status: None,
        }
    }

    /// The size of the preview Reperio shows for a RAW file.
    fn shown(data: &[u8], ext: &str) -> Option<(u32, u32)> {
        let src = data.to_vec();
        let item = raw_item(ext, data.len());
        let jpeg = preview(&src, &item, 4000)?;
        let img = image::load_from_memory(&jpeg).unwrap();
        Some((img.width(), img.height()))
    }

    /// A CR2: IFD0 is the full-size JPEG as one old-style JPEG strip, IFD1
    /// a small EXIF thumbnail, IFD3 the lossless sensor data, larger than
    /// either.
    fn cr2(orientation: u16) -> Vec<u8> {
        let mut t = TiffFile::new(true, b"II*\0");
        t.data[4..8].copy_from_slice(&0x100u32.to_le_bytes());
        t.data[8..12].copy_from_slice(b"CR\x02\0");
        let make = t.blob(b"Canon\0");
        let full = jpeg(1200, 800);
        let full_at = t.blob(&full);
        let thumb = jpeg(160, 120);
        let thumb_at = t.blob(&thumb);
        let sensor = lossless(3 << 20);
        let sensor_at = t.blob(&sensor);
        t.ifd(
            0,
            &[
                (259, 3, 1, 6),
                (271, 2, 6, make),
                (273, 4, 1, full_at),
                (274, 3, 1, u32::from(orientation)),
                (279, 4, 1, full.len() as u32),
            ],
            IFD1,
        );
        t.ifd(
            1,
            &[(513, 4, 1, thumb_at), (514, 4, 1, thumb.len() as u32)],
            IFD2,
        );
        t.ifd(
            2,
            &[(259, 3, 1, 1), (273, 4, 1, thumb_at), (279, 4, 1, 16)],
            IFD3,
        );
        t.ifd(
            3,
            &[
                (259, 3, 1, 6),
                (273, 4, 1, sensor_at),
                (279, 4, 1, sensor.len() as u32),
            ],
            0,
        );
        t.data
    }

    #[test]
    fn cr2_shows_its_full_size_jpeg_upright() {
        assert_eq!(shown(&cr2(1), "cr2"), Some((1200, 800)));
        // Orientation 8 (rotate 270° to view) is in the RAW's IFD0 only.
        assert_eq!(shown(&cr2(8), "cr2"), Some((800, 1200)));
        // A thumbnail needs only the structure, and gets the same picture.
        let data = cr2(1);
        let item = raw_item("cr2", data.len());
        let (thumb, sig) = thumbnail_with_signature(&data, &item, 320).unwrap();
        let img = image::load_from_memory(&thumb).unwrap();
        assert_eq!((img.width(), img.height()), (320, 213));
        assert_eq!((sig.width, sig.height), (1200, 800));
    }

    #[test]
    fn dng_shows_its_preview_ifd_not_the_sensor_data() {
        // IFD0: an uncompressed thumbnail; SubIFDs: the lossless-JPEG
        // sensor data and a new-style JPEG preview.
        let mut t = TiffFile::new(true, b"II*\0");
        let strip = t.blob(&[90; 256 * 3]);
        let sensor = lossless(2 << 20);
        let sensor_at = t.blob(&sensor);
        let prev = jpeg(1024, 683);
        let prev_at = t.blob(&prev);
        let subs = t.blob(&[IFD1.to_le_bytes(), IFD2.to_le_bytes()].concat());
        t.ifd(
            0,
            &[
                (254, 4, 1, 1),
                (259, 3, 1, 1),
                (273, 4, 1, strip),
                (279, 4, 1, 256 * 3),
                (330, 4, 2, subs),
                (50706, 1, 4, 0x0104),
            ],
            0,
        );
        t.ifd(
            1,
            &[
                (254, 4, 1, 0),
                (259, 3, 1, 7),
                (273, 4, 1, sensor_at),
                (279, 4, 1, sensor.len() as u32),
            ],
            0,
        );
        t.ifd(
            2,
            &[
                (254, 4, 1, 1),
                (259, 3, 1, 7),
                (273, 4, 1, prev_at),
                (279, 4, 1, prev.len() as u32),
            ],
            0,
        );
        assert_eq!(shown(&t.data, "dng"), Some((1024, 683)));
    }

    #[test]
    fn nef_shows_its_jpg_from_raw_in_a_sub_ifd() {
        // Big-endian, as Nikon writes them.
        let mut t = TiffFile::new(false, b"MM\0*");
        let make = t.blob(b"NIKON CORPORATION\0");
        let thumb = t.blob(&[0; 160 * 120 * 3]);
        let full = jpeg(1500, 1000);
        let full_at = t.blob(&full);
        let sensor_at = t.blob(&vec![7; 1 << 20]);
        let subs = t.blob(&[IFD1.to_be_bytes(), IFD2.to_be_bytes()].concat());
        t.ifd(
            0,
            &[
                (259, 3, 1, 1),
                (271, 2, 18, make),
                (273, 4, 1, thumb),
                (279, 4, 1, 160 * 120 * 3),
                (330, 4, 2, subs),
            ],
            0,
        );
        t.ifd(
            1,
            &[
                (259, 3, 1, 6),
                (513, 4, 1, full_at),
                (514, 4, 1, full.len() as u32),
            ],
            0,
        );
        t.ifd(
            2,
            &[
                (259, 3, 1, 34713),
                (273, 4, 1, sensor_at),
                (279, 4, 1, 1 << 20),
            ],
            0,
        );
        assert_eq!(shown(&t.data, "nef"), Some((1500, 1000)));
    }

    #[test]
    fn rw2_shows_its_jpg_from_raw_tag() {
        let mut t = TiffFile::new(true, b"IIU\0");
        let full = jpeg(900, 600);
        let at = t.blob(&full);
        t.ifd(
            0,
            &[
                (0x2E, 7, full.len() as u32, at),
                (273, 4, 1, at),
                (279, 4, 1, 64),
            ],
            0,
        );
        assert_eq!(shown(&t.data, "rw2"), Some((900, 600)));
    }

    #[test]
    fn orf_finds_a_preview_its_structure_does_not_point_to() {
        // Olympus keeps the large preview in its maker notes; only the
        // small EXIF thumbnail is in the IFDs.
        let mut t = TiffFile::new(true, b"IIRO");
        let thumb = jpeg(160, 120);
        let thumb_at = t.blob(&thumb);
        t.blob(b"OLYMPUS\0II\x03\0");
        t.blob(&jpeg(1280, 960));
        t.blob(&vec![3; 1 << 20]);
        t.ifd(
            0,
            &[(513, 4, 1, thumb_at), (514, 4, 1, thumb.len() as u32)],
            0,
        );
        assert_eq!(shown(&t.data, "orf"), Some((1280, 960)));
        // For a thumbnail the small one is enough, and found without the
        // search.
        let src = t.data.clone();
        let (j, _) = raw_preview(&src, 120).unwrap();
        assert_eq!(j, thumb);
    }

    #[test]
    fn raf_shows_the_jpeg_its_header_points_to() {
        let full = jpeg(1920, 1280);
        let mut raf = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
        raf.resize(84, b' ');
        let at = 160u32;
        for v in [at, full.len() as u32, at + full.len() as u32, 100, 0, 0] {
            raf.extend(v.to_be_bytes());
        }
        raf.resize(at as usize, 0);
        raf.extend(&full);
        raf.extend(vec![9; 100_000]);
        assert_eq!(shown(&raf, "raf"), Some((1920, 1280)));
    }

    #[test]
    fn cr3_shows_its_first_track_upright() {
        use crate::formats::bmff::tests::bx;
        let full = jpeg(1600, 1200);
        let small = jpeg(320, 240);
        // CMT1: IFD0 with orientation 6 (rotate 90° to view).
        let mut cmt1 = TiffFile::new(true, b"II*\0");
        cmt1.ifd(0, &[(274, 3, 1, 6)], 0);
        let canon = bx(
            b"uuid",
            &[
                &[
                    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46,
                    0x2b, 0x6a, 0x48,
                ][..],
                &bx(b"CMT1", &cmt1.data),
            ]
            .concat(),
        );
        let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
        // The track's table is written once the media's offset is known.
        let trak = |at: u32, len: u32| {
            let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stsd.extend(bx(b"CRAW", &[0; 8]));
            let stco = [&[0u8, 0, 0, 0, 0, 0, 0, 1][..], &at.to_be_bytes()].concat();
            let stsc = [
                &[0u8, 0, 0, 0, 0, 0, 0, 1][..],
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(),
            ]
            .concat();
            let stsz = [
                &[0u8; 4][..],
                &0u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &len.to_be_bytes(),
            ]
            .concat();
            let stbl = bx(
                b"stbl",
                &[
                    bx(b"stsd", &stsd),
                    bx(b"stco", &stco),
                    bx(b"stsc", &stsc),
                    bx(b"stsz", &stsz),
                ]
                .concat(),
            );
            bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)))
        };
        let moov_len = bx(b"moov", &[canon.clone(), trak(0, 0), trak(0, 0)].concat()).len();
        let mdat_at = (ftyp.len() + moov_len + 8) as u32;
        let moov = bx(
            b"moov",
            &[
                canon,
                trak(mdat_at, full.len() as u32),
                trak(mdat_at + full.len() as u32, small.len() as u32),
            ]
            .concat(),
        );
        let mut file = [ftyp, moov].concat();
        file.extend(bx(
            b"mdat",
            &[full.clone(), small, vec![1; 50_000]].concat(),
        ));
        assert_eq!(shown(&file, "cr3"), Some((1200, 1600)));
    }

    #[test]
    fn raw_without_a_viewable_preview_shows_nothing() {
        let mut t = TiffFile::new(true, b"II*\0");
        let sensor = lossless(1 << 20);
        let at = t.blob(&sensor);
        t.ifd(
            0,
            &[
                (259, 3, 1, 7),
                (273, 4, 1, at),
                (279, 4, 1, sensor.len() as u32),
            ],
            0,
        );
        assert_eq!(shown(&t.data, "dng"), None);
    }

    #[test]
    fn damaged_raw_files_never_panic() {
        let good = cr2(6);
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // The same damage under each format's header, so each one's walk
        // meets it. Finding the preview is what reads untrusted structure;
        // decoding what it finds is the JPEG decoder's job.
        let heads: [&[u8]; 4] = [
            b"II*\0",
            b"FUJIFILMCCD-RAW ",
            b"\0\0\0\x18ftypcrx ",
            b"IIRO",
        ];
        for head in heads {
            for _ in 0..60 {
                let mut bad = good.clone();
                bad[..head.len()].copy_from_slice(head);
                // Cut it short, and scribble over its structure.
                bad.truncate((next() % good.len() as u64) as usize);
                for _ in 0..20 {
                    let len = bad.len().clamp(1, 0x1200);
                    let at = (next() % len as u64) as usize;
                    if at < bad.len() {
                        bad[at] = next() as u8;
                    }
                }
                let _ = raw_preview(&bad, 4000);
            }
        }
        // Lengths and offsets of 0xFFFFFFFF everywhere.
        let _ = shown(&[0xFF; 70_000], "nef");
        let _ = raw_preview(&vec![0xFF; 70_000], 4000);
    }
}
