//! Rebuilding videos whose index is missing.
//!
//! An MP4 or MOV from a camera is mostly one big `mdat` box of compressed
//! frames, followed by a `moov` box: the index saying where every frame is,
//! how long it lasts and how to decode it. Cameras write the index last, when
//! recording stops. A recording cut short (battery out, card pulled) has no
//! index; so does a recovered video whose last part was overwritten or never
//! found. Players refuse such files, though every frame is still there.
//!
//! Professional repair tools rebuild the index using a good video from the
//! same camera as a template: it gives the codec settings (frame size,
//! profile, the parameter sets the decoder needs), frame rate and layout.
//! The broken file's frames are then found by walking the video stream
//! itself: H.264 and H.265 frames are sequences of length-prefixed NAL
//! units whose headers say where each frame starts. The original data is
//! kept untouched; only a new index is written after it.
//!
//! Without a reference, the codec settings come from the stream itself
//! when the camera repeats its parameter sets before keyframes, as most
//! action cameras and drones do; a camera profile (see [`crate::camera`])
//! fills in the rest.
//!
//! The sound is rebuilt from what lies between the video frames: see
//! [`crate::audiofix`]. Where it can't be told apart from other data, the
//! repaired video plays without sound.

use std::collections::HashMap;
use std::io::{self, Write};

use crate::audiofix;
use crate::camera::{self, AudioCodec, Profile};
use crate::source::Source;

/// Largest index read from a reference video.
const MAX_MOOV: u64 = 64 << 20;
/// Largest single frame accepted.
const MAX_FRAME: u64 = 64 << 20;
/// Largest NAL unit accepted.
const MAX_NAL: u64 = 32 << 20;
/// A stretch this long without a frame ends the video: what follows is
/// another file or leftover data.
const MAX_GAP: u64 = 8 << 20;
/// Most frames rebuilt (about 40 hours at 60 fps).
const MAX_FRAMES: usize = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Avc,
    Hevc,
}

/// What a good video from the same camera teaches about the broken one.
#[derive(Debug, Clone)]
pub struct Reference {
    ftyp: Vec<u8>,
    moov: BoxNode,
    /// Index of the video `trak` among the `moov` box's children.
    trak: usize,
    codec: Codec,
    nal_len: usize,
    /// Media timescale and frame duration of the video track.
    timescale: u32,
    delta: u32,
    movie_timescale: u32,
    params: Params,
    /// How to find and describe the sound, when it can be rebuilt.
    sound: Option<Sound>,
    /// The camera that made the video, when recognised.
    pub camera: Option<&'static Profile>,
}

/// The sound format of the videos a reference stands for.
#[derive(Debug, Clone)]
enum Sound {
    Aac {
        channels: u16,
        /// Known from a reference; otherwise measured against the video's
        /// length, falling back to `usual_rate`.
        rate: Option<u32>,
        usual_rate: u32,
        /// The reference's sound track, kept as the template.
        trak: Option<BoxNode>,
    },
    /// Uncompressed sound: each sample (in the index's sense) is
    /// `unit_bytes` long and lasts `unit_delta` at `timescale`.
    Pcm {
        unit_bytes: u32,
        unit_delta: u32,
        timescale: u32,
        trak: Option<BoxNode>,
        /// The sample entry box, when there is no template track.
        entry: Vec<u8>,
    },
}

/// A rebuilt video: `prefix`, then `data` copied from the broken file, then
/// the new index.
#[derive(Debug, Clone)]
pub struct Rebuilt {
    pub prefix: Vec<u8>,
    /// Range of the broken file holding the frames.
    pub data: (u64, u64),
    pub moov: Vec<u8>,
    pub frames: usize,
    pub seconds: f64,
    /// Sound samples (AAC frames, or PCM units) found and indexed; zero
    /// when the repaired video plays without sound.
    pub sound_samples: usize,
}

impl Rebuilt {
    pub fn len(&self) -> u64 {
        self.prefix.len() as u64 + (self.data.1 - self.data.0) + self.moov.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes the repaired video. Returns the number of unreadable sectors
    /// met in the broken file (written as zeros).
    pub fn write_to(&self, broken: &dyn Source, out: &mut impl Write) -> io::Result<u64> {
        out.write_all(&self.prefix)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut pos = self.data.0;
        let mut bad = 0;
        while pos < self.data.1 {
            let want = buf.len().min((self.data.1 - pos) as usize);
            let (n, b) = crate::source::read_tolerant(broken, pos, &mut buf[..want]);
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "video data ended early",
                ));
            }
            out.write_all(&buf[..n])?;
            bad += b;
            pos += n as u64;
        }
        out.write_all(&self.moov)?;
        Ok(bad)
    }

    /// The repaired video as a source, for previews and checks.
    pub fn view<'a>(&'a self, broken: &'a dyn Source) -> RebuiltView<'a> {
        RebuiltView { r: self, broken }
    }
}

pub struct RebuiltView<'a> {
    r: &'a Rebuilt,
    broken: &'a dyn Source,
}

impl Source for RebuiltView<'_> {
    fn len(&self) -> u64 {
        self.r.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let p = self.r.prefix.len() as u64;
        let d = self.r.data.1 - self.r.data.0;
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let rest = &mut buf[done..];
            let n = if at < p {
                let s = &self.r.prefix[at as usize..];
                let n = s.len().min(rest.len());
                rest[..n].copy_from_slice(&s[..n]);
                n
            } else if at < p + d {
                let want = rest.len().min((p + d - at) as usize);
                let n = self
                    .broken
                    .read_at(self.r.data.0 + at - p, &mut rest[..want])?;
                if n == 0 {
                    break;
                }
                n
            } else {
                let i = (at - p - d) as usize;
                let Some(s) = self.r.moov.get(i..) else { break };
                let n = s.len().min(rest.len());
                if n == 0 {
                    break;
                }
                rest[..n].copy_from_slice(&s[..n]);
                n
            };
            done += n;
        }
        Ok(done)
    }
}

// ---------------------------------------------------------------- boxes

/// Boxes whose children are boxes, and which a rebuilt index modifies.
const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"edts", b"mdia", b"minf", b"stbl"];

#[derive(Debug, Clone)]
struct BoxNode {
    typ: [u8; 4],
    /// The box body, for boxes that are not containers.
    body: Vec<u8>,
    children: Vec<BoxNode>,
}

impl BoxNode {
    fn leaf(typ: &[u8; 4], body: Vec<u8>) -> Self {
        Self {
            typ: *typ,
            body,
            children: Vec::new(),
        }
    }

    fn parse(data: &[u8], depth: usize) -> Option<Vec<BoxNode>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?);
            let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
            let (size, header) = match size32 {
                0 => (data.len() - pos, 8),
                1 => {
                    let s = u64::from_be_bytes(data.get(pos + 8..pos + 16)?.try_into().ok()?);
                    (usize::try_from(s).ok()?, 16)
                }
                n => (n as usize, 8),
            };
            if size < header || pos + size > data.len() {
                return None;
            }
            let body = &data[pos + header..pos + size];
            let node = if CONTAINERS.contains(&&typ) && depth < 8 {
                BoxNode {
                    typ,
                    body: Vec::new(),
                    children: Self::parse(body, depth + 1)?,
                }
            } else {
                Self::leaf(&typ, body.to_vec())
            };
            out.push(node);
            pos += size;
        }
        Some(out)
    }

    fn child(&self, typ: &[u8; 4]) -> Option<&BoxNode> {
        self.children.iter().find(|c| &c.typ == typ)
    }

    fn child_mut(&mut self, typ: &[u8; 4]) -> Option<&mut BoxNode> {
        self.children.iter_mut().find(|c| &c.typ == typ)
    }

    fn serialize(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend([0, 0, 0, 0]);
        out.extend(self.typ);
        if CONTAINERS.contains(&&self.typ) {
            for c in &self.children {
                c.serialize(out);
            }
        } else {
            out.extend(&self.body);
        }
        let size = (out.len() - start) as u32;
        out[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn full_box(version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
    let mut v = vec![
        version,
        (flags >> 16) as u8,
        (flags >> 8) as u8,
        flags as u8,
    ];
    v.extend(rest);
    v
}

/// Sets the duration field of an `mvhd`, `tkhd` or `mdhd` body.
fn set_duration(body: &mut [u8], typ: &[u8; 4], duration: u64) {
    let v1 = body.first() == Some(&1);
    let at = match (typ, v1) {
        (b"tkhd", false) => 20,
        (b"tkhd", true) => 28,
        (_, false) => 16,
        (_, true) => 24,
    };
    if v1 {
        if let Some(s) = body.get_mut(at..at + 8) {
            s.copy_from_slice(&duration.to_be_bytes());
        }
    } else if let Some(s) = body.get_mut(at..at + 4) {
        s.copy_from_slice(&(duration.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    }
}

fn timescale(body: &[u8]) -> Option<u32> {
    be32(body, if body.first() == Some(&1) { 20 } else { 12 })
}

// ------------------------------------------------------- parameter sets

/// Reads bits from an RBSP (a NAL unit with emulation prevention removed).
struct Bits {
    d: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut d = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            d.push(b);
        }
        Self { d, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

#[derive(Debug, Clone, Default)]
struct AvcSps {
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    frame_mbs_only: bool,
    separate_colour_plane: bool,
    /// What a sample description needs, when the picture size and
    /// format could be read.
    format: Option<Format>,
}

#[derive(Debug, Clone, Default)]
struct HevcSps {
    log2_max_poc_lsb: u32,
    separate_colour_plane: bool,
    format: Option<Format>,
}

/// Picture size and format from a sequence parameter set.
#[derive(Debug, Clone, Copy, Default)]
struct Format {
    width: u32,
    height: u32,
    chroma: u32,
    depth_luma: u32,
    depth_chroma: u32,
    /// Frame rate from the stream's timing info: (timescale, frame duration).
    timing: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Default)]
struct HevcPps {
    sps: u32,
    output_flag: bool,
    extra_bits: u32,
}

/// Parameter sets, by ID: what slice headers are read with.
#[derive(Debug, Clone, Default)]
struct Params {
    avc_sps: HashMap<u32, AvcSps>,
    avc_pps: HashMap<u32, u32>,
    hevc_sps: HashMap<u32, HevcSps>,
    hevc_pps: HashMap<u32, HevcPps>,
    /// Frame rate from an H.265 video parameter set.
    hevc_timing: Option<(u32, u32)>,
}

impl Params {
    /// The picture format of the first sequence parameter set that has one.
    fn format(&self) -> Option<Format> {
        let mut f = self
            .avc_sps
            .values()
            .find_map(|s| s.format)
            .or_else(|| self.hevc_sps.values().find_map(|s| s.format))?;
        if f.timing.is_none() {
            f.timing = self.hevc_timing;
        }
        Some(f)
    }

    fn add(&mut self, codec: Codec, nal: &[u8]) {
        match codec {
            Codec::Avc => match nal.first().map(|b| b & 0x1F) {
                Some(7) => {
                    if let Some((id, sps)) = parse_avc_sps(&nal[1..]) {
                        self.avc_sps.insert(id, sps);
                    }
                }
                Some(8) => {
                    let mut b = Bits::new(&nal[1..]);
                    if let (Some(pps), Some(sps)) = (b.ue(), b.ue()) {
                        self.avc_pps.insert(pps, sps);
                    }
                }
                _ => {}
            },
            Codec::Hevc => match nal.first().map(|b| (b >> 1) & 0x3F) {
                Some(32) if nal.len() > 2 => {
                    if let Some(t) = parse_hevc_vps_timing(&nal[2..]) {
                        self.hevc_timing = Some(t);
                    }
                }
                Some(33) if nal.len() > 2 => {
                    if let Some((id, sps)) = parse_hevc_sps(&nal[2..]) {
                        self.hevc_sps.insert(id, sps);
                    }
                }
                Some(34) if nal.len() > 2 => {
                    let mut b = Bits::new(&nal[2..]);
                    let parsed = (|| {
                        let id = b.ue()?;
                        let sps = b.ue()?;
                        b.skip(1); // dependent_slice_segments_enabled_flag
                        let output_flag = b.bit()? == 1;
                        let extra_bits = b.bits(3)?;
                        Some((
                            id,
                            HevcPps {
                                sps,
                                output_flag,
                                extra_bits,
                            },
                        ))
                    })();
                    if let Some((id, pps)) = parsed {
                        self.hevc_pps.insert(id, pps);
                    }
                }
                _ => {}
            },
        }
    }
}

fn parse_avc_sps(rbsp: &[u8]) -> Option<(u32, AvcSps)> {
    let mut b = Bits::new(rbsp);
    let profile = b.bits(8)?;
    b.skip(16);
    let id = b.ue()?;
    let mut sps = AvcSps::default();
    let (mut chroma, mut depth_luma, mut depth_chroma) = (1, 8, 8);
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = b.ue()?;
        if chroma == 3 {
            sps.separate_colour_plane = b.bit()? == 1;
        }
        depth_luma = b.ue()?.saturating_add(8);
        depth_chroma = b.ue()?.saturating_add(8);
        b.skip(1);
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    sps.log2_max_frame_num = b.ue()? + 4;
    sps.poc_type = b.ue()?;
    if sps.poc_type == 0 {
        sps.log2_max_poc_lsb = b.ue()? + 4;
    } else if sps.poc_type == 1 {
        b.skip(1);
        b.se()?;
        b.se()?;
        for _ in 0..b.ue()?.min(255) {
            b.se()?;
        }
    }
    b.ue()?;
    b.skip(1);
    let width_mbs = b.ue()?;
    let height_units = b.ue()?;
    sps.frame_mbs_only = b.bit()? == 1;
    // The rest matters only for describing the stream; a parameter set
    // cut short still serves to read slice headers.
    sps.format = (|| {
        if !sps.frame_mbs_only {
            b.skip(1);
        }
        b.skip(1);
        let mut crop = [0u32; 4];
        if b.bit()? == 1 {
            for c in &mut crop {
                *c = b.ue()?;
            }
        }
        let field = 2 - u32::from(sps.frame_mbs_only);
        let (cx, cy) = match chroma {
            0 | 3 => (1, field),
            1 => (2, 2 * field),
            _ => (2, field),
        };
        let width = (width_mbs.checked_add(1)?.checked_mul(16)?)
            .checked_sub(cx * (crop[0].checked_add(crop[1])?))?;
        let height = (height_units.checked_add(1)?.checked_mul(16 * field)?)
            .checked_sub(cy * (crop[2].checked_add(crop[3])?))?;
        let mut timing = None;
        if b.bit()? == 1 {
            if b.bit()? == 1 && b.bits(8)? == 255 {
                b.skip(32);
            }
            if b.bit()? == 1 {
                b.skip(1);
            }
            if b.bit()? == 1 {
                b.skip(4);
                if b.bit()? == 1 {
                    b.skip(24);
                }
            }
            if b.bit()? == 1 {
                b.ue()?;
                b.ue()?;
            }
            if b.bit()? == 1 {
                let units = b.bits(32)?;
                let scale = b.bits(32)?;
                // A frame is two field ticks.
                timing = plausible_rate(scale, units.checked_mul(2)?);
            }
        }
        (width > 0 && height > 0 && width <= 16384 && height <= 16384).then_some(Format {
            width,
            height,
            chroma,
            depth_luma,
            depth_chroma,
            timing,
        })
    })();
    (sps.log2_max_frame_num <= 16 && sps.log2_max_poc_lsb <= 16).then_some((id, sps))
}

/// A frame rate between 1 and 240 fps, as (timescale, frame duration).
fn plausible_rate(scale: u32, duration: u32) -> Option<(u32, u32)> {
    (duration > 0 && scale >= duration && scale / duration <= 240).then_some((scale, duration))
}

fn parse_hevc_sps(rbsp: &[u8]) -> Option<(u32, HevcSps)> {
    let mut b = Bits::new(rbsp);
    b.skip(4);
    let sub_layers = b.bits(3)?;
    b.skip(1);
    // profile_tier_level
    b.skip(96);
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers as usize));
    }
    for (profile, level) in present {
        if profile == 1 {
            b.skip(88);
        }
        if level == 1 {
            b.skip(8);
        }
    }
    let id = b.ue()?;
    let mut sps = HevcSps::default();
    let chroma = b.ue()?;
    if chroma == 3 {
        sps.separate_colour_plane = b.bit()? == 1;
    }
    let width = b.ue()?;
    let height = b.ue()?;
    let mut crop = [0u32; 4];
    if b.bit()? == 1 {
        for c in &mut crop {
            *c = b.ue()?;
        }
    }
    let depth_luma = b.ue()?.saturating_add(8);
    let depth_chroma = b.ue()?.saturating_add(8);
    sps.log2_max_poc_lsb = b.ue()?.checked_add(4)?;
    let (cx, cy) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let w = width.checked_sub(cx * crop[0].checked_add(crop[1])?);
    let h = height.checked_sub(cy * crop[2].checked_add(crop[3])?);
    if let (Some(width), Some(height)) = (w, h) {
        if (1..=16384).contains(&width) && (1..=16384).contains(&height) {
            sps.format = Some(Format {
                width,
                height,
                chroma,
                depth_luma,
                depth_chroma,
                timing: None,
            });
        }
    }
    (sps.log2_max_poc_lsb <= 16).then_some((id, sps))
}

/// The frame rate an H.265 video parameter set gives, if any.
fn parse_hevc_vps_timing(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut b = Bits::new(rbsp);
    b.skip(4 + 2 + 6);
    let sub_layers = b.bits(3)?;
    b.skip(1 + 16);
    b.skip(96);
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers as usize));
    }
    for (profile, level) in present {
        if profile == 1 {
            b.skip(88);
        }
        if level == 1 {
            b.skip(8);
        }
    }
    let all = b.bit()? == 1;
    for _ in if all { 0 } else { sub_layers }..=sub_layers {
        b.ue()?;
        b.ue()?;
        b.ue()?;
    }
    let max_layer_id = b.bits(6)?;
    let sets = b.ue()?;
    if sets > 1023 {
        return None;
    }
    b.skip(sets as usize * (max_layer_id as usize + 1));
    if b.bit()? != 1 {
        return None;
    }
    let units = b.bits(32)?;
    let scale = b.bits(32)?;
    plausible_rate(scale, units)
}

/// Parameter sets from an `avcC` or `hvcC` box body. Returns the NAL
/// length size too.
fn config_params(codec: Codec, cfg: &[u8], params: &mut Params) -> Option<usize> {
    match codec {
        Codec::Avc => {
            let nal_len = usize::from(cfg.get(4)? & 3) + 1;
            let mut pos = 5;
            for count_mask in [0x1F, 0xFF] {
                let n = cfg.get(pos)? & count_mask;
                pos += 1;
                for _ in 0..n {
                    let len =
                        usize::from(u16::from_be_bytes(cfg.get(pos..pos + 2)?.try_into().ok()?));
                    params.add(codec, cfg.get(pos + 2..pos + 2 + len)?);
                    pos += 2 + len;
                }
            }
            Some(nal_len)
        }
        Codec::Hevc => {
            let nal_len = usize::from(cfg.get(21)? & 3) + 1;
            let arrays = *cfg.get(22)?;
            let mut pos = 23;
            for _ in 0..arrays {
                pos += 1;
                let n = u16::from_be_bytes(cfg.get(pos..pos + 2)?.try_into().ok()?);
                pos += 2;
                for _ in 0..n {
                    let len =
                        usize::from(u16::from_be_bytes(cfg.get(pos..pos + 2)?.try_into().ok()?));
                    params.add(codec, cfg.get(pos + 2..pos + 2 + len)?);
                    pos += 2 + len;
                }
            }
            Some(nal_len)
        }
    }
}

// ------------------------------------------------------------ reference

/// Top-level boxes of a file: (type, offset, total size, header size).
fn top_boxes(src: &dyn Source) -> Vec<([u8; 4], u64, u64, u64)> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    let len = src.len();
    // `pos` saturates after a box claiming to run to 2^64; adding to it
    // would overflow.
    while pos.saturating_add(8) <= len && out.len() < 1024 {
        let mut h = [0u8; 16];
        let n = src.read_at(pos, &mut h).unwrap_or(0);
        if n < 8 {
            break;
        }
        let size32 = u32::from_be_bytes(h[..4].try_into().unwrap());
        let typ: [u8; 4] = h[4..8].try_into().unwrap();
        if !typ.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            break;
        }
        let (size, header) = match size32 {
            0 => (len - pos, 8),
            1 if n >= 16 => (u64::from_be_bytes(h[8..16].try_into().unwrap()), 16),
            1 => break,
            s => (u64::from(s), 8),
        };
        if size < header {
            break;
        }
        out.push((typ, pos, size, header));
        pos = pos.saturating_add(size);
    }
    out
}

impl Reference {
    /// Learns from a good MP4 or MOV with an H.264 or H.265 video track.
    pub fn from_video(src: &dyn Source) -> Option<Reference> {
        let boxes = top_boxes(src);
        let read = |off: u64, len: u64| {
            let mut v = vec![0u8; len as usize];
            (src.read_at(off, &mut v).ok()? == v.len()).then_some(v)
        };
        let ftyp = match boxes.iter().find(|b| &b.0 == b"ftyp") {
            Some(&(_, off, size, _)) if size <= 1024 => read(off, size)?,
            _ => Vec::new(),
        };
        let &(_, off, size, header) = boxes.iter().find(|b| &b.0 == b"moov")?;
        if size > MAX_MOOV || off + size > src.len() {
            return None;
        }
        let body = read(off + header, size - header)?;
        let moov = BoxNode {
            typ: *b"moov",
            body: Vec::new(),
            children: BoxNode::parse(&body, 1)?,
        };
        let movie_timescale = timescale(&moov.child(b"mvhd")?.body)?;
        let sound = moov
            .children
            .iter()
            .filter(|c| &c.typ == b"trak")
            .find_map(sound_of_track);
        let camera = camera::identify(&ftyp, Some(&body));

        for (i, trak) in moov.children.iter().enumerate() {
            if &trak.typ != b"trak" {
                continue;
            }
            let Some(mdia) = trak.child(b"mdia") else {
                continue;
            };
            let handler = mdia.child(b"hdlr").and_then(|h| h.body.get(8..12));
            if handler != Some(b"vide") {
                continue;
            }
            let Some(stbl) = mdia.child(b"minf").and_then(|m| m.child(b"stbl")) else {
                continue;
            };
            let Some(stsd) = stbl.child(b"stsd") else {
                continue;
            };
            // stsd: version/flags, count, then the first sample entry.
            let entry = stsd.body.get(8..)?;
            let codec_tag = entry.get(4..8)?;
            let codec = match codec_tag {
                b"avc1" | b"avc3" => Codec::Avc,
                b"hvc1" | b"hev1" => Codec::Hevc,
                _ => continue,
            };
            let cfg_tag: &[u8; 4] = if codec == Codec::Avc {
                b"avcC"
            } else {
                b"hvcC"
            };
            let entry_len = (be32(entry, 0)? as usize).min(entry.len());
            // Visual sample entries have 78 bytes of fields after the box
            // header; their child boxes follow.
            let children = BoxNode::parse(entry.get(86..entry_len)?, 8)?;
            let cfg = children.iter().find(|c| &c.typ == cfg_tag)?;
            let mut params = Params::default();
            let nal_len = config_params(codec, &cfg.body, &mut params)?;
            let timescale = timescale(&mdia.child(b"mdhd")?.body)?;
            let stts = &stbl.child(b"stts")?.body;
            let delta = be32(stts, 12).filter(|&d| d > 0)?;
            return Some(Reference {
                ftyp,
                moov: moov.clone(),
                trak: i,
                codec,
                nal_len,
                timescale,
                delta,
                movie_timescale,
                params,
                sound,
                camera,
            });
        }
        None
    }

    /// Learns the codec settings from the broken video's own stream, for
    /// cameras that repeat their parameter sets before keyframes; the
    /// camera's profile gives the frame rate (when the stream doesn't) and
    /// the sound format.
    pub fn from_stream(broken: &dyn Source) -> Option<Reference> {
        let (_, start, end) = media_range(broken)?;
        let mut head = vec![0u8; (64 << 10).min(broken.len()) as usize];
        let n = broken.read_at(0, &mut head).ok()?;
        head.truncate(n);
        let profile = camera::identify(&head, None);
        let p = profile.unwrap_or(&camera::GENERIC);

        let mut buf = vec![0u8; (PARAM_SEARCH).min(end - start) as usize];
        let n = broken.read_at(start, &mut buf).ok()?;
        buf.truncate(n);
        let (codec, nals) = find_params(&buf)?;
        let mut params = Params::default();
        for nal in &nals {
            params.add(codec, nal);
        }
        let format = params.format()?;
        let (timescale, delta) = format.timing.unwrap_or(p.frame_rates[0]);
        let entry = video_entry(codec, &nals, &format)?;
        let movie_timescale = 1000;

        let stbl = BoxNode {
            typ: *b"stbl",
            body: Vec::new(),
            children: vec![BoxNode::leaf(b"stsd", stsd(&entry))],
        };
        let minf = BoxNode {
            typ: *b"minf",
            body: Vec::new(),
            children: vec![
                BoxNode::leaf(b"vmhd", full_box(0, 1, &[0; 8])),
                dinf(),
                stbl,
            ],
        };
        let mdia = BoxNode {
            typ: *b"mdia",
            body: Vec::new(),
            children: vec![
                BoxNode::leaf(b"mdhd", mdhd(timescale)),
                BoxNode::leaf(b"hdlr", hdlr(b"vide", "VideoHandler")),
                minf,
            ],
        };
        let trak = BoxNode {
            typ: *b"trak",
            body: Vec::new(),
            children: vec![
                BoxNode::leaf(b"tkhd", tkhd(1, 0, format.width, format.height)),
                mdia,
            ],
        };
        let moov = BoxNode {
            typ: *b"moov",
            body: Vec::new(),
            children: vec![BoxNode::leaf(b"mvhd", mvhd(movie_timescale, 3)), trak],
        };
        let sound = p.audio.map(|a| match a.codec {
            AudioCodec::Aac => Sound::Aac {
                channels: a.channels,
                rate: None,
                usual_rate: a.sample_rate,
                trak: None,
            },
            AudioCodec::PcmBe16 => Sound::Pcm {
                unit_bytes: 2 * u32::from(a.channels),
                unit_delta: 1,
                timescale: a.sample_rate,
                trak: None,
                entry: audiofix::twos_entry(a.sample_rate, a.channels),
            },
        });
        Some(Reference {
            ftyp: camera::ftyp_box(p),
            moov,
            trak: 1,
            codec,
            nal_len: 4,
            timescale,
            delta,
            movie_timescale,
            params,
            sound,
            camera: profile,
        })
    }
}

/// How far into the media data parameter sets are looked for.
const PARAM_SEARCH: u64 = 32 << 20;

/// The first run of parameter sets in a stream of 4-byte length-prefixed
/// NAL units: the codec, and the units (SPS and PPS, and for H.265 the
/// VPS). The run must be followed by a picture or other video unit, which
/// random data that happens to look like a parameter set never is.
fn find_params(buf: &[u8]) -> Option<(Codec, Vec<Vec<u8>>)> {
    let len_at = |i: usize| -> Option<usize> {
        Some(u32::from_be_bytes(buf.get(i..i + 4)?.try_into().ok()?) as usize)
    };
    let mut i = 0;
    while i + 8 <= buf.len() {
        let h0 = buf[i + 4];
        let codec = if buf[i] == 0 && buf[i + 1] == 0 && matches!(h0, 0x27 | 0x47 | 0x67) {
            Codec::Avc
        } else if buf[i] == 0 && buf[i + 1] == 0 && h0 == 0x40 && buf[i + 5] == 0x01 {
            Codec::Hevc
        } else {
            i += 1;
            continue;
        };
        let mut nals: Vec<Vec<u8>> = Vec::new();
        let mut pos = i;
        let mut ok = false;
        while let Some(len) = len_at(pos) {
            let Some(nal) = buf
                .get(pos + 4..pos + 4 + len)
                .filter(|_| (2..=4096).contains(&len))
            else {
                break;
            };
            let Some(info) = nal_info(codec, nal) else {
                break;
            };
            let is_param = matches!(
                (codec, info.kind),
                (Codec::Avc, 7 | 8) | (Codec::Hevc, 32..=34)
            );
            if is_param {
                nals.push(nal.to_vec());
            } else {
                ok = !nals.is_empty();
                break;
            }
            pos += 4 + len;
        }
        if ok {
            let mut params = Params::default();
            for nal in &nals {
                params.add(codec, nal);
            }
            let complete = match codec {
                Codec::Avc => !params.avc_sps.is_empty() && !params.avc_pps.is_empty(),
                Codec::Hevc => !params.hevc_sps.is_empty() && !params.hevc_pps.is_empty(),
            };
            if complete && params.format().is_some() {
                return Some((codec, nals));
            }
        }
        i += 1;
    }
    None
}

/// The NAL unit type of a parameter set.
fn nal_type(codec: Codec, nal: &[u8]) -> u8 {
    match codec {
        Codec::Avc => nal[0] & 0x1F,
        Codec::Hevc => (nal[0] >> 1) & 0x3F,
    }
}

/// A visual sample entry (`avc1` or `hvc1`) with its decoder
/// configuration, built from the stream's parameter sets.
fn video_entry(codec: Codec, nals: &[Vec<u8>], f: &Format) -> Option<Vec<u8>> {
    let of = |t: u8| -> Vec<&Vec<u8>> {
        let mut seen = Vec::new();
        for n in nals.iter().filter(|n| nal_type(codec, n) == t) {
            if !seen.contains(&n) {
                seen.push(n);
            }
        }
        seen
    };
    let push_list = |cfg: &mut Vec<u8>, list: &[&Vec<u8>]| {
        for n in list {
            cfg.extend((n.len() as u16).to_be_bytes());
            cfg.extend(n.iter());
        }
    };
    let (tag, cfg_tag, cfg) = match codec {
        Codec::Avc => {
            let sps = of(7);
            let pps = of(8);
            let first = sps.first()?;
            let mut cfg = vec![1, *first.get(1)?, *first.get(2)?, *first.get(3)?, 0xFF];
            cfg.push(0xE0 | sps.len().min(31) as u8);
            push_list(&mut cfg, &sps[..sps.len().min(31)]);
            cfg.push(pps.len().min(255) as u8);
            push_list(&mut cfg, &pps[..pps.len().min(255)]);
            if !matches!(first[1], 66 | 77 | 88) {
                cfg.push(0xFC | (f.chroma as u8 & 3));
                cfg.push(0xF8 | (f.depth_luma.saturating_sub(8) as u8 & 7));
                cfg.push(0xF8 | (f.depth_chroma.saturating_sub(8) as u8 & 7));
                cfg.push(0);
            }
            (b"avc1", b"avcC", cfg)
        }
        Codec::Hevc => {
            let sps = of(33);
            let rbsp = Bits::new(sps.first()?.get(2..)?).d;
            let ptl = rbsp.get(..13)?;
            let sub_layers = (ptl[0] >> 1) & 7;
            let nested = ptl[0] & 1;
            let mut cfg = vec![1];
            cfg.extend(&ptl[1..13]);
            cfg.extend([0xF0, 0x00, 0xFC]);
            cfg.push(0xFC | (f.chroma as u8 & 3));
            cfg.push(0xF8 | (f.depth_luma.saturating_sub(8) as u8 & 7));
            cfg.push(0xF8 | (f.depth_chroma.saturating_sub(8) as u8 & 7));
            cfg.extend([0, 0]);
            cfg.push(((sub_layers + 1) << 3) | (nested << 2) | 3);
            let arrays: Vec<(u8, Vec<&Vec<u8>>)> = [32, 33, 34]
                .into_iter()
                .map(|t| (t, of(t)))
                .filter(|(_, l)| !l.is_empty())
                .collect();
            cfg.push(arrays.len() as u8);
            for (t, list) in arrays {
                cfg.push(0x80 | t);
                cfg.extend((list.len() as u16).to_be_bytes());
                push_list(&mut cfg, &list);
            }
            // `hvc1` plays in QuickTime and on iPhones; the parameter sets
            // repeated in the stream are the same ones.
            (b"hvc1", b"hvcC", cfg)
        }
    };
    let mut e = Vec::new();
    e.extend([0; 6]);
    e.extend(1u16.to_be_bytes());
    e.extend([0; 16]);
    e.extend((f.width.min(65535) as u16).to_be_bytes());
    e.extend((f.height.min(65535) as u16).to_be_bytes());
    e.extend(0x0048_0000u32.to_be_bytes());
    e.extend(0x0048_0000u32.to_be_bytes());
    e.extend([0; 4]);
    e.extend(1u16.to_be_bytes());
    e.extend([0; 32]);
    e.extend(0x0018u16.to_be_bytes());
    e.extend(0xFFFFu16.to_be_bytes());
    BoxNode::leaf(cfg_tag, cfg).serialize(&mut e);
    let mut out = Vec::new();
    BoxNode::leaf(tag, e).serialize(&mut out);
    Some(out)
}

/// An `stsd` body holding one sample entry box.
fn stsd(entry: &[u8]) -> Vec<u8> {
    let mut v = full_box(0, 0, &1u32.to_be_bytes());
    v.extend(entry);
    v
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn mvhd(timescale: u32, next_track: u32) -> Vec<u8> {
    let mut v = full_box(0, 0, &[0; 8]);
    v.extend(timescale.to_be_bytes());
    v.extend(0u32.to_be_bytes());
    v.extend(0x0001_0000u32.to_be_bytes());
    v.extend(0x0100u16.to_be_bytes());
    v.extend([0; 10]);
    for m in MATRIX {
        v.extend(m.to_be_bytes());
    }
    v.extend([0; 24]);
    v.extend(next_track.to_be_bytes());
    v
}

fn tkhd(id: u32, volume: u16, width: u32, height: u32) -> Vec<u8> {
    let mut v = full_box(0, 3, &[0; 8]);
    v.extend(id.to_be_bytes());
    v.extend([0; 4]);
    v.extend(0u32.to_be_bytes());
    v.extend([0; 8]);
    v.extend([0; 4]); // layer, alternate group
    v.extend(volume.to_be_bytes());
    v.extend([0; 2]);
    for m in MATRIX {
        v.extend(m.to_be_bytes());
    }
    v.extend((width << 16).to_be_bytes());
    v.extend((height << 16).to_be_bytes());
    v
}

fn mdhd(timescale: u32) -> Vec<u8> {
    let mut v = full_box(0, 0, &[0; 8]);
    v.extend(timescale.to_be_bytes());
    v.extend(0u32.to_be_bytes());
    v.extend(0x55C4u16.to_be_bytes()); // "und"
    v.extend([0; 2]);
    v
}

fn hdlr(kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut v = full_box(0, 0, &[0; 4]);
    v.extend(kind);
    v.extend([0; 12]);
    v.extend(name.as_bytes());
    v.push(0);
    v
}

/// A data reference saying the media is in this file.
fn dinf() -> BoxNode {
    let mut dref = Vec::new();
    dref.extend(28u32.to_be_bytes());
    dref.extend(b"dref");
    dref.extend(full_box(0, 0, &1u32.to_be_bytes()));
    dref.extend(12u32.to_be_bytes());
    dref.extend(b"url ");
    dref.extend([0, 0, 0, 1]);
    BoxNode::leaf(b"dinf", dref)
}

/// The sound format of a reference's track, if it is one this can rebuild.
fn sound_of_track(trak: &BoxNode) -> Option<Sound> {
    let mdia = trak.child(b"mdia")?;
    if mdia.child(b"hdlr").and_then(|h| h.body.get(8..12)) != Some(b"soun") {
        return None;
    }
    let stbl = mdia.child(b"minf")?.child(b"stbl")?;
    let entry = stbl.child(b"stsd")?.body.get(8..)?;
    match entry.get(4..8)? {
        b"mp4a" => {
            let (channels, rate) = audiofix::aac_config(entry)?;
            Some(Sound::Aac {
                channels,
                rate: Some(rate),
                usual_rate: rate,
                trak: Some(trak.clone()),
            })
        }
        b"twos" | b"sowt" | b"lpcm" | b"in24" | b"in32" | b"raw " => {
            // Follow the reference's own way of counting samples.
            let unit_bytes = be32(&stbl.child(b"stsz")?.body, 4).filter(|&s| s > 0)?;
            let unit_delta = be32(&stbl.child(b"stts")?.body, 12).filter(|&d| d > 0)?;
            let timescale = timescale(&mdia.child(b"mdhd")?.body)?;
            Some(Sound::Pcm {
                unit_bytes,
                unit_delta,
                timescale,
                trak: Some(trak.clone()),
                entry: Vec::new(),
            })
        }
        _ => None,
    }
}

// --------------------------------------------------------------- frames

#[derive(Debug, Clone)]
struct Frame {
    offset: u64,
    size: u32,
    sync: bool,
    /// Picture order count, made increasing across the whole video.
    poc: Option<i64>,
}

/// One NAL unit's header facts.
struct Nal {
    kind: u8,
    /// Starts a new frame if a frame is already open.
    starts_frame: bool,
    vcl: bool,
    sync: bool,
    ref_idc: u8,
}

fn nal_info(codec: Codec, h: &[u8]) -> Option<Nal> {
    match codec {
        Codec::Avc => {
            let b = *h.first()?;
            if b & 0x80 != 0 {
                return None;
            }
            let kind = b & 0x1F;
            let ref_idc = (b >> 5) & 3;
            let (vcl, starts) = match kind {
                1 | 5 => (true, h.get(1)? & 0x80 != 0), // first_mb_in_slice == 0
                6..=9 => (false, true),
                10..=12 => (false, false),
                _ => return None,
            };
            // IDR slices and parameter sets are always reference data.
            if (kind == 5 || kind == 7 || kind == 8) && ref_idc == 0 {
                return None;
            }
            Some(Nal {
                kind,
                starts_frame: starts,
                vcl,
                sync: kind == 5,
                ref_idc,
            })
        }
        Codec::Hevc => {
            let (b0, b1) = (*h.first()?, *h.get(1)?);
            if b0 & 0x80 != 0 || b1 & 7 == 0 || (b0 & 1) != 0 || (b1 >> 3) != 0 {
                return None; // forbidden bit, temporal ID, layer ID 0
            }
            let kind = (b0 >> 1) & 0x3F;
            let (vcl, starts) = match kind {
                0..=9 | 16..=21 => (true, h.get(2)? & 0x80 != 0),
                32..=35 | 39 => (false, true),
                36 | 37 | 38 | 40 => (false, false),
                _ => return None,
            };
            Some(Nal {
                kind,
                starts_frame: starts,
                vcl,
                sync: (16..=21).contains(&kind),
                ref_idc: 1,
            })
        }
    }
}

struct Walker<'a> {
    src: &'a dyn Source,
    codec: Codec,
    nal_len: usize,
    end: u64,
    params: Params,
    // POC state.
    prev_msb: i64,
    prev_lsb: i64,
    base: i64,
    max_poc: i64,
    /// H.264: frame number of the last reference picture.
    prev_ref_frame_num: Option<u32>,
    /// A window of the file: walking reads a few bytes at a time, often
    /// one byte apart while looking past sound data.
    cache: std::cell::RefCell<(u64, Vec<u8>)>,
}

/// Size of the read window.
const WINDOW: usize = 1 << 20;

impl Walker<'_> {
    fn read(&self, at: u64, n: usize) -> Option<Vec<u8>> {
        if at.checked_add(n as u64)? > self.end {
            return None;
        }
        let mut cache = self.cache.borrow_mut();
        let (start, buf) = &mut *cache;
        let inside = at >= *start && at + n as u64 <= *start + buf.len() as u64;
        if !inside {
            if n > WINDOW / 2 {
                let mut v = vec![0u8; n];
                return (self.src.read_at(at, &mut v).ok()? == n).then_some(v);
            }
            let want = (WINDOW as u64).min(self.end - at) as usize;
            buf.resize(want, 0);
            let got = self.src.read_at(at, buf).ok()?;
            buf.truncate(got);
            *start = at;
            if got < n {
                return None;
            }
        }
        let i = (at - *start) as usize;
        Some(buf[i..i + n].to_vec())
    }

    /// The NAL unit at `at`: its total size (with length prefix) and header.
    fn nal_at(&self, at: u64) -> Option<(u64, Nal)> {
        let h = self.read(at, self.nal_len + 3)?;
        let len = h[..self.nal_len]
            .iter()
            .fold(0u64, |a, &b| (a << 8) | u64::from(b));
        if !(2..=MAX_NAL).contains(&len) || at + self.nal_len as u64 + len > self.end {
            return None;
        }
        let nal = nal_info(self.codec, &h[self.nal_len..])?;
        Some((self.nal_len as u64 + len, nal))
    }

    /// A whole frame starting at `at`, if one does.
    fn frame_at(&mut self, at: u64) -> Option<Frame> {
        let mut pos = at;
        let mut have_vcl = false;
        let mut sync = false;
        let mut first_vcl: Option<(u64, u64, u8, u8)> = None;
        let mut params_in_frame = Vec::new();
        while let Some((size, nal)) = self.nal_at(pos) {
            if pos == at && !nal.starts_frame {
                return None;
            }
            if pos > at && nal.starts_frame && have_vcl {
                break;
            }
            if nal.vcl {
                if !have_vcl {
                    first_vcl = Some((pos, size, nal.kind, nal.ref_idc));
                }
                have_vcl = true;
                sync |= nal.sync;
            } else if matches!(
                (self.codec, nal.kind),
                (Codec::Avc, 7 | 8) | (Codec::Hevc, 32..=34)
            ) {
                params_in_frame.push((pos, size));
            }
            pos += size;
            if pos - at > MAX_FRAME {
                return None;
            }
        }
        if !have_vcl {
            return None;
        }
        // Parameter sets sent with the stream replace the reference's.
        for (p, s) in params_in_frame {
            if let Some(nal) =
                self.read(p + self.nal_len as u64, (s - self.nal_len as u64) as usize)
            {
                self.params.add(self.codec, &nal);
            }
        }
        let (vp, vs, kind, ref_idc) = first_vcl?;
        let slice = self.read(
            vp + self.nal_len as u64,
            ((vs - self.nal_len as u64) as usize).min(64),
        )?;
        let poc = self.poc(&slice, kind, ref_idc)?;
        Some(Frame {
            offset: at,
            size: (pos - at) as u32,
            sync,
            poc,
        })
    }

    /// Reads the slice header far enough to know the picture order count.
    /// `None` means the slice header is invalid (so this is not a frame);
    /// `Some(None)` means the order could not be worked out.
    fn poc(&mut self, nal: &[u8], kind: u8, ref_idc: u8) -> Option<Option<i64>> {
        match self.codec {
            Codec::Avc => {
                let mut b = Bits::new(&nal[1..]);
                let _first_mb = b.ue()?;
                let slice_type = b.ue()?;
                // An IDR picture is all intra slices (I or SI). Sound data
                // that happens to look like an IDR NAL unit seldom is.
                if slice_type > 9 || (kind == 5 && !matches!(slice_type % 5, 2 | 4)) {
                    return None;
                }
                let pps = b.ue()?;
                let Some(sps) = self
                    .params
                    .avc_pps
                    .get(&pps)
                    .and_then(|s| self.params.avc_sps.get(s))
                    .cloned()
                else {
                    // A picture naming a parameter set that doesn't exist
                    // is not one, when the stream has any.
                    return self.params.avc_pps.is_empty().then_some(None);
                };
                if sps.separate_colour_plane {
                    b.skip(2);
                }
                let frame_num = b.bits(sps.log2_max_frame_num)?;
                let idr = kind == 5;
                // Each picture's frame number is one more than the last
                // reference picture's (zero for an IDR picture). Sound data
                // that happens to look like a slice rarely agrees.
                if idr {
                    if frame_num != 0 {
                        return None;
                    }
                } else if let Some(last) = self.prev_ref_frame_num {
                    if frame_num != (last + 1) % (1 << sps.log2_max_frame_num) {
                        return None;
                    }
                }
                if idr || ref_idc != 0 {
                    self.prev_ref_frame_num = Some(frame_num);
                }
                if !sps.frame_mbs_only && b.bit()? == 1 {
                    b.skip(1);
                }
                if idr {
                    b.ue()?;
                }
                if sps.poc_type != 0 {
                    // Types 1 and 2: in practice, decode order is display order.
                    return Some(None);
                }
                let lsb = i64::from(b.bits(sps.log2_max_poc_lsb)?);
                let max = 1i64 << sps.log2_max_poc_lsb;
                Some(Some(self.order(lsb, max, idr, ref_idc != 0)))
            }
            Codec::Hevc => {
                let mut b = Bits::new(&nal[2..]);
                b.bit()?; // first_slice_segment_in_pic_flag (set: we read the first slice)
                if (16..=23).contains(&kind) {
                    b.skip(1);
                }
                let pps_id = b.ue()?;
                let Some(pps) = self.params.hevc_pps.get(&pps_id).cloned() else {
                    return self.params.hevc_pps.is_empty().then_some(None);
                };
                let Some(sps) = self.params.hevc_sps.get(&pps.sps).cloned() else {
                    return self.params.hevc_sps.is_empty().then_some(None);
                };
                b.skip(pps.extra_bits as usize);
                if b.ue()? > 2 {
                    return None;
                }
                if pps.output_flag {
                    b.skip(1);
                }
                if sps.separate_colour_plane {
                    b.skip(2);
                }
                let idr = kind == 19 || kind == 20;
                let max = 1i64 << sps.log2_max_poc_lsb;
                let lsb = if idr {
                    0
                } else {
                    i64::from(b.bits(sps.log2_max_poc_lsb)?)
                };
                // IDR and BLA pictures restart the count, as does a CRA
                // picture the stream starts with. RASL, RADL and sub-layer
                // non-reference pictures don't anchor it.
                let reset = (16..=20).contains(&kind) || (kind == 21 && self.max_poc < 0);
                let anchor = !(6..=9).contains(&kind) && !(kind < 16 && kind.is_multiple_of(2));
                Some(Some(self.order(lsb, max, reset, anchor)))
            }
        }
    }

    /// Picture order count from its low bits, kept increasing across
    /// resets so frames of the whole video sort into display order.
    fn order(&mut self, lsb: i64, max: i64, reset: bool, anchor: bool) -> i64 {
        if reset {
            self.base = self.max_poc + 1;
            self.prev_msb = 0;
            self.prev_lsb = 0;
        }
        let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        if anchor || reset {
            self.prev_msb = msb;
            self.prev_lsb = lsb;
        }
        let poc = self.base + msb + lsb;
        self.max_poc = self.max_poc.max(poc);
        poc
    }
}

/// Where the media data of a broken video is: from the start of its `mdat`
/// payload to the end of the box or the file, whichever comes first.
fn media_range(src: &dyn Source) -> Option<(Vec<u8>, u64, u64)> {
    let mut ftyp = Vec::new();
    for (typ, off, size, header) in top_boxes(src) {
        match &typ {
            b"ftyp" if size <= 1024 => {
                let mut v = vec![0u8; size as usize];
                if src.read_at(off, &mut v).ok()? == v.len() {
                    ftyp = v;
                }
            }
            b"mdat" => {
                let end = off.saturating_add(size).min(src.len());
                return Some((ftyp, off + header, end));
            }
            b"moov" | b"free" | b"skip" | b"wide" | b"uuid" | b"pnot" | b"udta" | b"meta" => {}
            _ => return None,
        }
    }
    None
}

/// Rebuilds the index of `broken`, a video whose `moov` is missing or
/// unusable, using `reference` from the same camera.
pub fn rebuild(broken: &dyn Source, reference: &Reference) -> Option<Rebuilt> {
    let (ftyp, start, end) = media_range(broken)?;
    let mut w = Walker {
        src: broken,
        codec: reference.codec,
        nal_len: reference.nal_len,
        end,
        params: reference.params.clone(),
        prev_msb: 0,
        prev_lsb: 0,
        base: 0,
        max_poc: -1,
        prev_ref_frame_num: None,
        cache: Default::default(),
    };

    let mut frames: Vec<Frame> = Vec::new();
    // Stretches between video frames: sound and metadata chunks.
    let mut gaps: Vec<(u64, u64)> = Vec::new();
    let mut pos = start;
    let mut last_end = start;
    while pos < end && frames.len() < MAX_FRAMES {
        if let Some(f) = w.frame_at(pos) {
            if f.offset > last_end {
                gaps.push((last_end, f.offset));
            }
            pos = f.offset + u64::from(f.size);
            last_end = pos;
            frames.push(f);
            continue;
        }
        // Not a frame: sound or metadata interleaved with the video, or the
        // end of the recording. Look for the next frame.
        if pos - last_end > MAX_GAP {
            break;
        }
        pos += 1;
    }
    // A video starts with a frame the decoder can begin at.
    let first = frames.iter().position(|f| f.sync)?;
    frames.drain(..first);
    if frames.is_empty() {
        return None;
    }
    let data_start = start;
    let mut data_end = last_end;
    // Sound recorded after the last video frame.
    if let Some(Sound::Aac { channels, .. }) = &reference.sound {
        let to = end.min(last_end + audiofix::MAX_CHUNK as u64);
        let mut tail = vec![0u8; (to - last_end) as usize];
        if !tail.is_empty()
            && broken.read_at(last_end, &mut tail).ok() == Some(tail.len())
            && audiofix::split_aac(&tail, *channels, 0).is_some()
        {
            gaps.push((last_end, to));
            data_end = to;
        }
    }

    // Composition offsets from the picture order: frames sorted by POC are
    // shown one frame duration apart.
    let delta = reference.delta;
    let n = frames.len();
    // `shift`: how many frames the first frame shown is delayed by
    // reordering; the edit list skips that delay so the video starts at 0.
    let (offsets, shift): (Option<Vec<i64>>, i64) = if frames.iter().all(|f| f.poc.is_some()) {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| (frames[i].poc, i));
        let mut shown = vec![0i64; n];
        for (rank, &i) in order.iter().enumerate() {
            shown[i] = rank as i64;
        }
        let shift = (0..n)
            .map(|i| i as i64 - shown[i])
            .max()
            .unwrap_or(0)
            .max(0);
        let offs: Vec<i64> = (0..n)
            .map(|i| (shown[i] + shift - i as i64) * i64::from(delta))
            .collect();
        if offs.iter().any(|&o| o != offs[0]) {
            (Some(offs), shift * i64::from(delta))
        } else {
            (None, 0)
        }
    } else {
        (None, 0)
    };

    let mdat_large = data_end - data_start + 8 > u64::from(u32::MAX);
    let mut prefix = if ftyp.is_empty() {
        reference.ftyp.clone()
    } else {
        ftyp
    };
    if mdat_large {
        prefix.extend(1u32.to_be_bytes());
        prefix.extend(b"mdat");
        prefix.extend((data_end - data_start + 16).to_be_bytes());
    } else {
        prefix.extend(((data_end - data_start + 8) as u32).to_be_bytes());
        prefix.extend(b"mdat");
    }
    let base = prefix.len() as u64;

    // The new sample tables.
    let mut stts = full_box(0, 0, &1u32.to_be_bytes());
    stts.extend((n as u32).to_be_bytes());
    stts.extend(delta.to_be_bytes());
    let mut stsc = full_box(0, 0, &1u32.to_be_bytes());
    stsc.extend([0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);
    let mut stsz = full_box(0, 0, &0u32.to_be_bytes());
    stsz.extend((n as u32).to_be_bytes());
    let mut co64 = full_box(0, 0, &(n as u32).to_be_bytes());
    for f in &frames {
        stsz.extend(f.size.to_be_bytes());
        co64.extend((base + f.offset - data_start).to_be_bytes());
    }
    let syncs: Vec<u32> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.sync)
        .map(|(i, _)| i as u32 + 1)
        .collect();

    let media_duration = n as u64 * u64::from(delta);
    let movie_duration = media_duration * u64::from(reference.movie_timescale)
        / u64::from(reference.timescale.max(1));

    let mut moov = reference.moov.clone();
    let video = moov.children.get(reference.trak)?.clone();
    moov.children.retain(|c| &c.typ != b"trak");
    let mut trak = video;
    trak.children.retain(|c| &c.typ != b"tref");
    if let Some(tkhd) = trak.child_mut(b"tkhd") {
        set_duration(&mut tkhd.body, b"tkhd", movie_duration);
    }
    let mut elst = full_box(0, 0, &1u32.to_be_bytes());
    elst.extend((movie_duration.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    elst.extend((shift as u32).to_be_bytes());
    elst.extend([0, 1, 0, 0]);
    trak.children.retain(|c| &c.typ != b"edts");
    let edts = BoxNode {
        typ: *b"edts",
        body: Vec::new(),
        children: vec![BoxNode::leaf(b"elst", elst)],
    };
    let at = trak
        .children
        .iter()
        .position(|c| &c.typ == b"mdia")
        .unwrap_or(trak.children.len());
    trak.children.insert(at, edts);
    {
        let mdia = trak.child_mut(b"mdia")?;
        if let Some(mdhd) = mdia.child_mut(b"mdhd") {
            set_duration(&mut mdhd.body, b"mdhd", media_duration);
        }
        let stbl = mdia.child_mut(b"minf")?.child_mut(b"stbl")?;
        let stsd = stbl.child(b"stsd")?.clone();
        let mut children = vec![stsd, BoxNode::leaf(b"stts", stts)];
        if let Some(offs) = &offsets {
            // Version 1 allows negative offsets, but players handle the
            // shifted, non-negative form with an edit list best.
            let mut ctts = full_box(0, 0, &[]);
            let mut runs: Vec<(u32, i64)> = Vec::new();
            for &o in offs {
                match runs.last_mut() {
                    Some((c, v)) if *v == o => *c += 1,
                    _ => runs.push((1, o)),
                }
            }
            ctts.extend((runs.len() as u32).to_be_bytes());
            for (c, v) in runs {
                ctts.extend(c.to_be_bytes());
                ctts.extend((v.max(0) as u32).to_be_bytes());
            }
            children.push(BoxNode::leaf(b"ctts", ctts));
        }
        if syncs.len() < n {
            let mut stss = full_box(0, 0, &(syncs.len() as u32).to_be_bytes());
            for s in &syncs {
                stss.extend(s.to_be_bytes());
            }
            children.push(BoxNode::leaf(b"stss", stss));
        }
        children.push(BoxNode::leaf(b"stsc", stsc));
        children.push(BoxNode::leaf(b"stsz", stsz));
        children.push(BoxNode::leaf(b"co64", co64));
        stbl.children = children;
    }
    let at = moov
        .children
        .iter()
        .position(|c| &c.typ == b"mvhd")
        .map_or(0, |i| i + 1);
    moov.children.insert(at, trak);
    let seconds = media_duration as f64 / f64::from(reference.timescale.max(1));
    let mut sound_samples = 0;
    if let Some(sound) = &reference.sound {
        let place = |off: u64| base + off - data_start;
        if let Some((trak, samples)) = sound_track(
            broken,
            &gaps,
            sound,
            seconds,
            &place,
            reference.movie_timescale,
        ) {
            moov.children.insert(at + 1, trak);
            sound_samples = samples;
        }
    }
    if let Some(mvhd) = moov.child_mut(b"mvhd") {
        set_duration(&mut mvhd.body, b"mvhd", movie_duration);
    }
    let mut moov_bytes = Vec::new();
    moov.serialize(&mut moov_bytes);

    Some(Rebuilt {
        prefix,
        data: (data_start, data_end),
        moov: moov_bytes,
        frames: n,
        seconds,
        sound_samples,
    })
}

/// The sound chunks among the stretches between video frames, as a track.
/// Returns the track and its number of samples.
fn sound_track(
    broken: &dyn Source,
    gaps: &[(u64, u64)],
    sound: &Sound,
    seconds: f64,
    place: &dyn Fn(u64) -> u64,
    movie_timescale: u32,
) -> Option<(BoxNode, usize)> {
    // Each chunk: where it is and the sizes of its samples (for PCM, one
    // entry holding the number of units).
    let mut chunks: Vec<(u64, Vec<u32>)> = Vec::new();
    // Where each AAC chunk's sound lies, to look at it again.
    let mut aac_spans: Vec<(u64, usize)> = Vec::new();
    for &(a, b) in gaps {
        let len = b - a;
        if len == 0 || len > audiofix::MAX_CHUNK as u64 {
            continue;
        }
        let mut data = vec![0u8; len as usize];
        if broken.read_at(a, &mut data).ok()? != data.len() {
            continue;
        }
        match sound {
            Sound::Aac { channels, .. } => {
                // A chunk may be sound followed by telemetry (GoPro writes
                // its GPMF records right after the sound).
                let sound_len = memchr::memmem::find_iter(&data, b"DEVC")
                    .find(|&q| q > 0 && camera::is_metadata_chunk(&data[q..]))
                    .unwrap_or(data.len());
                if let Some(sizes) = audiofix::split_aac(&data[..sound_len], *channels, 0) {
                    chunks.push((a, sizes));
                    aac_spans.push((a, sound_len));
                }
            }
            Sound::Pcm { unit_bytes, .. } => {
                if len % u64::from(*unit_bytes) == 0 && !camera::is_metadata_chunk(&data) {
                    chunks.push((a, vec![(len / u64::from(*unit_bytes)) as u32]));
                }
            }
        }
    }
    if chunks.is_empty() {
        return None;
    }
    // A false boundary inside a frame makes odd-sized pieces. Now that
    // the usual frame size of this recording is known, split again,
    // preferring pieces near it.
    if let Sound::Aac { channels, .. } = sound {
        let mut all: Vec<u32> = chunks.iter().flat_map(|c| c.1.iter().copied()).collect();
        if all.len() >= 16 {
            all.sort_unstable();
            let usual = all[all.len() / 2] as usize;
            for (chunk, &(a, n)) in chunks.iter_mut().zip(&aac_spans) {
                let mut data = vec![0u8; n];
                if broken.read_at(a, &mut data).ok() != Some(n) {
                    continue;
                }
                if let Some(sizes) = audiofix::split_aac(&data, *channels, usual) {
                    chunk.1 = sizes;
                }
            }
        }
    }

    let (timescale, entry, template) = match sound {
        Sound::Aac {
            channels,
            rate,
            usual_rate,
            trak,
        } => {
            let frames: usize = chunks.iter().map(|c| c.1.len()).sum();
            let rate = rate.unwrap_or_else(|| audiofix::measure_rate(frames, seconds, *usual_rate));
            (rate, audiofix::mp4a_entry(rate, *channels), trak.as_ref())
        }
        Sound::Pcm {
            timescale,
            trak,
            entry,
            ..
        } => (*timescale, entry.clone(), trak.as_ref()),
    };

    let mut stts = full_box(0, 0, &1u32.to_be_bytes());
    let mut stsz;
    let samples: usize;
    let mut runs: Vec<(u32, u32, u32)> = Vec::new(); // first chunk, samples per chunk, entry
    let per_chunk = |c: &(u64, Vec<u32>)| match sound {
        Sound::Aac { .. } => c.1.len() as u32,
        Sound::Pcm { .. } => c.1[0],
    };
    for (i, c) in chunks.iter().enumerate() {
        let k = per_chunk(c);
        if runs.last().is_none_or(|r| r.1 != k) {
            runs.push((i as u32 + 1, k, 1));
        }
    }
    let media_duration: u64 = match sound {
        Sound::Aac { .. } => {
            samples = chunks.iter().map(|c| c.1.len()).sum();
            stts.extend((samples as u32).to_be_bytes());
            stts.extend(1024u32.to_be_bytes());
            stsz = full_box(0, 0, &0u32.to_be_bytes());
            stsz.extend((samples as u32).to_be_bytes());
            for c in &chunks {
                for s in &c.1 {
                    stsz.extend(s.to_be_bytes());
                }
            }
            samples as u64 * 1024
        }
        Sound::Pcm {
            unit_bytes,
            unit_delta,
            ..
        } => {
            samples = chunks.iter().map(|c| c.1[0] as usize).sum();
            stts.extend((samples as u32).to_be_bytes());
            stts.extend(unit_delta.to_be_bytes());
            stsz = full_box(0, 0, &unit_bytes.to_be_bytes());
            stsz.extend((samples as u32).to_be_bytes());
            samples as u64 * u64::from(*unit_delta)
        }
    };
    let mut stsc = full_box(0, 0, &(runs.len() as u32).to_be_bytes());
    for (first, k, e) in runs {
        stsc.extend(first.to_be_bytes());
        stsc.extend(k.to_be_bytes());
        stsc.extend(e.to_be_bytes());
    }
    let mut co64 = full_box(0, 0, &(chunks.len() as u32).to_be_bytes());
    for c in &chunks {
        co64.extend(place(c.0).to_be_bytes());
    }
    let movie_duration = media_duration * u64::from(movie_timescale) / u64::from(timescale.max(1));

    let mut trak = match template {
        Some(t) => t.clone(),
        None => BoxNode {
            typ: *b"trak",
            body: Vec::new(),
            children: vec![
                BoxNode::leaf(b"tkhd", tkhd(2, 0x0100, 0, 0)),
                BoxNode {
                    typ: *b"mdia",
                    body: Vec::new(),
                    children: vec![
                        BoxNode::leaf(b"mdhd", mdhd(timescale)),
                        BoxNode::leaf(b"hdlr", hdlr(b"soun", "SoundHandler")),
                        BoxNode {
                            typ: *b"minf",
                            body: Vec::new(),
                            children: vec![
                                BoxNode::leaf(b"smhd", full_box(0, 0, &[0; 4])),
                                dinf(),
                                BoxNode {
                                    typ: *b"stbl",
                                    body: Vec::new(),
                                    children: vec![BoxNode::leaf(b"stsd", stsd(&entry))],
                                },
                            ],
                        },
                    ],
                },
            ],
        },
    };
    // The reference's edit list and track references describe its own
    // recording.
    trak.children
        .retain(|c| !matches!(&c.typ, b"edts" | b"tref"));
    if let Some(tkhd) = trak.child_mut(b"tkhd") {
        set_duration(&mut tkhd.body, b"tkhd", movie_duration);
    }
    let mdia = trak.child_mut(b"mdia")?;
    if let Some(mdhd) = mdia.child_mut(b"mdhd") {
        set_duration(&mut mdhd.body, b"mdhd", media_duration);
    }
    let stbl = mdia.child_mut(b"minf")?.child_mut(b"stbl")?;
    let stsd_box = stbl.child(b"stsd")?.clone();
    stbl.children = vec![
        stsd_box,
        BoxNode::leaf(b"stts", stts),
        BoxNode::leaf(b"stsc", stsc),
        BoxNode::leaf(b"stsz", stsz),
        BoxNode::leaf(b"co64", co64),
    ];
    Some((trak, samples))
}

/// How long a video with an index runs, in seconds, from its movie header.
pub fn duration_secs(src: &dyn Source) -> Option<f64> {
    let &(_, off, size, header) = top_boxes(src).iter().find(|b| &b.0 == b"moov")?;
    if size > MAX_MOOV || off.checked_add(size)? > src.len() {
        return None;
    }
    let mut body = vec![0u8; (size - header) as usize];
    if src.read_at(off + header, &mut body).ok()? != body.len() {
        return None;
    }
    let mvhd = BoxNode::parse(&body, 1)?
        .into_iter()
        .find(|b| &b.typ == b"mvhd")?
        .body;
    let scale = timescale(&mvhd)?;
    let v1 = mvhd.first() == Some(&1);
    let dur = if v1 {
        u64::from_be_bytes(mvhd.get(24..32)?.try_into().ok()?)
    } else {
        u64::from(be32(&mvhd, 16)?)
    };
    (scale > 0).then(|| dur as f64 / f64::from(scale))
}

/// Whether `src` looks like a video whose frames are there but whose index
/// is missing: an `ftyp` and `mdat` with no usable `moov`.
pub fn needs_index(src: &dyn Source) -> bool {
    let boxes = top_boxes(src);
    let has_mdat = boxes.iter().any(|b| &b.0 == b"mdat");
    let moov_ok = boxes
        .iter()
        .any(|b| &b.0 == b"moov" && b.1.saturating_add(b.2) <= src.len());
    has_mdat && !moov_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
    }

    #[test]
    fn removes_emulation_prevention() {
        let b = Bits::new(&[0, 0, 3, 1, 0, 0, 3, 0]);
        assert_eq!(b.d, vec![0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn boxes_round_trip() {
        let mut data = Vec::new();
        let leaf = BoxNode::leaf(b"stsd", vec![1, 2, 3]);
        let tree = BoxNode {
            typ: *b"stbl",
            body: vec![],
            children: vec![leaf],
        };
        tree.serialize(&mut data);
        let back = BoxNode::parse(&data, 0).unwrap();
        assert_eq!(back[0].children[0].body, vec![1, 2, 3]);
    }

    #[test]
    fn rejects_garbage() {
        let junk: Vec<u8> = (0..100_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert!(Reference::from_video(&junk).is_none());
        assert!(media_range(&junk).is_none());
    }
}
