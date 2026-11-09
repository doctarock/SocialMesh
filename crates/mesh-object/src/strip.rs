//! Remove identifying metadata from files before they are attached.
//!
//! Photos and documents often carry more than their visible content: GPS
//! coordinates, the time taken, camera or phone model and serial, editing
//! software, the author's name, embedded thumbnails (which can show what was
//! cropped out). For someone posting from a protest or leaking a document,
//! that metadata can identify them however well the network hides them.
//!
//! The approach is an **allowlist**: keep only the parts a viewer needs to
//! display the image, drop everything else, including data hidden after the
//! end of the image. Formats we can't clean safely are reported as
//! [`Strip::Unsupported`]; callers should refuse them unless the user insists.

/// Outcome of [`strip_metadata`].
#[derive(Debug, PartialEq, Eq)]
pub enum Strip {
    /// Cleaned copy. `removed` describes what was dropped (empty: none found).
    Cleaned { data: Vec<u8>, removed: Vec<String> },
    /// This type carries no metadata we know of (plain text and similar).
    NothingToStrip,
    /// May carry metadata we can't remove (PDF, video, office documents, HEIC…).
    Unsupported,
}

/// Strip metadata from `data` of type `media_type`.
///
/// Returns `Err` if the file doesn't parse as the format it claims to be:
/// attaching it unchecked could leak whatever we failed to parse.
pub fn strip_metadata(media_type: &str, data: &[u8]) -> Result<Strip, String> {
    let (data, removed) = match media_type {
        "image/jpeg" => jpeg(data)?,
        "image/png" => png(data)?,
        "image/webp" => webp(data)?,
        "text/plain" | "text/markdown" | "text/csv" | "application/json" => return Ok(Strip::NothingToStrip),
        _ => return Ok(Strip::Unsupported),
    };
    Ok(Strip::Cleaned { data, removed })
}

fn be16(d: &[u8], i: usize) -> Result<usize, String> {
    d.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| "truncated".to_string())
}

fn trailing(n: usize) -> String {
    format!("{n} bytes of hidden data after the end of the image")
}

// ---- JPEG -------------------------------------------------------------------

fn jpeg(d: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return Err("not a JPEG".into());
    }
    let mut out = vec![0xFF, 0xD8];
    let mut removed = Vec::new();
    let mut i = 2;
    loop {
        if d.get(i) != Some(&0xFF) {
            return Err("malformed JPEG (expected a marker)".into());
        }
        while d.get(i + 1) == Some(&0xFF) {
            i += 1; // fill bytes
        }
        let m = *d.get(i + 1).ok_or("truncated JPEG")?;
        i += 2;
        match m {
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                if i < d.len() {
                    // Phones append extra images here (MPF: depth maps, previews).
                    removed.push(trailing(d.len() - i));
                }
                return Ok((out, removed));
            }
            0xD0..=0xD7 | 0x01 => out.extend_from_slice(&[0xFF, m]),
            _ => {
                let len = be16(d, i)?;
                let seg = d.get(i..i + len).filter(|_| len >= 2).ok_or("truncated JPEG segment")?;
                let payload = &seg[2..];
                let keep = match m {
                    0xE0 => payload.starts_with(b"JFIF\0") || payload.starts_with(b"JFXX\0"),
                    0xE2 => payload.starts_with(b"ICC_PROFILE\0"),
                    0xEE => payload.starts_with(b"Adobe"),
                    0xE1..=0xEF | 0xFE => false,
                    _ => true, // image data: tables, frame and scan headers
                };
                if keep {
                    out.extend_from_slice(&[0xFF, m]);
                    out.extend_from_slice(seg);
                } else {
                    removed.push(describe_jpeg(m, payload));
                }
                i += len;
                if m == 0xDA {
                    // Entropy-coded scan data runs to the next real marker.
                    let start = i;
                    loop {
                        match (d.get(i), d.get(i + 1)) {
                            (Some(0xFF), Some(&n)) if n != 0 && !(0xD0..=0xD7).contains(&n) => break,
                            (Some(_), Some(_)) => i += 1,
                            _ => return Err("truncated JPEG scan".into()),
                        }
                    }
                    out.extend_from_slice(&d[start..i]);
                }
            }
        }
    }
}

fn describe_jpeg(m: u8, payload: &[u8]) -> String {
    if payload.starts_with(b"Exif\0") {
        "EXIF (GPS location, time, device, thumbnail)".into()
    } else if payload.starts_with(b"http://ns.adobe.com/xap") || payload.starts_with(b"http://ns.adobe.com/xmp") {
        "XMP (editing history, author, location)".into()
    } else if m == 0xED {
        "IPTC / Photoshop (captions, author, location)".into()
    } else if m == 0xFE {
        "comment".into()
    } else if payload.starts_with(b"MPF\0") {
        "multi-picture index (extra embedded images)".into()
    } else {
        format!("APP{} segment", m - 0xE0)
    }
}

// ---- PNG --------------------------------------------------------------------

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_KEEP: &[&[u8; 4]] = &[
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"bKGD", b"pHYs",
    b"acTL", b"fcTL", b"fdAT",
];

fn png(d: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    if !d.starts_with(PNG_SIG) {
        return Err("not a PNG".into());
    }
    let mut out = PNG_SIG.to_vec();
    let mut removed = Vec::new();
    let mut i = PNG_SIG.len();
    loop {
        let len = d.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("truncated PNG")?;
        let chunk = d.get(i..i + 12 + len).ok_or("truncated PNG chunk")?;
        let kind: [u8; 4] = chunk[4..8].try_into().expect("4 bytes");
        if PNG_KEEP.contains(&&kind) {
            out.extend_from_slice(chunk);
        } else {
            removed.push(match &kind {
                b"tEXt" | b"zTXt" | b"iTXt" => "text (author, software, comments)".into(),
                b"eXIf" => "EXIF (GPS location, time, device)".into(),
                b"tIME" => "last-modified time".into(),
                k => format!("{} chunk", String::from_utf8_lossy(k)),
            });
        }
        i += 12 + len;
        if &kind == b"IEND" {
            if i < d.len() {
                removed.push(trailing(d.len() - i));
            }
            return Ok((out, removed));
        }
    }
}

// ---- WebP -------------------------------------------------------------------

const WEBP_KEEP: &[&[u8; 4]] = &[b"VP8 ", b"VP8L", b"VP8X", b"ALPH", b"ANIM", b"ANMF", b"ICCP"];
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

fn webp(d: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    if d.len() < 12 || &d[..4] != b"RIFF" || &d[8..12] != b"WEBP" {
        return Err("not a WebP".into());
    }
    let riff_len = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
    let end = (8 + riff_len).min(d.len());
    let mut body = b"WEBP".to_vec();
    let mut removed = Vec::new();
    let mut i = 12;
    while i < end {
        let hdr = d.get(i..i + 8).ok_or("truncated WebP chunk")?;
        let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        let padded = len + (len & 1);
        let chunk = d.get(i..i + 8 + padded).ok_or("truncated WebP chunk")?;
        let kind: [u8; 4] = hdr[..4].try_into().expect("4 bytes");
        if WEBP_KEEP.contains(&&kind) {
            let start = body.len();
            body.extend_from_slice(chunk);
            if &kind == b"VP8X" && len > 0 {
                body[start + 8] &= !(VP8X_EXIF | VP8X_XMP);
            }
        } else {
            removed.push(match &kind {
                b"EXIF" => "EXIF (GPS location, time, device)".into(),
                b"XMP " => "XMP (editing history, author, location)".into(),
                k => format!("{} chunk", String::from_utf8_lossy(k).trim_end()),
            });
        }
        i += 8 + padded;
    }
    if d.len() > 8 + riff_len {
        removed.push(trailing(d.len() - 8 - riff_len));
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok((out, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(m: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, m];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn cleaned(s: Strip) -> (Vec<u8>, Vec<String>) {
        match s {
            Strip::Cleaned { data, removed } => (data, removed),
            other => panic!("expected Cleaned, got {other:?}"),
        }
    }

    #[test]
    fn jpeg_drops_exif_xmp_comments_and_trailing_images() {
        let mut f = vec![0xFF, 0xD8];
        f.extend(seg(0xE0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0"));
        f.extend(seg(0xE1, b"Exif\0\0GPS -35.3075 149.1244 iPhone 15"));
        f.extend(seg(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:creator>Jane Citizen</x:creator>"));
        f.extend(seg(0xFE, b"taken outside Parliament House"));
        f.extend(seg(0xE2, b"ICC_PROFILE\0\x01\x01profile"));
        f.extend(seg(0xDB, &[0u8; 65])); // quantisation table
        f.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0])); // scan header
        f.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56]); // scan data with stuffing and a restart marker
        f.extend([0xFF, 0xD9]);
        f.extend(b"\xFF\xD8MPF second image with its own Exif GPS");
        let (out, removed) = cleaned(strip_metadata("image/jpeg", &f).unwrap());
        for needle in [&b"GPS"[..], b"Jane", b"Parliament", b"Exif", b"MPF"] {
            assert!(!out.windows(needle.len()).any(|w| w == needle), "{:?} survived", String::from_utf8_lossy(needle));
        }
        assert!(out.windows(4).any(|w| w == b"JFIF"));
        assert!(out.windows(11).any(|w| w == b"ICC_PROFILE"));
        assert!(out.ends_with(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xD9]), "scan data intact");
        assert_eq!(removed.len(), 4, "{removed:?}");
        // Idempotent: a cleaned file has nothing left to remove.
        let (again, removed) = cleaned(strip_metadata("image/jpeg", &out).unwrap());
        assert_eq!(again, out);
        assert!(removed.is_empty());
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC: copied, not checked
        v
    }

    #[test]
    fn png_keeps_image_drops_text_and_exif() {
        let mut f = PNG_SIG.to_vec();
        f.extend(png_chunk(b"IHDR", &[0; 13]));
        f.extend(png_chunk(b"tEXt", b"Author\0Jane Citizen"));
        f.extend(png_chunk(b"eXIf", b"MM\0*GPS"));
        f.extend(png_chunk(b"IDAT", b"pixels"));
        f.extend(png_chunk(b"IEND", b""));
        f.extend(b"appended secret");
        let (out, removed) = cleaned(strip_metadata("image/png", &f).unwrap());
        assert!(!out.windows(4).any(|w| w == b"Jane"));
        assert!(!out.windows(3).any(|w| w == b"GPS"));
        assert!(!out.windows(4).any(|w| w == b"tEXt" || w == b"eXIf"));
        assert!(!out.windows(6).any(|w| w == b"secret"));
        assert!(out.windows(6).any(|w| w == b"pixels"));
        assert_eq!(removed.len(), 3, "{removed:?}");
    }

    #[test]
    fn webp_drops_exif_and_clears_flags() {
        let mut chunks = Vec::new();
        let mut vp8x = b"VP8X".to_vec();
        vp8x.extend(10u32.to_le_bytes());
        vp8x.extend([VP8X_EXIF | VP8X_XMP | 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        chunks.extend(vp8x);
        let mut vp8 = b"VP8 ".to_vec();
        vp8.extend(3u32.to_le_bytes());
        vp8.extend(b"img\0"); // odd length, padded
        chunks.extend(vp8);
        let mut exif = b"EXIF".to_vec();
        exif.extend(9u32.to_le_bytes());
        exif.extend(b"GPS -35.3\0");
        chunks.extend(exif);
        let mut f = b"RIFF".to_vec();
        f.extend(((4 + chunks.len()) as u32).to_le_bytes());
        f.extend(b"WEBP");
        f.extend(chunks);
        let (out, removed) = cleaned(strip_metadata("image/webp", &f).unwrap());
        assert!(!out.windows(3).any(|w| w == b"GPS"));
        assert_eq!(out[20] & (VP8X_EXIF | VP8X_XMP), 0, "VP8X metadata flags cleared");
        assert_eq!(out[20] & 0x10, 0x10, "other flags kept");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert_eq!(removed, vec!["EXIF (GPS location, time, device)".to_string()]);
    }

    #[test]
    fn other_types() {
        assert_eq!(strip_metadata("text/plain", b"hi").unwrap(), Strip::NothingToStrip);
        assert_eq!(strip_metadata("application/pdf", b"%PDF").unwrap(), Strip::Unsupported);
        assert_eq!(strip_metadata("video/mp4", b"").unwrap(), Strip::Unsupported);
        assert!(strip_metadata("image/jpeg", b"not a jpeg").is_err());
        assert!(strip_metadata("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF]).is_err(), "truncated");
    }
}
