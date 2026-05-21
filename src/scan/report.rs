use serde::Serialize;
use uuid::Uuid;

/// Structured scan report per SPEC §4.7. Persisted as JSON in
/// `server_state.last_scan_report` and returned by `GET /admin/scan-report`
/// (the latter is implemented in the HTTP commit).
///
/// `error` is written only when the scan fails mid-run (ops §P1). On success
/// it is omitted via `serde(skip_serializing_if = "Option::is_none")`,
/// preserving compatibility with the existing JSON shape.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub scan_id: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub duration_ms: u64,
    pub is_initial: bool,
    pub stats: ScanStats,
    pub issues: Vec<Issue>,
    pub skipped: Vec<SkippedEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ScanStats {
    pub files_enumerated: usize,
    pub tracks_inserted: usize,
    pub tracks_updated: usize,
    pub tracks_unchanged: usize,
    pub tracks_deleted: usize,
    pub albums_inserted: usize,
    pub albums_deleted: usize,
    pub tag_read_failed: usize,
    /// Album art / sidecars (`Folder.jpg`, `*.log`, `*.cue`, …) seen during
    /// walk. Counted only — paths are not enumerated in `skipped` (#19).
    pub companion_files_seen: usize,
}

/// Tag issue (playable but needs correction).
#[derive(Debug, Serialize)]
pub struct Issue {
    pub path: String,
    pub issue: &'static str,
}

/// Skipped from scan. `reason` matches the suffix strings in SPEC §4.7.
#[derive(Debug, Serialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: &'static str,
}

impl ScanReport {
    pub fn new_id() -> String {
        Uuid::new_v4().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp1_scan_report_serializes_with_expected_keys() {
        let report = ScanReport {
            scan_id: "abc".into(),
            started_at: 1,
            completed_at: 2,
            duration_ms: 1000,
            is_initial: true,
            stats: ScanStats::default(),
            issues: vec![],
            skipped: vec![],
            error: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        for key in &[
            "scan_id",
            "started_at",
            "completed_at",
            "duration_ms",
            "is_initial",
            "stats",
            "issues",
            "skipped",
            "files_enumerated",
            "tracks_inserted",
            "tracks_updated",
            "tracks_unchanged",
            "tracks_deleted",
            "albums_inserted",
            "albums_deleted",
            "tag_read_failed",
            "companion_files_seen",
        ] {
            assert!(json.contains(key), "missing key in JSON: {}", key);
        }
    }

    #[test]
    fn rp3_error_field_is_omitted_when_none() {
        // The `error` key must not appear in a successful report's JSON (backward compatible)
        let report = ScanReport {
            scan_id: "ok".into(),
            started_at: 0,
            completed_at: 0,
            duration_ms: 0,
            is_initial: false,
            stats: ScanStats::default(),
            issues: vec![],
            skipped: vec![],
            error: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("\"error\""),
            "error key should be omitted: {}",
            json
        );
    }

    #[test]
    fn rp4_error_field_serializes_when_some() {
        let report = ScanReport {
            scan_id: "fail".into(),
            started_at: 0,
            completed_at: 0,
            duration_ms: 0,
            is_initial: false,
            stats: ScanStats::default(),
            issues: vec![],
            skipped: vec![],
            error: Some("disk full".into()),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""error":"disk full""#));
    }

    #[test]
    fn rp2_issue_and_skipped_serialize_correctly() {
        let issue = Issue {
            path: "/x/y.flac".into(),
            issue: "missing_album",
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains(r#""path":"/x/y.flac""#));
        assert!(json.contains(r#""issue":"missing_album""#));

        // `.txt` is now a companion file and would be aggregated into the
        // counter, so use a non-companion extension to keep the
        // SkippedEntry serialization test on the `unsupported_extension` path (#19).
        let s = SkippedEntry {
            path: "/x/stray.exe".into(),
            reason: "unsupported_extension",
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""reason":"unsupported_extension""#));
    }
}
