//! File format recognisers.
//!
//! Each format answers two questions about a candidate offset: does a file of
//! mine start here (`matches`, a cheap check on the first bytes), and if so,
//! where does it end (`carve`, which walks the file's own structure). Walking
//! the structure, instead of searching for a footer, is what lets us recover
//! files at their exact size and reject random data that only looks like a
//! header.

mod asf;
pub(crate) mod bmff;
mod bmp;
pub(crate) mod crc;
pub(crate) mod custom;
mod delimited;
mod ebml;
mod flac;
mod font;
mod gif;
pub(crate) mod jpeg;
mod mp3;
mod mts;
mod mxf;
mod ole;
mod pdf;
mod plaintext;
mod png;
mod psd;
mod pst;
mod raf;
mod rar;
mod riff;
mod sevenzip;
mod sqlite;
pub(crate) mod tiff;
mod zip;

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::reader::Reader;

/// Bytes of context passed to `matches`.
pub const HEAD_LEN: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Photo,
    Raw,
    Video,
    Audio,
    Document,
    Other,
}

impl Kind {
    pub fn folder(self) -> &'static str {
        match self {
            Kind::Photo => "Photos",
            Kind::Raw => "RAW Photos",
            Kind::Video => "Videos",
            Kind::Audio => "Audio",
            Kind::Document => "Documents",
            Kind::Other => "Other",
        }
    }

    /// Classifies a file by its extension.
    pub fn from_ext(ext: &str) -> Kind {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "jpe" | "png" | "gif" | "bmp" | "heic" | "heif" | "webp" | "tif"
            | "tiff" | "avif" | "thm" | "psd" | "psb" => Kind::Photo,
            "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "dng" | "orf"
            | "rw2" | "pef" | "raf" | "rwl" | "3fr" | "erf" | "kdc" | "mrw" | "x3f" | "srw"
            | "iiq" => Kind::Raw,
            "mov" | "mp4" | "m4v" | "avi" | "mts" | "m2ts" | "3gp" | "mkv" | "wmv" | "mpg"
            | "mpeg" | "flv" | "webm" | "mod" | "tod" | "insv" | "lrv" | "lrf" | "mxf" | "asf" => {
                Kind::Video
            }
            "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" | "wma" | "aif" | "aiff" | "amr" => {
                Kind::Audio
            }
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt"
            | "ods" | "odp" | "pages" | "numbers" | "key" | "csv" | "md" | "html" | "htm"
            | "epub" | "msg" | "vsd" => Kind::Document,
            _ => Kind::Other,
        }
    }
}

/// A file found by `carve`.
#[derive(Debug, Clone)]
pub struct Found {
    pub len: u64,
    pub ext: &'static str,
    pub kind: Kind,
    /// When the photo was taken or the video recorded, if the file says.
    pub taken: Option<DateTime>,
}

pub trait Format: Send + Sync {
    /// Short name used to select formats on the command line.
    fn name(&self) -> &'static str;

    /// Largest file this format will carve; bounds the work done on a
    /// damaged candidate.
    fn max_len(&self) -> u64;

    /// Cheap check on the first [`HEAD_LEN`] bytes at a candidate offset.
    fn matches(&self, head: &[u8]) -> bool;

    /// Walks the file starting at `start` and returns its extent, or `None`
    /// if the data there is not a valid file. Reads must stay below
    /// `start + limit`.
    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found>;
}

/// The format whose header is at the start of `head`, if any.
pub fn identify<'f>(formats: &'f [Box<dyn Format>], head: &[u8]) -> Option<&'f dyn Format> {
    formats.iter().find(|f| f.matches(head)).map(|f| f.as_ref())
}

/// Every supported format, in the order they are tried.
pub fn all() -> Vec<Box<dyn Format>> {
    let mut v: Vec<Box<dyn Format>> = vec![
        Box::new(jpeg::Jpeg),
        Box::new(bmff::Bmff),
        Box::new(tiff::TiffRaw),
        Box::new(raf::Raf),
        Box::new(png::Png),
        Box::new(riff::Riff),
        Box::new(mts::Mts),
        Box::new(gif::Gif),
        Box::new(pdf::Pdf),
        Box::new(zip::Zip),
        Box::new(ole::Ole),
        Box::new(rar::Rar),
        Box::new(sevenzip::SevenZip),
        Box::new(sqlite::Sqlite),
        Box::new(pst::Pst),
        Box::new(mxf::Mxf),
        Box::new(asf::Asf),
        Box::new(flac::Flac),
        Box::new(mp3::Mp3),
        Box::new(ebml::Ebml),
        Box::new(psd::Psd),
        // Late: `00 01 00 00` is common, though the checks after it are not.
        Box::new(font::Font),
        // Last: its signature is only two bytes.
        Box::new(bmp::Bmp),
    ];
    // Your own scanners (src/formats/custom/), before the shape-based guessers.
    v.extend(custom::all());
    // After everything with a signature: text has none, only a shape.
    v.push(Box::new(delimited::Delimited));
    // Last of all: text is the weakest guess there is.
    v.push(Box::new(plaintext::PlainText));
    v
}

/// A calendar date and time, as recorded by the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    pub(crate) fn new(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Option<Self> {
        let valid = (1990..=2100).contains(&year)
            && (1..=12).contains(&month)
            && (1..=31).contains(&day)
            && hour < 24
            && minute < 60
            && second < 61;
        valid.then_some(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    /// Parses the EXIF form `YYYY:MM:DD HH:MM:SS`.
    pub fn parse_exif(s: &[u8]) -> Option<Self> {
        if s.len() < 19 {
            return None;
        }
        let num = |range: std::ops::Range<usize>| -> Option<u16> {
            std::str::from_utf8(&s[range]).ok()?.parse().ok()
        };
        Self::new(
            num(0..4)?,
            num(5..7)? as u8,
            num(8..10)? as u8,
            num(11..13)? as u8,
            num(14..16)? as u8,
            num(17..19)? as u8,
        )
    }

    /// Converts seconds since the Unix epoch (UTC).
    pub fn from_unix(secs: i64) -> Option<Self> {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self::new(
            u16::try_from(year).ok()?,
            month as u8,
            day as u8,
            (rem / 3600) as u8,
            (rem % 3600 / 60) as u8,
            (rem % 60) as u8,
        )
    }
}

impl Serialize for DateTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        ))
    }
}

impl<'de> Deserialize<'de> for DateTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Same field positions as the EXIF form.
        Self::parse_exif(s.as_bytes()).ok_or_else(|| serde::de::Error::custom("invalid date"))
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}.{:02}.{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Carvers read whatever is on the card, so damaged copies of valid
    /// files must give `None` or an extent inside the data, never a panic.
    #[test]
    fn carvers_survive_damaged_files() {
        let samples: Vec<(Box<dyn Format>, Vec<u8>)> = vec![
            (
                Box::new(ole::Ole),
                ole::tests::build(&[("WordDocument", 9000), ("x", 100)]),
            ),
            (Box::new(rar::Rar), rar::tests::rar4()),
            (Box::new(rar::Rar), rar::tests::rar5()),
            (Box::new(sevenzip::SevenZip), sevenzip::tests::build()),
            (Box::new(sqlite::Sqlite), sqlite::tests::build()),
            (Box::new(pst::Pst), pst::tests::build(true)),
            (Box::new(pst::Pst), pst::tests::build(false)),
            (Box::new(font::Font), font::tests::build(3000)),
            (Box::new(mxf::Mxf), mxf::tests::build(true)),
            (Box::new(mxf::Mxf), mxf::tests::build(false)),
            (Box::new(asf::Asf), asf::tests::build()),
            (Box::new(flac::Flac), flac::tests::build(40, true)),
            (Box::new(flac::Flac), flac::tests::build(40, false)),
        ];
        for (format, good) in &samples {
            check_survives_damage(format.as_ref(), good);
        }
    }

    /// Damages `good` every way a drive might and checks `format` neither
    /// panics nor claims more bytes than exist.
    pub(crate) fn check_survives_damage(format: &dyn Format, good: &[u8]) {
        let good = good.to_vec();
        let mut x = 0x9E37_79B9u32;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        {
            let len = good.len();
            let try_carve = |data: &Vec<u8>| {
                let mut data = data.clone();
                data.resize(data.len().max(HEAD_LEN), 0);
                if let Some(f) = format.carve(&mut Reader::new(&data, 0), 0, data.len() as u64) {
                    assert!(f.len > 0 && f.len <= data.len() as u64, "{}", format.name());
                }
            };
            assert!(format.matches(&[good.clone(), vec![0; HEAD_LEN]].concat()));
            for cut in (0..len).step_by(len / 97 + 1) {
                try_carve(&good[..cut].to_vec());
            }
            for _ in 0..2000 {
                let mut bad = good.clone();
                for _ in 0..1 + rand() % 4 {
                    let at = rand() as usize % len;
                    bad[at] ^= 1 << (rand() % 8);
                }
                try_carve(&bad);
                // Random bytes after a valid start.
                let keep = rand() as usize % len.min(600);
                let mut noise = good[..keep].to_vec();
                noise.extend((0..len - keep).map(|_| rand() as u8));
                try_carve(&noise);
            }
        }
    }

    /// Every file in `custom/` gets the same damage tests as the built-in
    /// scanners, and must recognise its own samples.
    #[test]
    fn custom_scanners_survive_damaged_files() {
        for (file, format, samples) in custom::samples() {
            for good in &samples {
                let mut padded = good.clone();
                padded.resize(padded.len().max(HEAD_LEN), 0);
                assert!(
                    format.matches(&padded),
                    "{file}: does not match its own sample"
                );
                let found = format.carve(&mut Reader::new(&padded, 0), 0, padded.len() as u64);
                assert!(found.is_some(), "{file}: cannot carve its own sample");
                check_survives_damage(format.as_ref(), good);
            }
        }
    }

    #[test]
    fn parses_exif_dates() {
        let d = DateTime::parse_exif(b"2004:07:15 14:03:59\0").unwrap();
        assert_eq!(d.to_string(), "2004-07-15 14.03.59");
        assert!(DateTime::parse_exif(b"0000:00:00 00:00:00").is_none());
        assert!(DateTime::parse_exif(b"    :  :     :  :  ").is_none());
    }

    #[test]
    fn converts_unix_time() {
        let d = DateTime::from_unix(1_089_900_239).unwrap();
        assert_eq!(d.to_string(), "2004-07-15 14.03.59");
        assert!(DateTime::from_unix(0).is_none());
    }
}
