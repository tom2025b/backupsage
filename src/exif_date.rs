//! EXIF capture-date extraction and media-kind classification.
//!
//! Date precedence mirrors photo-organizer: DateTimeOriginal >
//! DateTimeDigitized > DateTime. Times are treated as UTC — EXIF rarely
//! carries a reliable timezone and we refuse to guess; the value is for
//! *comparing copies of the same photo*, where any consistent reading works.

use std::io::Cursor;

/// Media classification by file extension (photo-organizer's lists plus
/// .tif/.tiff images and .cr3 RAW).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Raw,
    Video,
    Other,
}

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "heic", "webp", "tif", "tiff"];
const RAW_EXTS: &[&str] = &["nef", "cr2", "cr3", "arw", "dng"];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv"];

/// Formats the `image` crate can decode in v1.0 → eligible for pHash.
/// HEIC is an image but needs libheif (v1.1 feature); RAW needs rawler.
pub fn is_decodable_image(path: &str) -> bool {
    matches!(
        ext_lower(path).as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp" | "tif" | "tiff")
    )
}

pub fn media_kind(path: &str) -> MediaKind {
    match ext_lower(path).as_deref() {
        Some(e) if IMAGE_EXTS.contains(&e) => MediaKind::Image,
        Some(e) if RAW_EXTS.contains(&e) => MediaKind::Raw,
        Some(e) if VIDEO_EXTS.contains(&e) => MediaKind::Video,
        _ => MediaKind::Other,
    }
}

fn ext_lower(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
}

/// A capture date plus which EXIF field it came from (`exif_src` in the
/// schema — provenance is shown to the user, spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExifDate {
    pub unix: i64,
    pub src: &'static str,
}

/// Extract the best capture date from an in-memory EXIF container
/// (JPEG, TIFF-based RAW, PNG, WebP, HEIF — whatever kamadak-exif reads).
pub fn extract_date(buf: &[u8]) -> Option<ExifDate> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(buf))
        .ok()?;
    for (tag, name) in [
        (exif::Tag::DateTimeOriginal, "DateTimeOriginal"),
        (exif::Tag::DateTimeDigitized, "DateTimeDigitized"),
        (exif::Tag::DateTime, "DateTime"),
    ] {
        if let Some(field) = exif.get_field(tag, exif::In::PRIMARY) {
            if let exif::Value::Ascii(ref groups) = field.value {
                if let Some(first) = groups.first() {
                    if let Some(unix) = parse_exif_datetime(first) {
                        return Some(ExifDate { unix, src: name });
                    }
                }
            }
        }
    }
    None
}

/// Parse `"YYYY:MM:DD HH:MM:SS"` to a unix timestamp (UTC).
fn parse_exif_datetime(raw: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(raw).ok()?.trim_end_matches('\0').trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b':' || b[7] != b':' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // "0000:00:00 00:00:00" and similar placeholders are absence, not data.
    if y == 0 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-days algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Handcrafted little-endian TIFF with an EXIF sub-IFD ────────────────
    fn u16le(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn u32le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn ifd_entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
        let mut e = Vec::with_capacity(12);
        e.extend(u16le(tag));
        e.extend(u16le(typ));
        e.extend(u32le(count));
        e.extend(u32le(value));
        e
    }

    /// TIFF: IFD0 { DateTime, →ExifIFD }, ExifIFD { DateTimeOriginal,
    /// DateTimeDigitized }; each date a 20-byte ASCII string.
    fn tiff_with_dates(original: &str, digitized: &str, plain: &str) -> Vec<u8> {
        const STR_A: u32 = 68; // DateTime
        const STR_B: u32 = 88; // DateTimeOriginal
        const STR_C: u32 = 108; // DateTimeDigitized
        let mut t = Vec::new();
        t.extend(b"II");
        t.extend(u16le(42));
        t.extend(u32le(8)); // IFD0 at 8
        t.extend(u16le(2)); // IFD0: 2 entries
        t.extend(ifd_entry(0x0132, 2, 20, STR_A)); // DateTime
        t.extend(ifd_entry(0x8769, 4, 1, 38)); // Exif IFD pointer → 38
        t.extend(u32le(0)); // next IFD: none  (ends at 38)
        t.extend(u16le(2)); // Exif IFD: 2 entries
        t.extend(ifd_entry(0x9003, 2, 20, STR_B)); // DateTimeOriginal
        t.extend(ifd_entry(0x9004, 2, 20, STR_C)); // DateTimeDigitized
        t.extend(u32le(0)); // next IFD: none  (ends at 68)
        for s in [plain, original, digitized] {
            let mut bytes = s.as_bytes().to_vec();
            bytes.resize(20, 0);
            t.extend(bytes);
        }
        assert_eq!(t.len(), 128);
        t
    }

    #[test]
    fn prefers_datetime_original() {
        let tiff = tiff_with_dates(
            "2023:07:04 19:22:33",
            "2024:01:01 00:00:00",
            "2025:01:01 00:00:00",
        );
        let d = extract_date(&tiff).expect("date parsed");
        assert_eq!(d.src, "DateTimeOriginal");
        assert_eq!(d.unix, 1_688_498_553); // 2023-07-04T19:22:33Z
    }

    #[test]
    fn falls_back_along_the_precedence_chain() {
        // Original invalid → Digitized wins; both invalid → DateTime.
        let t1 = tiff_with_dates("garbage", "2024:02:29 12:00:00", "2025:01:01 00:00:00");
        assert_eq!(extract_date(&t1).unwrap().src, "DateTimeDigitized");
        let t2 = tiff_with_dates("garbage", "garbage", "2020:06:15 08:30:00");
        let d = extract_date(&t2).unwrap();
        assert_eq!(d.src, "DateTime");
        assert_eq!(d.unix, 1_592_209_800);
    }

    #[test]
    fn garbage_and_zero_dates_are_none() {
        assert!(extract_date(b"not exif at all").is_none());
        let zeros = tiff_with_dates("0000:00:00 00:00:00", "garbage", "garbage");
        assert!(extract_date(&zeros).is_none());
    }

    #[test]
    fn civil_days_roundtrip_epoch_and_leap() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn media_kind_tables() {
        assert_eq!(media_kind("a/b/IMG.JPG"), MediaKind::Image);
        assert_eq!(media_kind("x.heic"), MediaKind::Image);
        assert_eq!(media_kind("x.CR3"), MediaKind::Raw);
        assert_eq!(media_kind("x.nef"), MediaKind::Raw);
        assert_eq!(media_kind("v.MOV"), MediaKind::Video);
        assert_eq!(media_kind("doc.pdf"), MediaKind::Other);
        assert_eq!(media_kind("no_extension"), MediaKind::Other);
        assert!(is_decodable_image("p.WebP"));
        assert!(!is_decodable_image("p.heic")); // v1.1: libheif feature
        assert!(!is_decodable_image("p.nef"));
    }
}
