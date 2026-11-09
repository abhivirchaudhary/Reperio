//! What common cameras write into their videos, so a video whose index is
//! lost can be rebuilt without a good clip from the same camera.
//!
//! A rebuilt index needs the codec settings, frame rate and sound format.
//! Many cameras repeat their parameter sets (SPS, PPS and for H.265 the
//! VPS) in the stream before every keyframe, so the codec settings come
//! from the broken file itself (see [`crate::videofix::Reference::from_stream`]).
//! What the stream doesn't say is filled in from the camera's profile:
//! the file brand players expect, the usual frame rates and the sound
//! format, and which interleaved chunks are metadata to step over.
//!
//! A camera is recognised from what survives in a broken file (its `ftyp`
//! brands, boxes written before the media data, text its encoder leaves in
//! the stream) or, in a good file from the same card, from its index
//! (handler names, sample entry types, `udta` and `uuid` boxes).
//!
//! The values are what these cameras are known to write; where a model
//! line varies (sample rate, frame rate), the stream is trusted first and
//! the profile is only the fallback.

/// How the sound track is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC-LC frames, 1024 samples each.
    Aac,
    /// Uncompressed 16-bit samples, big-endian (`twos`).
    PcmBe16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioProfile {
    pub codec: AudioCodec,
    /// The usual rate; the rebuilt track measures the real one where it
    /// can (see [`crate::audiofix`]).
    pub sample_rate: u32,
    pub channels: u16,
    /// Sound is written in chunks between the video frames (true of every
    /// camera here; listed so a profile says so explicitly).
    pub interleaved: bool,
}

#[derive(Debug)]
pub struct Profile {
    /// Shown to people: "DJI Osmo Pocket", "GoPro".
    pub name: &'static str,
    /// The `ftyp` box this camera writes, used when the broken file lost its
    /// own: major brand, then compatible brands.
    pub brand: [u8; 4],
    pub compatible: &'static [[u8; 4]],
    /// QuickTime (`qt  `) files use a different sound sample entry layout.
    pub quicktime: bool,
    /// Frame rates the camera records at, as (timescale, frame duration).
    /// The first is the default when the stream carries no timing.
    pub frame_rates: &'static [(u32, u32)],
    pub audio: Option<AudioProfile>,
    /// Sample entry types of tracks that are not picture or sound: their
    /// chunks sit between the video and sound and are stepped over.
    pub metadata_tracks: &'static [&'static [u8; 4]],
    /// Whether the camera repeats parameter sets in the stream, so no
    /// reference video is needed. Cameras that don't need a reference.
    pub in_band_params: bool,
    /// Extension of the low-resolution copy recorded beside each video.
    pub proxy_ext: Option<&'static str>,
    // Recognition.
    /// Major or compatible `ftyp` brands only this camera writes.
    brands: &'static [[u8; 4]],
    /// Byte strings found near the start of its files (`uuid` boxes, free
    /// box text, encoder names in the stream) or in its index (handler
    /// names, `udta` boxes).
    markers: &'static [&'static [u8]],
}

const AAC_48K_STEREO: Option<AudioProfile> = Some(AudioProfile {
    codec: AudioCodec::Aac,
    sample_rate: 48_000,
    channels: 2,
    interleaved: true,
});

/// 23.976, 25, 29.97, 30, 50, 59.94, 60 and 24 fps.
const COMMON_RATES: &[(u32, u32)] = &[
    (30_000, 1001),
    (24_000, 1001),
    (25_000, 1000),
    (30_000, 1000),
    (50_000, 1000),
    (60_000, 1001),
    (60_000, 1000),
    (24_000, 1000),
];

pub static PROFILES: &[Profile] = &[
    Profile {
        name: "DJI Osmo Pocket",
        brand: *b"isom",
        compatible: &[*b"isom", *b"iso2", *b"avc1", *b"mp41"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: AAC_48K_STEREO,
        metadata_tracks: &[b"djmd", b"dbgi"],
        in_band_params: true,
        proxy_ext: Some("lrf"),
        brands: &[],
        markers: &[b"DJI OsmoPocket", b"DJI Pocket", b"OsmoPocket", b"FC2101"],
    },
    Profile {
        name: "DJI Osmo Action",
        brand: *b"isom",
        compatible: &[*b"isom", *b"iso2", *b"avc1", *b"mp41"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: AAC_48K_STEREO,
        metadata_tracks: &[b"djmd", b"dbgi"],
        in_band_params: true,
        proxy_ext: Some("lrf"),
        brands: &[],
        markers: &[
            b"DJI OsmoAction",
            b"Osmo Action",
            b"OsmoAction",
            b"AC001",
            b"AC002",
        ],
    },
    Profile {
        // Mavic, Mini, Air and Phantom drones: no microphone.
        name: "DJI drone",
        brand: *b"isom",
        compatible: &[*b"isom", *b"iso2", *b"avc1", *b"mp41"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: None,
        metadata_tracks: &[b"djmd", b"dbgi", b"tmcd"],
        in_band_params: true,
        proxy_ext: Some("lrf"),
        brands: &[],
        markers: &[
            b"DJI Mavic",
            b"Mavic",
            b"DJI Mini",
            b"DJI Air",
            b"FC3170",
            b"FC3411",
        ],
    },
    Profile {
        // Any other DJI device: the handler and metadata names are shared.
        name: "DJI",
        brand: *b"isom",
        compatible: &[*b"isom", *b"iso2", *b"avc1", *b"mp41"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: AAC_48K_STEREO,
        metadata_tracks: &[b"djmd", b"dbgi", b"tmcd"],
        in_band_params: true,
        proxy_ext: Some("lrf"),
        brands: &[],
        markers: &[
            b"DJI.Meta",
            b"DJI.AVC",
            b"DJI.HEVC",
            b"DJI.Subtitle",
            b"djmd",
            b"DJI ",
        ],
    },
    Profile {
        // HERO5 onwards: AAC sound, a timecode track and GPMF telemetry
        // ("GoPro MET", chunks starting with a DEVC record).
        name: "GoPro",
        brand: *b"mp41",
        compatible: &[*b"mp41"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: AAC_48K_STEREO,
        metadata_tracks: &[b"gpmd", b"tmcd", b"fdsc"],
        in_band_params: true,
        proxy_ext: Some("lrv"),
        brands: &[],
        markers: &[
            b"GoPro AVC",
            b"GoPro H.265",
            b"GoPro AAC",
            b"GoPro MET",
            b"GPMF",
            b"GoPro",
        ],
    },
    Profile {
        // QuickTime files with timed metadata (`mebx`). iPhones keep the
        // parameter sets in the index only and record variable frame
        // rates on a 600 timescale, so a reference clip is still needed
        // for the picture; the profile gives the brand and sound.
        name: "iPhone",
        brand: *b"qt  ",
        compatible: &[*b"qt  "],
        quicktime: true,
        frame_rates: &[(600, 20), (600, 10), (600, 25), (600, 24)],
        audio: Some(AudioProfile {
            codec: AudioCodec::Aac,
            sample_rate: 44_100,
            channels: 2,
            interleaved: true,
        }),
        metadata_tracks: &[b"mebx"],
        in_band_params: false,
        proxy_ext: None,
        brands: &[],
        markers: &[
            b"com.apple.quicktime.make",
            b"Core Media Video",
            b"Core Media Audio",
            b"Core Media Metadata",
            b"Apple",
        ],
    },
    Profile {
        // XAVC S: H.264 or H.265 with uncompressed sound and an `rtmd`
        // real-time metadata track; a `uuid` box (PROF/USMT) precedes
        // the media data.
        name: "Sony XAVC S",
        brand: *b"XAVC",
        compatible: &[*b"XAVC", *b"mp42", *b"iso2"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: Some(AudioProfile {
            codec: AudioCodec::PcmBe16,
            sample_rate: 48_000,
            channels: 2,
            interleaved: true,
        }),
        metadata_tracks: &[b"rtmd", b"tmcd"],
        in_band_params: true,
        proxy_ext: None,
        brands: &[*b"XAVC"],
        markers: &[b"Sony", b"SONY", b"rtmd", b"PROF", b"USMT"],
    },
    Profile {
        // EOS and PowerShot MP4s; Canon's `uuid` box carries "CNCV".
        name: "Canon",
        brand: *b"mp42",
        compatible: &[*b"mp42", *b"isom", *b"CAEP"],
        quicktime: false,
        frame_rates: COMMON_RATES,
        audio: AAC_48K_STEREO,
        metadata_tracks: &[b"tmcd", b"CTMD"],
        in_band_params: true,
        proxy_ext: None,
        brands: &[*b"CAEP"],
        markers: &[b"CNCV", b"CanonMVI", b"Canon", b"CNTH"],
    },
];

/// The profile used when nothing identifies the camera: an ordinary MP4
/// with AAC sound, which is what most cameras and phones write.
pub static GENERIC: Profile = Profile {
    name: "camera",
    brand: *b"isom",
    compatible: &[*b"isom", *b"iso2", *b"avc1", *b"mp41"],
    quicktime: false,
    frame_rates: COMMON_RATES,
    audio: AAC_48K_STEREO,
    metadata_tracks: &[b"tmcd"],
    in_band_params: true,
    proxy_ext: None,
    brands: &[],
    markers: &[],
};

/// The camera that made a file, from bytes of it: its start (the `ftyp`
/// and boxes before the media data, plus the first stretch of the stream,
/// where encoders leave their name) and, for a good file, its index.
pub fn identify(head: &[u8], moov: Option<&[u8]>) -> Option<&'static Profile> {
    let ftyp = ftyp_brands(head);
    let contains = |hay: &[u8], needle: &[u8]| memchr::memmem::find(hay, needle).is_some();
    let mut best: Option<(usize, &'static Profile)> = None;
    for p in PROFILES {
        let mut score = 0;
        if p.brands.iter().any(|b| ftyp.contains(b)) {
            score += 3;
        }
        for m in p.markers {
            if contains(head, m) || moov.is_some_and(|v| contains(v, m)) {
                score += 2;
            }
        }
        // Ties go to the earlier, more specific profile.
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, p));
        }
    }
    best.map(|(_, p)| p)
}

/// The major and compatible brands of the `ftyp` box `head` starts with.
pub fn ftyp_brands(head: &[u8]) -> Vec<[u8; 4]> {
    if head.get(4..8) != Some(b"ftyp") {
        return Vec::new();
    }
    let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let body = head.get(8..size.min(head.len()).max(8)).unwrap_or_default();
    let mut out = Vec::new();
    if let Some(major) = body.get(..4) {
        out.push(major.try_into().unwrap());
    }
    // Skip the minor version.
    for c in body.get(8..).unwrap_or_default().chunks_exact(4).take(32) {
        out.push(c.try_into().unwrap());
    }
    out
}

/// Builds an `ftyp` box for a profile.
pub fn ftyp_box(p: &Profile) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(((16 + 4 * p.compatible.len()) as u32).to_be_bytes());
    v.extend(b"ftyp");
    v.extend(p.brand);
    v.extend(if p.quicktime { 0x2005_0300u32 } else { 0x200 }.to_be_bytes());
    for c in p.compatible {
        v.extend(c);
    }
    v
}

/// Whether a chunk of interleaved data is a known metadata record rather
/// than sound: GoPro's GPMF telemetry starts with a `DEVC` record, and a
/// timecode chunk is a single 4-byte frame number.
pub fn is_metadata_chunk(chunk: &[u8]) -> bool {
    chunk.starts_with(b"DEVC") || chunk.len() == 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_cameras() {
        let mut gopro = ftyp_box(&PROFILES[4]);
        gopro.extend(b"\0\0\0\x08free....GoPro AVC encoder");
        assert_eq!(identify(&gopro, None).unwrap().name, "GoPro");
        let sony = ftyp_box(&PROFILES[6]);
        assert_eq!(identify(&sony, None).unwrap().name, "Sony XAVC S");
        let moov = b"....hdlr....mhlrvideappl....Core Media Video".to_vec();
        assert_eq!(identify(b"", Some(&moov)).unwrap().name, "iPhone");
        assert!(identify(&ftyp_box(&GENERIC), None).is_none());
        assert_eq!(ftyp_brands(&sony)[0], *b"XAVC");
    }

    #[test]
    fn recognises_metadata_chunks() {
        assert!(is_metadata_chunk(b"DEVC\0\0\0\0"));
        assert!(!is_metadata_chunk(&[0x21, 0x10, 0x05, 0x00, 0x07]));
    }
}
