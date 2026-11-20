//! Metadata of an uploaded file: type, dimensions, dates, EXIF (photos) or ffprobe (videos).

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Image => "IMAGE",
            Kind::Video => "VIDEO",
        }
    }
}

/// (kind, mime) from the file extension, falling back to the multipart content type.
pub fn classify(file_name: &str, content_type: Option<&str>) -> Option<(Kind, String)> {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let known = match ext.as_str() {
        "jpg" | "jpeg" => Some((Kind::Image, "image/jpeg")),
        "png" => Some((Kind::Image, "image/png")),
        "webp" => Some((Kind::Image, "image/webp")),
        "gif" => Some((Kind::Image, "image/gif")),
        "heic" => Some((Kind::Image, "image/heic")),
        "heif" => Some((Kind::Image, "image/heif")),
        "avif" => Some((Kind::Image, "image/avif")),
        "dng" => Some((Kind::Image, "image/x-adobe-dng")),
        "tif" | "tiff" => Some((Kind::Image, "image/tiff")),
        "mp4" => Some((Kind::Video, "video/mp4")),
        "mov" => Some((Kind::Video, "video/quicktime")),
        "3gp" => Some((Kind::Video, "video/3gpp")),
        "mkv" => Some((Kind::Video, "video/x-matroska")),
        "webm" => Some((Kind::Video, "video/webm")),
        "avi" => Some((Kind::Video, "video/x-msvideo")),
        "m4v" => Some((Kind::Video, "video/x-m4v")),
        _ => None,
    };
    if let Some((k, m)) = known {
        return Some((k, m.to_string()));
    }
    let ct = content_type?.to_ascii_lowercase();
    if ct.starts_with("image/") {
        Some((Kind::Image, ct))
    } else if ct.starts_with("video/") {
        Some((Kind::Video, ct))
    } else {
        None
    }
}

/// What the file itself says.
#[derive(Debug, Default)]
pub struct Meta {
    /// As displayed (orientation applied).
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    /// Wall-clock capture time (photos: EXIF DateTimeOriginal).
    pub local_taken: Option<NaiveDateTime>,
    /// UTC offset of `local_taken` when the file states it (EXIF OffsetTimeOriginal).
    pub offset: Option<FixedOffset>,
    /// Capture instant when the file only has UTC (videos: creation_time).
    pub taken_utc: Option<DateTime<Utc>>,
    /// SyncAssetExifV1 keys known for this file.
    pub exif: Map<String, Value>,
}

pub fn read_meta(path: &Path, kind: Kind) -> Meta {
    let mut meta = match kind {
        Kind::Image => read_image_meta(path),
        Kind::Video => read_video_meta(path),
    };
    if let Ok(md) = std::fs::metadata(path) {
        meta.exif.insert("fileSizeInByte".into(), json!(md.len()));
    }
    meta
}

// ---------- photos ----------

fn ascii(f: &exif::Field) -> Option<String> {
    match &f.value {
        exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn rational(f: &exif::Field, i: usize) -> Option<f64> {
    match &f.value {
        exif::Value::Rational(v) => v.get(i).filter(|r| r.denom != 0).map(|r| r.to_f64()),
        exif::Value::SRational(v) => v.get(i).filter(|r| r.denom != 0).map(|r| r.to_f64()),
        // Cameras write RATIONAL; some editors write floats.
        exif::Value::Double(v) => v.get(i).copied(),
        exif::Value::Float(v) => v.get(i).map(|f| *f as f64),
        _ => None,
    }
}

fn gps_coord(e: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag, neg: &str) -> Option<f64> {
    let f = e.get_field(tag, exif::In::PRIMARY)?;
    let (d, m, s) = (rational(f, 0)?, rational(f, 1).unwrap_or(0.0), rational(f, 2).unwrap_or(0.0));
    let mut v = d + m / 60.0 + s / 3600.0;
    if e.get_field(ref_tag, exif::In::PRIMARY).and_then(ascii).is_some_and(|r| r.eq_ignore_ascii_case(neg)) {
        v = -v;
    }
    v.is_finite().then_some(v)
}

fn exif_datetime(e: &exif::Exif, tag: exif::Tag, offset_tag: exif::Tag) -> Option<(NaiveDateTime, Option<FixedOffset>)> {
    let f = e.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Ascii(v) = &f.value else { return None };
    let mut dt = exif::DateTime::from_ascii(v.first()?).ok()?;
    if let Some(off) = e.get_field(offset_tag, exif::In::PRIMARY) {
        if let exif::Value::Ascii(ov) = &off.value {
            if let Some(o) = ov.first() {
                let _ = dt.parse_offset(o);
            }
        }
    }
    let date = NaiveDate::from_ymd_opt(dt.year as i32, dt.month as u32, dt.day as u32)?;
    let naive = date.and_hms_nano_opt(dt.hour as u32, dt.minute as u32, dt.second as u32, dt.nanosecond.unwrap_or(0))?;
    let offset = dt.offset.and_then(|m| FixedOffset::east_opt(m as i32 * 60));
    Some((naive, offset))
}

fn exposure_string(seconds: f64) -> String {
    if seconds > 0.0 && seconds < 1.0 {
        format!("1/{}", (1.0 / seconds).round() as i64)
    } else {
        let s = format!("{seconds:.1}");
        s.trim_end_matches(".0").to_string()
    }
}

fn read_image_meta(path: &Path) -> Meta {
    let mut meta = Meta::default();
    let mut orientation = 1u32;
    if let Ok(file) = std::fs::File::open(path) {
        let mut reader = std::io::BufReader::new(file);
        if let Ok(e) = exif::Reader::new().read_from_container(&mut reader) {
            use exif::{In, Tag};
            let get = |t: Tag| e.get_field(t, In::PRIMARY);
            let x = &mut meta.exif;
            if let Some(f) = get(Tag::Orientation) {
                if let Some(o) = f.value.get_uint(0) {
                    orientation = o;
                    x.insert("orientation".into(), json!(o.to_string()));
                }
            }
            if let Some(w) = get(Tag::PixelXDimension).and_then(|f| f.value.get_uint(0)) {
                x.insert("exifImageWidth".into(), json!(w));
            }
            if let Some(h) = get(Tag::PixelYDimension).and_then(|f| f.value.get_uint(0)) {
                x.insert("exifImageHeight".into(), json!(h));
            }
            for (tag, key) in [(Tag::Make, "make"), (Tag::Model, "model"), (Tag::LensModel, "lensModel"), (Tag::ImageDescription, "description")] {
                if let Some(s) = get(tag).and_then(ascii) {
                    x.insert(key.into(), json!(s));
                }
            }
            if let Some(v) = get(Tag::FNumber).and_then(|f| rational(f, 0)) {
                x.insert("fNumber".into(), json!(v));
            }
            if let Some(v) = get(Tag::FocalLength).and_then(|f| rational(f, 0)) {
                x.insert("focalLength".into(), json!(v));
            }
            if let Some(v) = get(Tag::PhotographicSensitivity).and_then(|f| f.value.get_uint(0)) {
                x.insert("iso".into(), json!(v));
            }
            if let Some(v) = get(Tag::ExposureTime).and_then(|f| rational(f, 0)) {
                x.insert("exposureTime".into(), json!(exposure_string(v)));
            }
            if let (Some(lat), Some(lon)) = (
                gps_coord(&e, Tag::GPSLatitude, Tag::GPSLatitudeRef, "S"),
                gps_coord(&e, Tag::GPSLongitude, Tag::GPSLongitudeRef, "W"),
            ) {
                // (0, 0) = no fix on some phones
                if lat != 0.0 || lon != 0.0 {
                    x.insert("latitude".into(), json!(lat));
                    x.insert("longitude".into(), json!(lon));
                }
            }
            if let Some((local, offset)) = exif_datetime(&e, Tag::DateTimeOriginal, Tag::OffsetTimeOriginal) {
                meta.local_taken = Some(local);
                meta.offset = offset;
            }
            if let Some((local, offset)) = exif_datetime(&e, Tag::DateTime, Tag::OffsetTime) {
                if let Some(utc) = to_utc(local, offset) {
                    x.insert("modifyDate".into(), json!(crate::dto::iso(utc.timestamp_millis())));
                }
            }
        }
    }
    // Real pixel size from the image header (EXIF sizes are sometimes wrong or missing). The format is
    // guessed from the content: the upload is still a `.part` file at this point.
    let header_size = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()).ok().and_then(|r| r.into_dimensions().ok());
    if let Some((w, h)) = header_size {
        let (w, h) = (w as i64, h as i64);
        let sideways = (5..=8).contains(&orientation);
        meta.width = Some(if sideways { h } else { w });
        meta.height = Some(if sideways { w } else { h });
    } else if let (Some(w), Some(h)) = (meta.exif.get("exifImageWidth").and_then(Value::as_i64), meta.exif.get("exifImageHeight").and_then(Value::as_i64)) {
        let sideways = (5..=8).contains(&orientation);
        meta.width = Some(if sideways { h } else { w });
        meta.height = Some(if sideways { w } else { h });
    }
    meta
}

// ---------- videos ----------

fn ffprobe(path: &Path) -> Option<Value> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// "+48.8566+002.3522/" or "+48.8566+002.3522+035.000/" (ISO 6709).
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let s = s.trim().trim_end_matches('/');
    let idx: Vec<usize> = s.char_indices().filter(|(_, c)| *c == '+' || *c == '-').map(|(i, _)| i).collect();
    if idx.len() < 2 {
        return None;
    }
    let end = idx.get(2).copied().unwrap_or(s.len());
    let lat: f64 = s[idx[0]..idx[1]].parse().ok()?;
    let lon: f64 = s[idx[1]..end].parse().ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0 && n > 0.0).then(|| (n / d * 1000.0).round() / 1000.0)
}

fn read_video_meta(path: &Path) -> Meta {
    let mut meta = Meta::default();
    let Some(probe) = ffprobe(path) else {
        tracing::warn!("ffprobe failed for {}", path.display());
        return meta;
    };
    let format = &probe["format"];
    if let Some(d) = format["duration"].as_str().and_then(|s| s.parse::<f64>().ok()) {
        meta.duration_ms = Some((d * 1000.0).round() as i64);
    }
    let tags = &format["tags"];
    if let Some(t) = tags["creation_time"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        let t = t.with_timezone(&Utc);
        // Some cameras write 1904-01-01 / 1970 when the clock is unset.
        if t.timestamp() > 86400 * 365 {
            meta.taken_utc = Some(t);
        }
    }
    let loc = tags["location"].as_str().or_else(|| tags["com.apple.quicktime.location.ISO6709"].as_str());
    if let Some((lat, lon)) = loc.and_then(parse_iso6709) {
        meta.exif.insert("latitude".into(), json!(lat));
        meta.exif.insert("longitude".into(), json!(lon));
    }
    for (key, names) in [
        ("make", ["com.android.manufacturer", "com.apple.quicktime.make"]),
        ("model", ["com.android.model", "com.apple.quicktime.model"]),
    ] {
        if let Some(v) = names.iter().find_map(|n| tags[*n].as_str()).filter(|s| !s.is_empty()) {
            meta.exif.insert(key.into(), json!(v));
        }
    }
    if let Some(stream) = probe["streams"].as_array().and_then(|s| s.iter().find(|s| s["codec_type"] == "video")) {
        let (w, h) = (stream["width"].as_i64(), stream["height"].as_i64());
        let rotation = stream["side_data_list"]
            .as_array()
            .and_then(|l| l.iter().find_map(|sd| sd["rotation"].as_i64()))
            .or_else(|| stream["tags"]["rotate"].as_str().and_then(|r| r.parse().ok()))
            .unwrap_or(0);
        let sideways = rotation.rem_euclid(180) == 90;
        if let (Some(w), Some(h)) = (w, h) {
            meta.exif.insert("exifImageWidth".into(), json!(w));
            meta.exif.insert("exifImageHeight".into(), json!(h));
            meta.width = Some(if sideways { h } else { w });
            meta.height = Some(if sideways { w } else { h });
        }
        if let Some(fps) = stream["avg_frame_rate"].as_str().and_then(parse_rate) {
            meta.exif.insert("fps".into(), json!(fps));
        }
    }
    meta
}

// ---------- dates ----------

fn to_utc(local: NaiveDateTime, offset: Option<FixedOffset>) -> Option<DateTime<Utc>> {
    match offset {
        Some(o) => o.from_local_datetime(&local).single().map(|d| d.with_timezone(&Utc)),
        // No offset in the file: assume the server's time zone (the photos are the owner's).
        None => Local.from_local_datetime(&local).earliest().map(|d| d.with_timezone(&Utc)),
    }
}

fn tz_name(o: FixedOffset) -> String {
    let secs = o.local_minus_utc();
    let (sign, secs) = if secs < 0 { ('-', -secs) } else { ('+', secs) };
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if m == 0 { format!("UTC{sign}{h}") } else { format!("UTC{sign}{h}:{m:02}") }
}

pub fn fmt_local(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()
}

/// Final dates of an asset: (fileCreatedAt UTC, localDateTime wall clock, timeZone).
///
/// File metadata wins over what the phone sends: EXIF DateTimeOriginal (+ its offset), then the
/// video creation time, then the phone's fileCreatedAt, then now.
pub fn resolve_dates(meta: &Meta, app_created: Option<DateTime<Utc>>) -> (DateTime<Utc>, NaiveDateTime, Option<String>) {
    if let Some(local) = meta.local_taken {
        if let Some(utc) = to_utc(local, meta.offset) {
            return (utc, local, meta.offset.map(tz_name));
        }
    }
    let utc = meta.taken_utc.or(app_created).unwrap_or_else(Utc::now);
    (utc, utc.with_timezone(&Local).naive_local(), None)
}

/// Parses the app's ISO dates ("2024-05-01T12:03:22.000Z").
pub fn parse_app_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_files() {
        assert_eq!(classify("PXL_1.jpg", None).unwrap().0, Kind::Image);
        assert_eq!(classify("PXL_1.MP4", None).unwrap(), (Kind::Video, "video/mp4".to_string()));
        assert_eq!(classify("x.bin", Some("image/x-foo")).unwrap().0, Kind::Image);
        assert!(classify("x.bin", Some("application/pdf")).is_none());
    }

    #[test]
    fn iso6709() {
        assert_eq!(parse_iso6709("+48.8566+002.3522/"), Some((48.8566, 2.3522)));
        assert_eq!(parse_iso6709("-33.8688+151.2093+012.000/"), Some((-33.8688, 151.2093)));
        assert_eq!(parse_iso6709("garbage"), None);
    }

    #[test]
    fn exposure() {
        assert_eq!(exposure_string(1.0 / 120.0), "1/120");
        assert_eq!(exposure_string(2.0), "2");
        assert_eq!(exposure_string(1.5), "1.5");
    }

    #[test]
    fn dates_with_offset() {
        let local = NaiveDate::from_ymd_opt(2024, 5, 1).unwrap().and_hms_opt(14, 3, 22).unwrap();
        let meta = Meta { local_taken: Some(local), offset: FixedOffset::east_opt(7200), ..Default::default() };
        let (utc, l, tz) = resolve_dates(&meta, None);
        assert_eq!(utc.to_rfc3339(), "2024-05-01T12:03:22+00:00");
        assert_eq!(fmt_local(l), "2024-05-01T14:03:22.000");
        assert_eq!(tz.as_deref(), Some("UTC+2"));
    }

    #[test]
    fn dates_fallback_to_app() {
        let app = parse_app_date("2023-01-02T03:04:05.000Z").unwrap();
        let (utc, _, tz) = resolve_dates(&Meta::default(), Some(app));
        assert_eq!(utc, app);
        assert_eq!(tz, None);
        let video = Meta { taken_utc: parse_app_date("2022-06-01T10:00:00Z"), ..Default::default() };
        assert_eq!(resolve_dates(&video, Some(app)).0, parse_app_date("2022-06-01T10:00:00Z").unwrap());
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn has_ffmpeg() -> bool {
        Command::new("ffprobe").arg("-version").output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn reads_photo_exif() {
        // 64x48 JPEG, orientation 6, Paris GPS, values written as floats by PIL
        let m = read_meta(&fixture("exif.jpg"), Kind::Image);
        assert_eq!((m.width, m.height), (Some(48), Some(64)), "orientation 6 swaps the sides");
        assert_eq!(m.offset, FixedOffset::east_opt(7200));
        assert_eq!(fmt_local(m.local_taken.unwrap()), "2024-05-01T14:03:22.000");
        let x = &m.exif;
        assert_eq!((x["make"].as_str(), x["model"].as_str()), (Some("Google"), Some("Pixel 8a")));
        assert_eq!(x["orientation"], "6");
        assert_eq!(x["iso"], 100);
        assert_eq!(x["exposureTime"], "1/120");
        assert!((x["fNumber"].as_f64().unwrap() - 1.7).abs() < 1e-9);
        assert!((x["latitude"].as_f64().unwrap() - 48.856_666).abs() < 1e-5);
        assert!((x["longitude"].as_f64().unwrap() - 2.3522).abs() < 1e-5);
    }

    #[test]
    fn reads_video_meta() {
        if !has_ffmpeg() {
            eprintln!("ffprobe not installed: skipped");
            return;
        }
        let m = read_meta(&fixture("clip.mp4"), Kind::Video);
        assert_eq!((m.width, m.height, m.duration_ms), (Some(64), Some(36), Some(1000)));
        assert_eq!(m.taken_utc, parse_app_date("2024-06-15T08:30:00Z"));
        assert_eq!(m.exif["fps"], 10.0);
    }

    #[test]
    fn tz_names() {
        assert_eq!(tz_name(FixedOffset::east_opt(-5 * 3600).unwrap()), "UTC-5");
        assert_eq!(tz_name(FixedOffset::east_opt(19800).unwrap()), "UTC+5:30");
    }
}
