use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::ItemKey;

/// Tags + audio properties + codec info extracted from a single track
/// (corresponds to the tracks table in SPEC §3.1).
#[derive(Debug, Clone)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub compilation: bool,
    pub track_num: Option<u32>,
    pub disc_num: Option<u32>,
    /// COMPOSER / TCOM / ©wrt — for classical library browsing (#9).
    pub composer: Option<String>,
    /// CONDUCTOR / TPE3 — for classical library browsing (#9).
    pub conductor: Option<String>,
    /// PERFORMER / TOPE / ©prf — orchestra / ensemble (#9).
    pub performer: Option<String>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub codec: String,
    pub mime_type: String,
    pub file_size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("lofty error parsing {path}: {source}")]
    Lofty {
        path: std::path::PathBuf,
        #[source]
        source: lofty::error::LoftyError,
    },
}

/// Read tags and audio properties from `path`. For M4A, re-parse the container
/// to determine ALAC vs AAC (SPEC §14).
pub fn read(path: &Path) -> Result<TrackTags, TagError> {
    let file_size = std::fs::metadata(path)
        .map_err(|source| TagError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    let tagged_file = Probe::open(path)
        .map_err(|source| TagError::Lofty {
            path: path.to_path_buf(),
            source,
        })?
        .read()
        .map_err(|source| TagError::Lofty {
            path: path.to_path_buf(),
            source,
        })?;

    let file_type = tagged_file.file_type();
    let props = tagged_file.properties();
    let tag = tagged_file.primary_tag();

    let codec = codec_for(file_type, path);
    let mime_type = mime_for(file_type).to_string();
    let bit_depth = if codec_is_lossless(&codec) {
        props.bit_depth()
    } else {
        None
    };

    let (
        title,
        artist,
        album_artist,
        album,
        genre,
        compilation,
        track_num,
        disc_num,
        composer,
        conductor,
        performer,
    ) = if let Some(t) = tag {
        (
            t.get_string(ItemKey::TrackTitle).map(String::from),
            t.get_string(ItemKey::TrackArtist).map(String::from),
            t.get_string(ItemKey::AlbumArtist).map(String::from),
            t.get_string(ItemKey::AlbumTitle).map(String::from),
            t.get_string(ItemKey::Genre).map(String::from),
            t.get_string(ItemKey::FlagCompilation)
                .map(parse_bool_flag)
                .unwrap_or(false),
            t.get_string(ItemKey::TrackNumber)
                .and_then(parse_num_prefix),
            t.get_string(ItemKey::DiscNumber).and_then(parse_num_prefix),
            t.get_string(ItemKey::Composer).map(String::from),
            t.get_string(ItemKey::Conductor).map(String::from),
            t.get_string(ItemKey::Performer).map(String::from),
        )
    } else {
        (
            None, None, None, None, None, false, None, None, None, None, None,
        )
    };

    Ok(TrackTags {
        title,
        artist,
        album_artist,
        album,
        genre,
        compilation,
        track_num,
        disc_num,
        composer,
        conductor,
        performer,
        duration_ms: Some(props.duration().as_millis() as u64),
        sample_rate: props.sample_rate(),
        bit_depth,
        channels: props.channels(),
        bitrate: props.audio_bitrate(),
        codec,
        mime_type,
        file_size,
    })
}

/// Extract the leading u32 from a "current/total" string such as "3/12".
fn parse_num_prefix(s: &str) -> Option<u32> {
    s.split('/').next()?.trim().parse().ok()
}

/// Normalize the `cpil` / `TCMP` / `COMPILATION` flag representations to bool.
fn parse_bool_flag(s: &str) -> bool {
    let s = s.trim();
    s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
}

fn codec_for(ft: FileType, path: &Path) -> String {
    match ft {
        FileType::Flac => "flac".to_string(),
        FileType::Mpeg => "mp3".to_string(),
        FileType::Wav => "pcm".to_string(),
        FileType::Aiff => "pcm".to_string(),
        FileType::Mp4 => detect_mp4_codec(path),
        _ => "unknown".to_string(),
    }
}

/// Re-parse the M4A container to determine ALAC vs AAC (SPEC §14).
/// On failure, fall back to AAC (treating ALAC as AAC just drops bit_depth;
/// playback still works).
fn detect_mp4_codec(path: &Path) -> String {
    use lofty::mp4::{Mp4Codec, Mp4File};
    use std::fs::File;
    use std::io::BufReader;

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return "aac".to_string(),
    };
    let mut reader = BufReader::new(file);
    match <Mp4File as AudioFile>::read_from(&mut reader, ParseOptions::new()) {
        Ok(mp4) => match mp4.properties().codec() {
            Mp4Codec::ALAC => "alac".to_string(),
            Mp4Codec::AAC => "aac".to_string(),
            _ => "aac".to_string(),
        },
        Err(_) => "aac".to_string(),
    }
}

fn codec_is_lossless(codec: &str) -> bool {
    matches!(codec, "flac" | "alac" | "pcm")
}

/// MIME mapping per SPEC §7.3.
fn mime_for(ft: FileType) -> &'static str {
    match ft {
        FileType::Flac => "audio/flac",
        FileType::Mp4 => "audio/mp4",
        FileType::Mpeg => "audio/mpeg",
        FileType::Wav => "audio/x-wav",
        FileType::Aiff => "audio/x-aiff",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // SPEC §14: compilation flag (M4A `cpil` / MP3 `TCMP` / Vorbis `COMPILATION`)
    // treats "1" / "true" / "yes" as set. Regressions here cause compilation
    // albums to split instead of collapsing under Various Artists.
    #[test]
    fn pb1_parse_bool_flag_true_variants() {
        assert!(parse_bool_flag("1"));
        assert!(parse_bool_flag("true"));
        assert!(parse_bool_flag("TRUE"));
        assert!(parse_bool_flag("True"));
        assert!(parse_bool_flag("yes"));
        assert!(parse_bool_flag("YES"));
        assert!(parse_bool_flag(" 1 ")); // whitespace tolerated
    }

    #[test]
    fn pb2_parse_bool_flag_false_variants() {
        assert!(!parse_bool_flag(""));
        assert!(!parse_bool_flag("0"));
        assert!(!parse_bool_flag("false"));
        assert!(!parse_bool_flag("no"));
        assert!(!parse_bool_flag("2")); // numbers other than "1" are false
        assert!(!parse_bool_flag("garbage"));
    }

    #[test]
    fn pn1_parse_num_prefix_extracts_first_number() {
        assert_eq!(parse_num_prefix("3"), Some(3));
        assert_eq!(parse_num_prefix("3/12"), Some(3)); // "current/total" form
        assert_eq!(parse_num_prefix(" 7 "), Some(7));
        assert_eq!(parse_num_prefix("0"), Some(0));
    }

    #[test]
    fn pn2_parse_num_prefix_returns_none_on_garbage() {
        assert_eq!(parse_num_prefix(""), None);
        assert_eq!(parse_num_prefix("abc"), None);
        assert_eq!(parse_num_prefix("/12"), None); // empty prefix
        assert_eq!(parse_num_prefix("-1"), None); // out of u32 range
    }

    // SPEC §7.3 MIME mapping. Linn reads the codec from protocolInfo,
    // so getting this wrong breaks playback before it starts.
    #[test]
    fn mf1_mime_for_known_filetypes() {
        assert_eq!(mime_for(FileType::Flac), "audio/flac");
        assert_eq!(mime_for(FileType::Mp4), "audio/mp4");
        assert_eq!(mime_for(FileType::Mpeg), "audio/mpeg");
        assert_eq!(mime_for(FileType::Wav), "audio/x-wav");
        assert_eq!(mime_for(FileType::Aiff), "audio/x-aiff");
    }

    #[test]
    fn mf2_mime_for_unknown_falls_back_to_octet_stream() {
        // Unimplemented FileType variants (e.g. WavPack) must not panic; fall back to octet-stream
        assert_eq!(mime_for(FileType::WavPack), "application/octet-stream");
    }

    // SPEC §14 / §4.6: codec identification. For non-M4A use FileType (not
    // extension fall-through); the file type identified by lofty's Probe is
    // the source of truth.
    #[test]
    fn cf1_codec_for_known_filetypes() {
        let p = PathBuf::from("/dummy.flac");
        assert_eq!(codec_for(FileType::Flac, &p), "flac");
        assert_eq!(codec_for(FileType::Mpeg, &p), "mp3");
        assert_eq!(codec_for(FileType::Wav, &p), "pcm");
        assert_eq!(codec_for(FileType::Aiff, &p), "pcm");
    }

    #[test]
    fn cf2_codec_for_mp4_falls_back_to_aac_when_file_missing() {
        // detect_mp4_codec returns aac when file open fails (SPEC §14: treating
        // ALAC as AAC only drops bit_depth, playback still works — safe fallback)
        let missing = PathBuf::from("/no/such/path/missing.m4a");
        assert_eq!(codec_for(FileType::Mp4, &missing), "aac");
    }

    #[test]
    fn cf3_codec_for_unknown_returns_unknown() {
        let p = PathBuf::from("/dummy.xyz");
        assert_eq!(codec_for(FileType::WavPack, &p), "unknown");
    }

    // SPEC §4.6: codec_is_lossless is the entry point for tier classification
    // (`hires` / `lossless`). If ALAC isn't treated as lossless, M4A HD albums
    // collapse into `mixed` and the display breaks.
    #[test]
    fn cl1_codec_is_lossless_includes_all_three_lossless_codecs() {
        assert!(codec_is_lossless("flac"));
        assert!(codec_is_lossless("alac"));
        assert!(codec_is_lossless("pcm"));
    }

    #[test]
    fn cl2_codec_is_lossless_rejects_lossy_and_unknown() {
        assert!(!codec_is_lossless("mp3"));
        assert!(!codec_is_lossless("aac"));
        assert!(!codec_is_lossless("unknown"));
        assert!(!codec_is_lossless(""));
        assert!(!codec_is_lossless("FLAC")); // case-sensitive: internal form is lowercase
    }
}
