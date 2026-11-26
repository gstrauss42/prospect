//! Image type and size from the bytes themselves: JPEG, PNG and WebP headers, parsed by hand.
//!
//! Servers lie about `Content-Type` (an HTML error page served as `image/jpeg`, a `.jpg` URL that
//! answers WebP), so every downloaded image is identified from its first bytes before it is
//! trusted, shown to the vision model or stored.

/// The image type and size from the first bytes of a JPEG, PNG or WebP file: what a browser will
/// display (a JPEG's EXIF rotation by 90° swaps the two). None for anything else, and for
/// headers that are cut short or corrupt.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return jpeg(bytes).map(|(w, h)| ("image/jpeg", w, h));
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png(bytes).map(|(w, h)| ("image/png", w, h));
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return webp(bytes).map(|(w, h)| ("image/webp", w, h));
    }
    None
}

/// True when a whole file (not just its header) arrived: a PNG ends with its IEND chunk, a JPEG
/// has an end-of-image marker after its scan data, a WebP's RIFF size matches its length. A
/// truncated download would show as a half-grey photo, so it is rejected.
pub fn is_complete(bytes: &[u8]) -> bool {
    match sniff(bytes) {
        Some(("image/png", _, _)) => {
            bytes.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82])
        }
        Some(("image/jpeg", _, _)) => {
            // Inside scan data 0xFF is always followed by 0x00 or a restart marker, so an FF D9
            // after the main scan starts can only be the real end. (An EXIF thumbnail is a whole
            // JPEG inside APP1; walking the segments keeps its markers from being mistaken.)
            let sos = jpeg_scan_start(bytes);
            let eoi = bytes.windows(2).rposition(|w| w == [0xFF, 0xD9]);
            matches!((sos, eoi), (Some(s), Some(e)) if e > s)
        }
        Some(("image/webp", _, _)) => {
            let riff = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
            // The RIFF size counts everything after the first 8 bytes; a pad byte may follow.
            riff.checked_add(8)
                .is_some_and(|total| bytes.len() == total || bytes.len() == total + 1)
        }
        _ => false,
    }
}

/// The offset of a JPEG's first start-of-scan marker, found by walking the marker segments.
fn jpeg_scan_start(b: &[u8]) -> Option<usize> {
    let mut i = 2usize;
    loop {
        if *b.get(i)? != 0xFF {
            return None;
        }
        while *b.get(i + 1)? == 0xFF {
            i += 1;
        }
        match *b.get(i + 1)? {
            0xDA => return Some(i),
            0x01 | 0xD0..=0xD7 => i += 2,
            0x00 | 0xD8 | 0xD9 => return None,
            _ => {
                let len = be16(b, i + 2)? as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
}

fn be16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn be32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

fn le16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn le24(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        0,
    ]))
}

fn le32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

/// PNG: the IHDR chunk must come first and carry a non-zero size.
fn png(b: &[u8]) -> Option<(u32, u32)> {
    if be32(b, 8)? != 13 || b.get(12..16)? != b"IHDR" {
        return None;
    }
    let (w, h) = (be32(b, 16)?, be32(b, 20)?);
    let (depth, color) = (*b.get(24)?, *b.get(25)?);
    let valid_depth = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    // The spec caps each side at 2^31 - 1.
    (w > 0 && h > 0 && w <= i32::MAX as u32 && h <= i32::MAX as u32 && valid_depth)
        .then_some((w, h))
}

/// JPEG: walk the marker segments (APPn, DQT, DHT, COM…) to the first start-of-frame. Also reads
/// the EXIF orientation on the way, since a camera's "rotate 90°" flag swaps what is displayed.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    let mut orientation = 1u16;
    loop {
        if *b.get(i)? != 0xFF {
            return None;
        }
        // Any number of 0xFF fill bytes may precede a marker.
        while *b.get(i + 1)? == 0xFF {
            i += 1;
        }
        let marker = *b.get(i + 1)?;
        match marker {
            // Markers without a length: TEM and the restart markers.
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            // A second SOI, end of image or start of scan before any frame header: not a usable file.
            0x00 | 0xD8 | 0xD9 | 0xDA => return None,
            _ => {}
        }
        let len = be16(b, i + 2)? as usize;
        if len < 2 {
            return None;
        }
        let seg = b.get(i + 4..i + 2 + len)?;
        let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            if seg.len() < 6 {
                return None;
            }
            let h = u16::from_be_bytes([seg[1], seg[2]]) as u32;
            let w = u16::from_be_bytes([seg[3], seg[4]]) as u32;
            // A height of 0 means "defined later by a DNL marker": unusual and unsupported here.
            if w == 0 || h == 0 || seg[5] == 0 {
                return None;
            }
            return Some(if (5..=8).contains(&orientation) {
                (h, w)
            } else {
                (w, h)
            });
        }
        if marker == 0xE1
            && seg.starts_with(b"Exif\0\0")
            && let Some(o) = exif_orientation(&seg[6..])
        {
            orientation = o;
        }
        i += 2 + len;
    }
}

/// The Orientation tag (0x0112) of IFD0 in a TIFF-structured EXIF block, if present and sane.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(0..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let u16_at = |i: usize| if little { le16(tiff, i) } else { be16(tiff, i) };
    let u32_at = |i: usize| if little { le32(tiff, i) } else { be32(tiff, i) };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for n in 0..count.min(256) {
        let entry = ifd + 2 + n * 12;
        if u16_at(entry)? == 0x0112 {
            // Type SHORT (3), count 1: the value sits in the first two bytes of the value field.
            if u16_at(entry + 2)? != 3 {
                return None;
            }
            let v = u16_at(entry + 8)?;
            return (1..=8).contains(&v).then_some(v);
        }
    }
    None
}

/// WebP: the first chunk is VP8 (lossy), VP8L (lossless) or VP8X (extended, with a canvas size).
fn webp(b: &[u8]) -> Option<(u32, u32)> {
    let riff = le32(b, 4)?;
    if riff < 12 {
        return None;
    }
    let chunk = b.get(12..16)?;
    let (w, h) = match chunk {
        b"VP8 " => {
            // Frame tag (3 bytes, bit 0 clear on a key frame), start code 9d 01 2a, then 14-bit
            // width and height (the top two bits are a scaling hint).
            let tag = *b.get(20)?;
            if tag & 1 != 0 || b.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            (
                (le16(b, 26)? & 0x3FFF) as u32,
                (le16(b, 28)? & 0x3FFF) as u32,
            )
        }
        b"VP8L" => {
            if *b.get(20)? != 0x2F {
                return None;
            }
            let bits = le32(b, 21)?;
            if bits >> 29 != 0 {
                return None; // version must be 0
            }
            ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => (le24(b, 24)? + 1, le24(b, 27)? + 1),
        _ => return None,
    };
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete, valid 1×1 baseline JPEG (grey), as produced by common encoders.
    pub(crate) fn tiny_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        // APP0 JFIF
        v.extend_from_slice(&[
            0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
            0x00, 0x01, 0x00, 0x00,
        ]);
        // DQT: one table of 64 ones
        v.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        v.extend_from_slice(&[1u8; 64]);
        // SOF0: 8-bit, 1×1, one component
        v.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
        ]);
        // DHT DC table 0: one code of length 1 for category 0
        v.extend_from_slice(&[
            0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00,
        ]);
        // DHT AC table 0: one code of length 1 for EOB
        v.extend_from_slice(&[
            0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00,
        ]);
        // SOS, then one block: DC "0", EOB "0", padded with ones
        v.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x3F,
        ]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC (not checked)
        v
    }

    #[test]
    fn real_minimal_png() {
        // A real 1×1 RGBA PNG (67 bytes), byte for byte.
        let png: [u8; 67] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        assert_eq!(sniff(&png), Some(("image/png", 1, 1)));
        assert!(is_complete(&png));
        assert!(!is_complete(&png[..60]), "missing IEND");
        assert_eq!(
            sniff(&png_header(1920, 1280)),
            Some(("image/png", 1920, 1280))
        );
    }

    #[test]
    fn png_rejects_corrupt_and_truncated() {
        let good = png_header(800, 600);
        assert_eq!(sniff(&good[..20]), None, "cut inside IHDR");
        let mut not_ihdr = good.clone();
        not_ihdr[12..16].copy_from_slice(b"IDAT");
        assert_eq!(sniff(&not_ihdr), None);
        assert_eq!(sniff(&png_header(0, 600)), None);
        let mut bad_depth = good.clone();
        bad_depth[24] = 7;
        assert_eq!(sniff(&bad_depth), None);
        assert_eq!(sniff(&png_header(u32::MAX, 10)), None);
    }

    #[test]
    fn real_minimal_jpeg() {
        let j = tiny_jpeg();
        assert_eq!(sniff(&j), Some(("image/jpeg", 1, 1)));
        assert!(is_complete(&j));
        assert!(!is_complete(&j[..j.len() - 2]), "no EOI");

        // A camera JPEG carries a whole thumbnail JPEG (with its own scan and EOI) in APP1. If
        // the main image is cut off, the thumbnail's EOI must not make it look complete.
        let thumb = tiny_jpeg();
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((thumb.len() + 2 + 6) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&thumb);
        let mut camera = vec![0xFF, 0xD8];
        camera.extend_from_slice(&app1);
        camera.extend_from_slice(&j[2..]);
        assert_eq!(sniff(&camera), Some(("image/jpeg", 1, 1)));
        assert!(is_complete(&camera));
        let cut = &camera[..camera.len() - 4];
        assert!(!is_complete(cut), "only the thumbnail's EOI is left");
    }

    fn jpeg_with(sof_marker: u8, w: u16, h: u16, extra_before: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(extra_before);
        v.extend_from_slice(&[0xFF, sof_marker, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn jpeg_skips_app_segments_and_reads_progressive() {
        // APP1 with junk, a COM segment and fill bytes before a progressive (SOF2) frame.
        let mut extra = vec![0xFF, 0xE1, 0x00, 0x08, b'h', b'e', b'l', b'l', b'o', b'!'];
        extra.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x05, b'a', b'b', b'c']);
        extra.extend_from_slice(&[0xFF, 0xFF]); // fill
        let j = jpeg_with(0xC2, 1920, 1080, &extra[..extra.len() - 1]);
        assert_eq!(sniff(&j), Some(("image/jpeg", 1920, 1080)));
        // DHT (C4) is not a frame header even though it is in the C0–CF range.
        let mut dht = vec![0xFF, 0xC4, 0x00, 0x03, 0x00];
        dht.extend_from_slice(&[0xFF, 0xCC, 0x00, 0x03, 0x00]); // DAC
        let j2 = jpeg_with(0xC0, 640, 427, &dht);
        assert_eq!(sniff(&j2), Some(("image/jpeg", 640, 427)));
    }

    #[test]
    fn jpeg_rejects_corrupt_and_truncated() {
        let j = jpeg_with(0xC0, 1600, 900, &[]);
        for cut in [3, 6, 10, 12] {
            assert_eq!(sniff(&j[..cut]), None, "cut at {cut}");
        }
        assert_eq!(sniff(&jpeg_with(0xC0, 0, 900, &[])), None, "zero width");
        assert_eq!(sniff(&jpeg_with(0xC0, 900, 0, &[])), None, "DNL height");
        // Start of scan before any frame header.
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x00]), None);
        // A segment length that runs past the end.
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0x10, 0x00, 0x01]), None);
        // Garbage where a marker should be.
        assert_eq!(
            sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x02, 0x41, 0x42]),
            None
        );
    }

    fn exif_app1(little: bool, orientation: u16) -> Vec<u8> {
        let mut t: Vec<u8> = Vec::new();
        if little {
            t.extend_from_slice(b"II\x2a\x00");
            t.extend_from_slice(&8u32.to_le_bytes());
            t.extend_from_slice(&1u16.to_le_bytes());
            t.extend_from_slice(&0x0112u16.to_le_bytes());
            t.extend_from_slice(&3u16.to_le_bytes());
            t.extend_from_slice(&1u32.to_le_bytes());
            t.extend_from_slice(&orientation.to_le_bytes());
            t.extend_from_slice(&[0, 0]);
            t.extend_from_slice(&0u32.to_le_bytes());
        } else {
            t.extend_from_slice(b"MM\x00\x2a");
            t.extend_from_slice(&8u32.to_be_bytes());
            t.extend_from_slice(&1u16.to_be_bytes());
            t.extend_from_slice(&0x0112u16.to_be_bytes());
            t.extend_from_slice(&3u16.to_be_bytes());
            t.extend_from_slice(&1u32.to_be_bytes());
            t.extend_from_slice(&orientation.to_be_bytes());
            t.extend_from_slice(&[0, 0]);
            t.extend_from_slice(&0u32.to_be_bytes());
        }
        let mut seg = b"Exif\0\0".to_vec();
        seg.extend_from_slice(&t);
        let mut v = vec![0xFF, 0xE1];
        v.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(&seg);
        v
    }

    #[test]
    fn jpeg_exif_rotation_swaps_displayed_size() {
        assert_eq!(
            sniff(&jpeg_with(0xC0, 4000, 3000, &exif_app1(true, 6))),
            Some(("image/jpeg", 3000, 4000))
        );
        assert_eq!(
            sniff(&jpeg_with(0xC0, 4000, 3000, &exif_app1(false, 8))),
            Some(("image/jpeg", 3000, 4000))
        );
        assert_eq!(
            sniff(&jpeg_with(0xC0, 4000, 3000, &exif_app1(false, 1))),
            Some(("image/jpeg", 4000, 3000))
        );
        assert_eq!(
            sniff(&jpeg_with(0xC0, 4000, 3000, &exif_app1(true, 3))),
            Some(("image/jpeg", 4000, 3000))
        );
        // Broken EXIF is ignored, not fatal.
        let broken = [
            0xFF, 0xE1, 0x00, 0x0A, b'E', b'x', b'i', b'f', 0, 0, b'X', b'X',
        ];
        assert_eq!(
            sniff(&jpeg_with(0xC0, 4000, 3000, &broken)),
            Some(("image/jpeg", 4000, 3000))
        );
    }

    fn riff(chunk: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((chunk.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(chunk);
        v
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn real_minimal_webps() {
        // Real files (they decode with libwebp): 1×1 lossless, 3×2 lossy, and 5×4 extended
        // (VP8X with alpha and EXIF chunks).
        let lossless = hex("524946461a000000574542505650384c0d0000002f00000010071011118888fe0700");
        assert_eq!(sniff(&lossless), Some(("image/webp", 1, 1)));
        assert!(is_complete(&lossless));
        assert!(!is_complete(&lossless[..24]));
        let lossy = hex(
            "52494646320000005745425056503820260000007001009d012a0300020007409625a0027401400000febbb7aaa9ebf5adf513b11c65d0000000",
        );
        assert_eq!(sniff(&lossy), Some(("image/webp", 3, 2)));
        assert!(is_complete(&lossy));
        let extended = hex(concat!(
            "524946466c00000057454250565038580a00000018000000040000030000414c50480a000000010750c088084444ff035650382026000000",
            "7001009d012a0500040007409625a0027401400000febbb7aaa9ebf5adf513b11c65d0000000455849460e00000049492a0008000000000000000000"
        ));
        assert_eq!(sniff(&extended), Some(("image/webp", 5, 4)));
        assert!(is_complete(&extended));
        assert!(!is_complete(&extended[..100]));
    }

    #[test]
    fn webp_lossy_and_extended() {
        // VP8: key frame tag, start code, 1920×1280 (the top bits are scaling and are masked off).
        let mut vp8 = b"VP8 ".to_vec();
        vp8.extend_from_slice(&10u32.to_le_bytes());
        vp8.extend_from_slice(&[0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A]);
        vp8.extend_from_slice(&(1920u16 | 0x4000).to_le_bytes());
        vp8.extend_from_slice(&1280u16.to_le_bytes());
        let lossy = riff(&vp8);
        assert_eq!(sniff(&lossy), Some(("image/webp", 1920, 1280)));
        assert!(is_complete(&lossy));
        let mut not_key = lossy.clone();
        not_key[20] |= 1;
        assert_eq!(sniff(&not_key), None, "inter frame");
        let mut bad_start = lossy.clone();
        bad_start[24] = 0;
        assert_eq!(sniff(&bad_start), None);
        assert_eq!(sniff(&lossy[..27]), None, "truncated");

        // VP8X: flags, reserved, 24-bit canvas width-1 and height-1.
        let mut x = b"VP8X".to_vec();
        x.extend_from_slice(&10u32.to_le_bytes());
        x.extend_from_slice(&[0x10, 0, 0, 0]);
        x.extend_from_slice(&(3839u32).to_le_bytes()[..3]);
        x.extend_from_slice(&(2159u32).to_le_bytes()[..3]);
        assert_eq!(sniff(&riff(&x)), Some(("image/webp", 3840, 2160)));

        // VP8L with a non-zero version is invalid.
        let mut l = b"VP8L".to_vec();
        l.extend_from_slice(&5u32.to_le_bytes());
        l.push(0x2F);
        l.extend_from_slice(&(0xE000_0000u32).to_le_bytes());
        assert_eq!(sniff(&riff(&l)), None);
    }

    #[test]
    fn everything_else_is_rejected() {
        assert_eq!(sniff(b""), None);
        assert_eq!(
            sniff(b"<!DOCTYPE html><html><body>Not found</body></html>"),
            None
        );
        assert_eq!(
            sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"/>"),
            None
        );
        assert_eq!(sniff(b"GIF89a\x01\x00\x01\x00\x80\x00\x00"), None);
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WEBPVP9 \x00\x00\x00\x00"), None);
        assert_eq!(sniff(&[0xFF, 0xD8]), None);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert!(!is_complete(b"hello"));
    }
}
