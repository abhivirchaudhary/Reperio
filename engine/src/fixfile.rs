//! Repairing one damaged file, whatever it is: the entry point for
//! `reperio repair`. The kind of file is told from its content, falling
//! back on its extension when the start is what was lost.

use std::io::{self, Write};

use crate::docfix::{self, DocRepair};
use crate::photofix::{self, PhotoRepair};
use crate::source::Source;
use crate::videofix;

/// Largest photo or document repaired (they are rebuilt in memory).
pub const MAX_IN_MEMORY: u64 = 512 << 20;

pub enum Repaired {
    Video {
        rebuilt: videofix::Rebuilt,
        /// The camera recognised, if any.
        camera: Option<&'static str>,
        /// Rebuilt with a reference video rather than from the stream.
        guided: bool,
    },
    Photo {
        data: Vec<u8>,
        repair: PhotoRepair,
    },
    Document {
        data: Vec<u8>,
        /// Text read from a part that could not be rebuilt, to save beside it.
        text: Option<String>,
        repair: DocRepair,
    },
}

impl Repaired {
    /// Extension of the repaired file, when it differs from the damaged
    /// one's (a rescued preview is a JPEG, salvaged text is text).
    pub fn ext(&self) -> Option<&'static str> {
        match self {
            Repaired::Video { .. } => None,
            Repaired::Photo { repair, .. } => repair.ext(),
            Repaired::Document { repair, .. } => repair.ext(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Repaired::Video {
                rebuilt,
                camera,
                guided,
            } => {
                let how = match (guided, camera) {
                    (true, _) => "using the reference video".to_string(),
                    (false, Some(c)) => format!("from the stream's own settings ({c})"),
                    (false, None) => "from the stream's own settings".to_string(),
                };
                let sound = if rebuilt.sound_samples > 0 {
                    "with sound"
                } else {
                    "without sound (it could not be found)"
                };
                format!(
                    "Repaired {how}: {} frames, {:.1} seconds of video, {sound}.",
                    rebuilt.frames, rebuilt.seconds
                )
            }
            Repaired::Photo { repair, .. } => repair.describe().to_string(),
            Repaired::Document { repair, .. } => repair.describe().to_string(),
        }
    }

    /// Writes the repaired file. Returns the number of unreadable sectors
    /// met (written as zeros).
    pub fn write_to(&self, src: &dyn Source, out: &mut impl Write) -> io::Result<u64> {
        match self {
            Repaired::Video { rebuilt, .. } => rebuilt.write_to(src, out),
            Repaired::Photo { data, .. } | Repaired::Document { data, .. } => {
                out.write_all(data)?;
                Ok(0)
            }
        }
    }
}

/// What kind of file `head` starts, by content, then by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Video,
    Jpeg,
    Heic,
    Zip,
    Pdf,
    Ole,
}

pub fn kind_of(head: &[u8], ext: &str) -> Option<FileKind> {
    let brand = head.get(8..12).filter(|_| head.get(4..8) == Some(b"ftyp"));
    if matches!(
        brand,
        Some(b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis")
    ) {
        return Some(FileKind::Heic);
    }
    if brand.is_some() || head.get(4..8) == Some(b"mdat") {
        return Some(FileKind::Video);
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        return Some(FileKind::Jpeg);
    }
    if head.starts_with(b"PK\x03\x04") {
        return Some(FileKind::Zip);
    }
    if head.starts_with(b"%PDF-") {
        return Some(FileKind::Pdf);
    }
    if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Some(FileKind::Ole);
    }
    match ext {
        "mp4" | "mov" | "m4v" | "3gp" | "lrf" | "lrv" => Some(FileKind::Video),
        "jpg" | "jpeg" | "jpe" => Some(FileKind::Jpeg),
        "heic" | "heif" => Some(FileKind::Heic),
        "docx" | "xlsx" | "pptx" | "zip" | "odt" | "ods" | "odp" => Some(FileKind::Zip),
        "pdf" => Some(FileKind::Pdf),
        "doc" => Some(FileKind::Ole),
        _ => None,
    }
}

/// Repairs `src`. `reference` is a good file from the same camera, for
/// videos and for JPEGs whose header is lost.
pub fn repair(
    src: &dyn Source,
    ext: &str,
    reference: Option<&dyn Source>,
) -> Result<Repaired, String> {
    let mut head = vec![0u8; 4096.min(src.len()) as usize];
    let n = src.read_at(0, &mut head).map_err(|e| e.to_string())?;
    head.truncate(n);
    let kind = kind_of(&head, ext).ok_or("not a kind of file Reperio can repair")?;
    if kind == FileKind::Video {
        return repair_video(src, reference);
    }
    if src.len() > MAX_IN_MEMORY {
        return Err("too large to repair".into());
    }
    let data = read_all(src).map_err(|e| e.to_string())?;
    let reference = match reference {
        Some(r) if r.len() <= MAX_IN_MEMORY => Some(read_all(r).map_err(|e| e.to_string())?),
        _ => None,
    };
    match kind {
        FileKind::Jpeg | FileKind::Heic => {
            let refs: Vec<&[u8]> = reference.as_deref().into_iter().collect();
            let (data, repair) =
                photofix::repair(&data, &refs).ok_or("nothing in this photo could be rescued")?;
            Ok(Repaired::Photo { data, repair })
        }
        _ => {
            let fixed =
                docfix::repair(&data, ext).ok_or("nothing in this document could be rescued")?;
            Ok(Repaired::Document {
                data: fixed.data,
                text: fixed.text,
                repair: fixed.repair,
            })
        }
    }
}

fn repair_video(src: &dyn Source, reference: Option<&dyn Source>) -> Result<Repaired, String> {
    if let Some(r) = reference {
        let guide = videofix::Reference::from_video(r)
            .ok_or("the reference is not an MP4 or MOV with H.264 or H.265 video that plays")?;
        let rebuilt = videofix::rebuild(src, &guide)
            .ok_or("no video frames that match the reference were found")?;
        return Ok(Repaired::Video {
            rebuilt,
            camera: guide.camera.map(|c| c.name),
            guided: true,
        });
    }
    let guide = videofix::Reference::from_stream(src).ok_or(
        "the video's settings are not repeated in its stream; give a good video from the same camera with -r",
    )?;
    let rebuilt = videofix::rebuild(src, &guide).ok_or("no video frames were found")?;
    Ok(Repaired::Video {
        rebuilt,
        camera: guide.camera.map(|c| c.name),
        guided: false,
    })
}

/// Reads a whole source, filling unreadable sectors with zeros.
pub fn read_all(src: &dyn Source) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; src.len() as usize];
    let mut pos = 0;
    while pos < data.len() {
        let (n, _) = crate::source::read_tolerant(src, pos as u64, &mut data[pos..]);
        if n == 0 {
            break;
        }
        pos += n;
    }
    data.truncate(pos);
    Ok(data)
}
