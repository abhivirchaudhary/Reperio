//! Groups of similar photos: burst shots, edits and resized copies of the
//! same picture, and the same file found twice, so the results can show
//! one of each with the rest a click away.

use std::collections::HashMap;

use serde::Serialize;

use crate::formats::Kind;
use crate::preview::Signature;
use crate::session::{Health, Item, Origin};

/// Most of the 128 bits two photos' signatures may differ by and still be
/// grouped. The same picture resized, recompressed or lightly edited
/// differs by 0 to 4, the next shot of a burst (the camera moved a
/// little) by about 5; unrelated photos by around 64, and rarely under 20,
/// even when taken in the same light.
pub const MAX_DISTANCE: u32 = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    /// The member most worth keeping; also first in `members`.
    pub best: u32,
    /// Every file in the group, best first.
    pub members: Vec<u32>,
}

/// Groups `items` whose signatures are within [`MAX_DISTANCE`] of each
/// other, joined with `exact`, groups of identical files (see
/// [`crate::session::duplicate_groups`]). Photos are only grouped with
/// photos of the same kind: a RAW file and the JPEG the camera saved with
/// it are both worth keeping.
pub fn group(items: &[Item], sigs: &HashMap<u32, Signature>, exact: &[Vec<u32>]) -> Vec<Group> {
    let by_id: HashMap<u32, &Item> = items.iter().map(|i| (i.id, i)).collect();
    let mut sets = Sets::default();

    let photos: Vec<(&Item, &Signature)> = items
        .iter()
        .filter(|i| matches!(i.kind, Kind::Photo | Kind::Raw))
        .filter_map(|i| Some((i, sigs.get(&i.id).filter(|s| !s.flat)?)))
        .collect();
    // Every pair is compared: a 64-bit XOR and count per pair is quick
    // enough for the tens of thousands of photos a card holds, and unlike
    // bucketing by parts of the hash it misses no pair within the distance.
    for (n, (a, sa)) in photos.iter().enumerate() {
        for (b, sb) in &photos[n + 1..] {
            if a.kind == b.kind && sa.distance(sb) <= MAX_DISTANCE {
                sets.join(a.id, b.id);
            }
        }
    }
    for copies in exact {
        for pair in copies.windows(2) {
            sets.join(pair[0], pair[1]);
        }
    }

    let mut groups: HashMap<u32, Vec<&Item>> = HashMap::new();
    let ids: Vec<u32> = sets.parent.keys().copied().collect();
    for id in ids {
        if let Some(item) = by_id.get(&id) {
            groups.entry(sets.root(id)).or_default().push(item);
        }
    }
    let mut out: Vec<Group> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort_by_key(|i| rank(i, sigs.get(&i.id)));
            let members: Vec<u32> = g.iter().map(|i| i.id).collect();
            Group {
                best: members[0],
                members,
            }
        })
        .collect();
    out.sort_by_key(|g| g.best);
    out
}

/// Order of preference within a group, best first: a file that will open,
/// then the most pixels, then one with its original name, then the larger
/// file (less compressed), then the first found.
fn rank(i: &Item, sig: Option<&Signature>) -> impl Ord {
    let broken = matches!(i.health, Health::Damaged | Health::Overwritten);
    let pixels = sig.map_or(0, |s| u64::from(s.width) * u64::from(s.height));
    let origin = match i.origin {
        Origin::Deleted => 0,
        Origin::Existing => 1,
        Origin::Snapshot => 2,
        Origin::Carved => 3,
    };
    (
        broken,
        std::cmp::Reverse(pixels),
        origin,
        std::cmp::Reverse(i.size),
        i.id,
    )
}

/// Disjoint sets of ids (union-find).
#[derive(Default)]
struct Sets {
    parent: HashMap<u32, u32>,
}

impl Sets {
    fn root(&mut self, id: u32) -> u32 {
        let mut r = *self.parent.entry(id).or_insert(id);
        while let Some(&p) = self.parent.get(&r).filter(|&&p| p != r) {
            r = p;
        }
        // Point everything on the path straight at the root.
        let mut at = id;
        while at != r {
            let next = self.parent[&at];
            self.parent.insert(at, r);
            at = next;
        }
        r
    }

    fn join(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra != rb {
            self.parent.insert(ra.max(rb), ra.min(rb));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extent::Extent;
    use crate::preview;
    use image::{DynamicImage, RgbImage};

    /// A made-up photo: a sky-to-ground gradient with a few coloured
    /// blobs, different for every seed.
    fn photo(seed: u64, w: u32, h: u32) -> RgbImage {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 1000) as f32 / 1000.0
        };
        let blobs: Vec<[f32; 6]> = (0..6)
            .map(|_| {
                [
                    next(),
                    next(),
                    0.08 + next() * 0.25,
                    next() * 255.0,
                    next() * 255.0,
                    next() * 255.0,
                ]
            })
            .collect();
        let tilt = next();
        RgbImage::from_fn(w, h, |x, y| {
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            let base = 40.0 + 180.0 * ((v + tilt * u) / (1.0 + tilt));
            let mut c = [base, base * 0.9, 255.0 - base * 0.6];
            for b in &blobs {
                let d = ((u - b[0]).powi(2) + (v - b[1]).powi(2)).sqrt();
                if d < b[2] {
                    c = [b[3], b[4], b[5]];
                }
            }
            image::Rgb(c.map(|v| v.clamp(0.0, 255.0) as u8))
        })
    }

    fn encode(img: &RgbImage, quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(img)
            .unwrap();
        out
    }

    /// Puts the files one after another on a "card", as items.
    fn card(files: &[(&str, Vec<u8>)]) -> (Vec<u8>, Vec<Item>) {
        let mut data = Vec::new();
        let mut items = Vec::new();
        for (n, (name, bytes)) in files.iter().enumerate() {
            items.push(Item {
                id: n as u32,
                name: name.to_string(),
                dir: String::new(),
                ext: "jpg".into(),
                kind: Kind::Photo,
                origin: Origin::Deleted,
                health: Health::Good,
                size: bytes.len() as u64,
                taken: None,
                modified: None,
                extents: vec![Extent::new(data.len() as u64, bytes.len() as u64)],
                volume: None,
                fix: None,
                found_by: None,
                nsrl_status: None,
            });
            data.extend(bytes);
        }
        (data, items)
    }

    fn sigs(data: &Vec<u8>, items: &[Item]) -> HashMap<u32, Signature> {
        items
            .iter()
            .map(|i| (i.id, preview::signature(data, i).expect("signature")))
            .collect()
    }

    fn resized(img: &RgbImage, w: u32, h: u32) -> RgbImage {
        DynamicImage::ImageRgb8(img.clone())
            .resize(w, h, image::imageops::FilterType::Lanczos3)
            .to_rgb8()
    }

    #[test]
    fn groups_versions_of_the_same_photo_and_nothing_else() {
        let a = photo(1, 1200, 800);
        let brighter = RgbImage::from_fn(1200, 800, |x, y| {
            image::Rgb(a.get_pixel(x, y).0.map(|c| c.saturating_add(12)))
        });
        let cropped = DynamicImage::ImageRgb8(a.clone())
            .crop_imm(12, 8, 1176, 784)
            .to_rgb8();
        // The next shot of a burst: the camera moved a little.
        let panned = DynamicImage::ImageRgb8(photo(1, 1260, 820))
            .crop_imm(50, 14, 1200, 800)
            .to_rgb8();
        let mut files = vec![
            ("IMG_0001.JPG", encode(&a, 92)),
            ("IMG_0001 small.JPG", encode(&resized(&a, 600, 400), 80)),
            ("IMG_0001 recompressed.JPG", encode(&a, 45)),
            ("IMG_0001 edited.JPG", encode(&brighter, 90)),
            ("IMG_0001 cropped.JPG", encode(&cropped, 90)),
            ("IMG_0002.JPG", encode(&panned, 92)),
        ];
        // Thirty unrelated photos, at assorted sizes.
        let others: Vec<(String, Vec<u8>)> = (100..130u32)
            .map(|s| {
                let img = photo(u64::from(s), 640 + (s % 3) * 160, 480);
                (format!("IMG_{s}.JPG"), encode(&img, 85))
            })
            .collect();
        files.extend(others.iter().map(|(n, b)| (n.as_str(), b.clone())));
        let (data, items) = card(&files);
        let sigs = sigs(&data, &items);

        // Distances, for anyone tuning MAX_DISTANCE.
        let versions: Vec<u32> = (1..6).map(|i| sigs[&0].distance(&sigs[&i])).collect();
        let n = items.len() as u32;
        let nearest_unrelated = (6..n)
            .flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j)))
            .map(|(i, j)| sigs[&i].distance(&sigs[&j]))
            .min()
            .unwrap();
        eprintln!("versions: {versions:?}; nearest unrelated pair: {nearest_unrelated}");

        let groups = group(&items, &sigs, &[]);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let mut members = groups[0].members.clone();
        members.sort_unstable();
        assert_eq!(members, vec![0, 1, 2, 3, 4, 5]);
        // The best is the full-size original: most pixels, then the larger
        // (less compressed) file.
        assert_eq!(groups[0].best, 0);
        assert_eq!((sigs[&0].width, sigs[&0].height), (1200, 800));
    }

    #[test]
    fn exact_copies_group_and_the_best_copy_leads() {
        let jpg = encode(&photo(7, 800, 600), 85);
        let flat = encode(&RgbImage::from_pixel(800, 600, image::Rgb([3, 3, 3])), 85);
        let other_flat = encode(&RgbImage::from_pixel(640, 480, image::Rgb([5, 4, 3])), 70);
        let (data, mut items) = card(&[
            ("found.jpg", jpg.clone()),
            ("Holiday.jpg", jpg),
            ("black.jpg", flat.clone()),
            ("black copy.jpg", flat),
            ("dark.jpg", other_flat),
        ]);
        items[0].origin = Origin::Carved;
        let sigs = sigs(&data, &items);
        assert!(sigs[&2].flat && sigs[&4].flat);
        let exact = crate::session::duplicate_groups(&data, &items);
        let groups = group(&items, &sigs, &exact);
        // Flat pictures group only when they are the same file.
        assert_eq!(
            groups,
            vec![
                Group {
                    best: 1,
                    members: vec![1, 0]
                },
                Group {
                    best: 2,
                    members: vec![2, 3]
                },
            ]
        );
    }

    #[test]
    fn a_damaged_copy_is_never_the_best() {
        let img = photo(3, 1000, 750);
        let (data, mut items) = card(&[
            ("big but damaged.jpg", encode(&img, 95)),
            ("small.jpg", encode(&resized(&img, 500, 375), 85)),
        ]);
        items[0].health = Health::Damaged;
        let groups = group(&items, &sigs(&data, &items), &[]);
        assert_eq!(
            groups,
            vec![Group {
                best: 1,
                members: vec![1, 0]
            }]
        );
    }
}
