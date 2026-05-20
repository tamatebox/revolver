//! Album art extraction logic (SPEC §8.3).
//!
//! Priority:
//! 1. Embedded picture of the representative track
//!    (`PictureType::CoverFront` -> `Other` -> first).
//! 2. Inside the album folder: `cover.jpg|jpeg|png` -> `folder.*` -> `front.*`.
//! 3. Any other `.jpg|jpeg|png` (stem lexicographic, case-insensitive).
//!
//! All matching is **case-insensitive** (real-world directories mix
//! `Cover.JPG` / `FOLDER.png` etc.).

use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, PictureType};
use lofty::probe::Probe;

/// Tuple type for carrying around `(Vec<u8>, &'static str)` -- `mime` uses
/// `&'static` while the bytes are owned per call.
pub type ExtractedArt = (Vec<u8>, &'static str);

/// Extract an embedded picture from the representative track file (SPEC §8.3).
///
/// Only JPEG / PNG are accepted. Other MIME types (TIFF/BMP/GIF/Unknown) are
/// dropped since Linn cannot do much with them over HTTP.
pub fn extract_embedded(track_path: &Path) -> Option<ExtractedArt> {
    let tagged = Probe::open(track_path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pics = tag.pictures();
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.iter().find(|p| p.pic_type() == PictureType::Other))
        .or_else(|| pics.first())?;
    let mime = match pic.mime_type()? {
        MimeType::Jpeg => "image/jpeg",
        MimeType::Png => "image/png",
        _ => return None,
    };
    Some((pic.data().to_vec(), mime))
}

/// Pick a candidate image from the album folder (SPEC §8.3).
///
/// Returns `(bytes, mime, source_path_string)`. `source_path_string` is for
/// logging / debugging only (not used as a cache key).
pub fn extract_folder(album_dir: &Path) -> Option<(Vec<u8>, &'static str, String)> {
    // Build the set of (path, stem_lc, ext_lc) tuples.
    let entries: Vec<(std::path::PathBuf, String, String)> = std::fs::read_dir(album_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if !p.is_file() {
                return None;
            }
            let stem = p.file_stem()?.to_str()?.to_ascii_lowercase();
            let ext = p.extension()?.to_str()?.to_ascii_lowercase();
            if matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
                Some((p, stem, ext))
            } else {
                None
            }
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    // Priority: cover.jpg/jpeg(1) -> cover.png(2) -> folder.jpg/jpeg(3) -> folder.png(4)
    //        -> front.jpg/jpeg(5) -> front.png(6) -> other jpg/jpeg/png(7)
    fn priority(stem: &str, ext: &str) -> u8 {
        let is_jpg = ext == "jpg" || ext == "jpeg";
        match (stem, is_jpg) {
            ("cover", true) => 1,
            ("cover", false) => 2,
            ("folder", true) => 3,
            ("folder", false) => 4,
            ("front", true) => 5,
            ("front", false) => 6,
            _ => 7,
        }
    }

    let chosen = entries.iter().min_by(|a, b| {
        let pa = priority(&a.1, &a.2);
        let pb = priority(&b.1, &b.2);
        // For ties, sort by stem lexicographically (case-insensitive; already lower-cased).
        pa.cmp(&pb).then_with(|| a.1.cmp(&b.1))
    })?;

    let bytes = std::fs::read(&chosen.0).ok()?;
    let mime = if chosen.2 == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    Some((bytes, mime, chosen.0.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn ef1_folder_with_cover_jpg_picks_it() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("cover.jpg"), b"jpegbytes").unwrap();
        fs::write(dir.path().join("random.jpg"), b"otherbytes").unwrap();
        let (bytes, mime, _) = extract_folder(dir.path()).unwrap();
        assert_eq!(bytes, b"jpegbytes");
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn ef2_folder_with_only_random_jpg_falls_back() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("zzz.jpg"), b"zzzbytes").unwrap();
        let (bytes, mime, _) = extract_folder(dir.path()).unwrap();
        assert_eq!(bytes, b"zzzbytes");
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn ef3_empty_folder_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(extract_folder(dir.path()).is_none());
    }

    #[test]
    fn ef4_case_insensitive_stem_and_ext() {
        let dir = TempDir::new().unwrap();
        // With FOLDER.png (priority 4) and Cover.JPG (priority 1), pick Cover.
        fs::write(dir.path().join("FOLDER.png"), b"folderpng").unwrap();
        fs::write(dir.path().join("Cover.JPG"), b"coverjpg").unwrap();
        let (bytes, mime, source) = extract_folder(dir.path()).unwrap();
        assert_eq!(bytes, b"coverjpg");
        assert_eq!(mime, "image/jpeg");
        assert!(source.ends_with("Cover.JPG"));
    }

    #[test]
    fn ef5_png_only_returned_with_image_png() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("cover.png"), b"pngbytes").unwrap();
        let (bytes, mime, _) = extract_folder(dir.path()).unwrap();
        assert_eq!(bytes, b"pngbytes");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn ef6_non_image_files_ignored() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        fs::write(dir.path().join("track.flac"), b"flac").unwrap();
        assert!(extract_folder(dir.path()).is_none());
    }

    #[test]
    fn ee1_extract_embedded_on_missing_file_returns_none() {
        // Even on a path where Probe::open fails, return None without panicking.
        let missing = std::path::Path::new("/no/such/path/xyz.flac");
        assert!(extract_embedded(missing).is_none());
    }
}
