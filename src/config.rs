use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// Top-level configuration, matching `config.toml.example` (SPEC §12).
#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: Server,
    pub library: Library,
    pub scan: Scan,
    pub browse: Browse,
}

#[derive(Debug, Deserialize)]
pub struct Server {
    pub friendly_name: String,
    pub http_port: u16,
    #[serde(default = "default_uuid")]
    pub uuid: String,
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// HTTP server bind address (security §5).
    /// Default `"0.0.0.0"` assumes the server is visible across the LAN via SSDP.
    /// Set to `"127.0.0.1"` etc. to restrict LAN exposure. **Direct exposure to
    /// the public Internet is out of scope** (see README).
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
}

fn default_uuid() -> String {
    "auto".to_string()
}

fn default_db_path() -> PathBuf {
    PathBuf::from("revolver.db")
}

fn default_bind_address() -> String {
    "0.0.0.0".to_string()
}

#[derive(Debug, Deserialize)]
pub struct Library {
    pub root: PathBuf,
    pub extensions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Scan {
    pub on_startup: bool,
    pub parallel: usize,
}

#[derive(Debug, Deserialize)]
pub struct Browse {
    pub recently_added_limit: usize,
    /// Cap albums shown under `cat:recent` by age in days. `None` = no age
    /// cap (show everything by recency). SPEC §6.7.
    #[serde(default)]
    pub recently_added_max_age_days: Option<u32>,
    pub random_albums_limit: usize,

    /// Selection and order of top-level facets surfaced at ObjectID "0"
    /// (SPEC §6.2, issue #8). Unknown / disabled entries are silently
    /// dropped at render time. Defaults to the full canonical list — same
    /// behavior as pre-#8 for users who do not set this key.
    #[serde(default = "default_top_level")]
    pub top_level: Vec<String>,

    #[serde(default)]
    pub quality_in_title: bool,
    #[serde(default = "default_quality_in_title_format")]
    pub quality_in_title_format: String,
    #[serde(default = "default_quality_in_title_include")]
    pub quality_in_title_include: Vec<String>,
    #[serde(default)]
    pub quality_in_title_show_specs: bool,
}

fn default_quality_in_title_format() -> String {
    "[{q}]".to_string()
}

/// Default top-level facet order (SPEC §6.2). Kept in sync with the
/// hard-coded order in `browse::categories::root_children` prior to #8.
pub fn default_top_level() -> Vec<String> {
    [
        "cat:aa",
        "cat:ar",
        "cat:al",
        "cat:gn",
        "cat:recent",
        "cat:played",
        "cat:random",
        "cat:hires",
        "cat:lossy",
        "cat:mixed",
        "cat:cm",
        "cat:cn",
        "cat:pf",
        "cat:yr",
        "cat:dec",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_quality_in_title_include() -> Vec<String> {
    vec![
        "hires".to_string(),
        "lossy".to_string(),
        "mixed".to_string(),
    ]
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c1_parse_example() {
        let text = include_str!("../config.toml.example");
        let cfg: Config = toml::from_str(text).expect("example must parse");

        assert_eq!(cfg.server.friendly_name, "Revolver");
        assert_eq!(cfg.server.http_port, 8200);
        assert_eq!(cfg.server.uuid, "auto");
        assert_eq!(cfg.server.db_path, PathBuf::from("revolver.db"));

        assert_eq!(cfg.library.root, PathBuf::from("/path/to/music"));
        assert_eq!(cfg.library.extensions.len(), 6);

        assert!(cfg.scan.on_startup);
        assert_eq!(cfg.scan.parallel, 8);

        assert_eq!(cfg.browse.recently_added_limit, 50);
        assert_eq!(cfg.browse.random_albums_limit, 100);
        assert!(!cfg.browse.quality_in_title);
    }

    #[test]
    fn c3_missing_required_field_errors() {
        // Omit friendly_name in [server] -> parse error (no default).
        let text = r#"
[server]
http_port = 8200

[library]
root = "/x"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 1

[browse]
recently_added_limit = 10
random_albums_limit = 10
"#;
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn c4_wrong_type_errors() {
        // http_port as string -> type mismatch.
        let text = r#"
[server]
friendly_name = "X"
http_port = "not-a-number"

[library]
root = "/x"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 1

[browse]
recently_added_limit = 10
random_albums_limit = 10
"#;
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn c5_port_zero_and_over_65535_handled() {
        // Port 0 is "invalid" as a port but valid u16 in TOML, so parsing succeeds
        // (the OS rejects it at actual bind time; this only checks the input schema).
        let text = r#"
[server]
friendly_name = "X"
http_port = 0

[library]
root = "/x"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 1

[browse]
recently_added_limit = 10
random_albums_limit = 10
"#;
        let cfg: Config = toml::from_str(text).expect("port 0 should parse");
        assert_eq!(cfg.server.http_port, 0);

        // 65536 is out of u16 range -> parse error.
        let too_big = text.replace("http_port = 0", "http_port = 65536");
        assert!(toml::from_str::<Config>(&too_big).is_err());

        // Negative values are also out of u16 range.
        let negative = text.replace("http_port = 0", "http_port = -1");
        assert!(toml::from_str::<Config>(&negative).is_err());
    }

    #[test]
    fn c6_load_returns_clear_error_for_missing_file() {
        // Error shape when the file itself does not exist.
        let err = Config::load(std::path::Path::new("/no/such/path/xyz.toml")).unwrap_err();
        // Expect a ConfigRead variant.
        let s = format!("{}", err);
        assert!(s.contains("config file not found"), "got: {}", s);
    }

    #[test]
    fn c2_defaults_applied() {
        let text = r#"
[server]
friendly_name = "Test"
http_port = 9000

[library]
root = "/music"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 4

[browse]
recently_added_limit = 10
random_albums_limit = 20
"#;
        let cfg: Config = toml::from_str(text).expect("minimal config must parse");

        assert_eq!(cfg.server.uuid, "auto");
        assert_eq!(cfg.server.db_path, PathBuf::from("revolver.db"));
        assert_eq!(cfg.server.bind_address, "0.0.0.0");
        assert!(!cfg.browse.quality_in_title);
        assert_eq!(cfg.browse.quality_in_title_format, "[{q}]");
        assert_eq!(
            cfg.browse.quality_in_title_include,
            vec!["hires", "lossy", "mixed"]
        );
        assert!(!cfg.browse.quality_in_title_show_specs);
    }
}
