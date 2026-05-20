use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Result of audio file enumeration (SPEC §4.1 step 1-2, §4.8).
#[derive(Debug, Default)]
pub struct WalkResult {
    pub audio_files: Vec<PathBuf>,
    pub skipped: Vec<SkippedFile>,
}

#[derive(Debug)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug)]
pub enum SkipReason {
    UnsupportedExtension,
    ZeroSize,
    /// Following a symlink resolved to a path outside library_root.
    /// SPEC §4.8 follows organizational symlinks, but escaping outside the
    /// library is a potential path-traversal vector, so reject explicitly
    /// (security §1).
    OutsideLibraryRoot,
}

impl SkipReason {
    /// Reason string used in the ScanReport JSON (SPEC §4.7).
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::UnsupportedExtension => "unsupported_extension",
            SkipReason::ZeroSize => "zero_size",
            SkipReason::OutsideLibraryRoot => "outside_library_root",
        }
    }
}

/// Recursively enumerate `root` and push audio files matching `extensions`
/// (no leading dot) into `audio_files`. Hidden entries (starting with `.`),
/// both files and directories, are pruned and not emitted in the scan
/// report (SPEC §4.8: `.DS_Store`, `.git/`, etc.). Symlinks are followed.
///
/// `extensions` is expected to be lowercased by the caller, but comparison
/// uses `eq_ignore_ascii_case` so mixed case still works.
pub fn walk(root: &Path, extensions: &[String]) -> WalkResult {
    let mut result = WalkResult::default();

    // Canonicalize root once to use as the comparison baseline. On failure,
    // use the given root as-is (subsequent symlink checks become a no-op —
    // backward compatible).
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    let walker = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            // root (depth 0) is the user-specified location, so always allow it.
            // Deeper entries starting with `.` (`.DS_Store`, `.git`, etc.) are pruned.
            e.depth() == 0
                || e.file_name()
                    .to_str()
                    .map(|s| !s.starts_with('.'))
                    .unwrap_or(true)
        });

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        let ext_match = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                extensions
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(e))
            })
            .unwrap_or(false);

        if !ext_match {
            result.skipped.push(SkippedFile {
                path: path.to_path_buf(),
                reason: SkipReason::UnsupportedExtension,
            });
            continue;
        }

        let zero_size = entry.metadata().map(|m| m.len() == 0).unwrap_or(false);
        if zero_size {
            result.skipped.push(SkippedFile {
                path: path.to_path_buf(),
                reason: SkipReason::ZeroSize,
            });
            continue;
        }

        // Reject symlinks whose target lies outside library_root (security §1).
        // Entries whose canonicalize fails are also rejected (fail-safe).
        match std::fs::canonicalize(path) {
            Ok(real) if real.starts_with(&canonical_root) => {
                result.audio_files.push(path.to_path_buf());
            }
            _ => {
                result.skipped.push(SkippedFile {
                    path: path.to_path_buf(),
                    reason: SkipReason::OutsideLibraryRoot,
                });
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn extensions() -> Vec<String> {
        ["flac", "wav", "mp3", "m4a", "aiff", "aif"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn touch(dir: &std::path::Path, rel: &str, content: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn w1_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let r = walk(tmp.path(), &extensions());
        assert!(r.audio_files.is_empty());
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn w2_single_flac() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "song.flac", b"audio");
        let r = walk(tmp.path(), &extensions());
        assert_eq!(r.audio_files.len(), 1);
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn w3_flac_and_txt() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "song.flac", b"audio");
        touch(tmp.path(), "notes.txt", b"text");
        let r = walk(tmp.path(), &extensions());
        assert_eq!(r.audio_files.len(), 1);
        assert_eq!(r.skipped.len(), 1);
        assert!(matches!(
            r.skipped[0].reason,
            SkipReason::UnsupportedExtension
        ));
    }

    #[test]
    fn w4_hidden_file_pruned() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), ".DS_Store", b"meta");
        let r = walk(tmp.path(), &extensions());
        assert!(r.audio_files.is_empty());
        assert!(
            r.skipped.is_empty(),
            "hidden files should be pruned, not skipped"
        );
    }

    #[test]
    fn w5_hidden_dir_pruned() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), ".git/HEAD", b"ref");
        touch(tmp.path(), ".git/hidden.flac", b"audio");
        let r = walk(tmp.path(), &extensions());
        assert!(r.audio_files.is_empty());
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn w6_zero_size_flac() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "empty.flac", b"");
        let r = walk(tmp.path(), &extensions());
        assert!(r.audio_files.is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(matches!(r.skipped[0].reason, SkipReason::ZeroSize));
    }

    #[test]
    fn w7_nested_dirs() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "sub/a.flac", b"audio");
        touch(tmp.path(), "sub/deeper/b.mp3", b"audio");
        let r = walk(tmp.path(), &extensions());
        assert_eq!(r.audio_files.len(), 2);
    }

    #[test]
    fn w8_case_insensitive_ext() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "song.FLAC", b"audio");
        let r = walk(tmp.path(), &extensions());
        assert_eq!(r.audio_files.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn w9_symlink_inside_library_is_accepted() {
        // Legitimate case: symlink within library_root resolving to the same root
        // (the consolidation pattern from SPEC §4.8)
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "real/track.flac", b"audio");
        std::os::unix::fs::symlink(
            tmp.path().join("real/track.flac"),
            tmp.path().join("linked.flac"),
        )
        .unwrap();

        let r = walk(tmp.path(), &extensions());
        // Both the real file and the symlink to the same backing object end up in audio_files
        assert_eq!(r.audio_files.len(), 2);
    }

    #[test]
    fn w11_unicode_filenames_collected() {
        // File / directory names with CJK / emoji must be collected into audio_files.
        // UPnP assumes UTF-8 paths, so dropping them server-side causes incidents
        // like "Japanese titles silently disappear".
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "ジャズ/コルトレーン.flac", b"audio");
        touch(tmp.path(), "🎵 favorites/track.flac", b"audio");
        touch(tmp.path(), "Björk - Vespertine/01.flac", b"audio");
        let r = walk(tmp.path(), &extensions());
        assert_eq!(r.audio_files.len(), 3, "unicode filenames must be collected");
        // Every file is reachable as a real object (canonicalize succeeds on UTF-8)
        for path in &r.audio_files {
            assert!(path.exists(), "discovered path should exist: {:?}", path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn w10_symlink_outside_library_is_skipped() {
        // Symlinks pointing to a real object outside library_root are rejected as OutsideLibraryRoot (security §1)
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.flac"), b"audio").unwrap();

        let lib = TempDir::new().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.flac"),
            lib.path().join("link.flac"),
        )
        .unwrap();

        let r = walk(lib.path(), &extensions());
        assert!(r.audio_files.is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(matches!(
            r.skipped[0].reason,
            SkipReason::OutsideLibraryRoot
        ));
    }
}
