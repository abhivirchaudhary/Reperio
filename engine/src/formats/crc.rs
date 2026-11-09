//! Checksums that formats store over their own headers or frames. A
//! matching checksum is the strongest evidence that bytes on a card are
//! a real header and not leftover data that happens to look like one.

/// The CRC-32 table (reflected polynomial 0xEDB88320) used by ZIP, RAR,
/// 7z and Outlook.
const CRC32: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Feeds `data` into a running CRC-32 without the usual pre- and
/// post-inversion. Outlook stores this "weak" form; [`crc32`] wraps it.
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC32[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// The standard CRC-32, as computed by zlib.
pub fn crc32(data: &[u8]) -> u32 {
    !crc32_update(!0, data)
}

/// FLAC's frame CRC-16 (polynomial 0x8005, not reflected, starting at 0).
const CRC16: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub fn crc16_flac(data: &[u8]) -> u16 {
    data.iter().fold(0, |crc, &b| {
        (crc << 8) ^ CRC16[usize::from((crc >> 8) as u8 ^ b)]
    })
}

/// FLAC's frame header CRC-8 (polynomial 0x07, starting at 0).
pub fn crc8_flac(data: &[u8]) -> u8 {
    data.iter().fold(0, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // The standard check values for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc16_flac(b"123456789"), 0xFEE8);
        assert_eq!(crc8_flac(b"123456789"), 0xF4);
    }
}
