//! Catalog of user-editable config keys (issue #13).
//!
//! Each entry binds a stable string key to:
//! - how to read its default from a [`Config`] loaded from `config.toml`,
//! - a validator that normalizes incoming JSON values,
//! - a reload tier describing when a change actually takes effect.
//!
//! The catalog is intentionally hand-rolled: the set of editable keys is small,
//! and a static list is easier to reason about than reflection over the toml
//! schema.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::db::config_overrides;
use crate::error::Result;
use crate::state::BrowseSettings;

/// Snapshot of toml defaults captured at startup, keyed by catalog key.
/// Decouples runtime resolution from the [`Config`] struct so [`crate::state::AppState`]
/// does not need to hold the full Config.
pub type DefaultsMap = HashMap<String, Value>;

/// When a change to a key actually takes effect. Exposed in the GET response so
/// the admin UI can warn the user that some saves need a rescan / restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadTier {
    /// Effective on the next request.
    Runtime,
    /// Effective after a follow-up action (e.g. rescan).
    Reload,
    /// Saved, but only takes effect on next process start.
    Restart,
}

impl ReloadTier {
    pub fn as_str(self) -> &'static str {
        match self {
            ReloadTier::Runtime => "runtime",
            ReloadTier::Reload => "reload",
            ReloadTier::Restart => "restart",
        }
    }
}

/// One option in an ordered-set / enum-valued config key. Sent verbatim to the
/// admin UI so it can render real labels instead of `cat:*` internal IDs.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ChoiceMeta {
    pub id: &'static str,
    pub label: &'static str,
}

/// Canonical option list for `browse.top_level`. Order here is the UI's
/// fallback presentation order for *disabled* (currently-hidden) entries —
/// the saved value's array order drives the *enabled* order.
///
/// Labels match the user-facing facet titles emitted by
/// `categories::build_root_facet`, pluralised where the facet is a *category
/// of* something (e.g. "Album Artists" — a category containing many album
/// artist containers).
pub const TOP_LEVEL_CHOICES: &[ChoiceMeta] = &[
    ChoiceMeta {
        id: "cat:aa",
        label: "Album Artists",
    },
    ChoiceMeta {
        id: "cat:ar",
        label: "Artists",
    },
    ChoiceMeta {
        id: "cat:al",
        label: "Albums",
    },
    ChoiceMeta {
        id: "cat:gn",
        label: "Genres",
    },
    ChoiceMeta {
        id: "cat:recent",
        label: "Recently Added",
    },
    ChoiceMeta {
        id: "cat:played",
        label: "Recently Played",
    },
    ChoiceMeta {
        id: "cat:random",
        label: "Random Albums",
    },
    ChoiceMeta {
        id: "cat:hires",
        label: "Hi-Res Albums",
    },
    ChoiceMeta {
        id: "cat:lossy",
        label: "Lossy Albums",
    },
    ChoiceMeta {
        id: "cat:mixed",
        label: "Mixed Quality",
    },
    ChoiceMeta {
        id: "cat:cm",
        label: "Composers",
    },
    ChoiceMeta {
        id: "cat:cn",
        label: "Conductors",
    },
    ChoiceMeta {
        id: "cat:pf",
        label: "Performers",
    },
    ChoiceMeta {
        id: "cat:yr",
        label: "Year",
    },
    ChoiceMeta {
        id: "cat:dec",
        label: "Decade",
    },
];

pub struct ConfigKey {
    pub key: &'static str,
    /// Short human-readable title for the admin UI row.
    pub label: &'static str,
    /// One-sentence explanation shown below the input.
    pub description: &'static str,
    pub reload_tier: ReloadTier,
    /// Read the toml-default for this key out of [`Config`].
    pub default: fn(&Config) -> Value,
    /// Validate and normalize an incoming value. Returns the canonical form to
    /// persist, or a human-readable error.
    pub validate: fn(&Value) -> std::result::Result<Value, String>,
    /// Closed option list for ordered-set keys; lets the UI render the
    /// `browse.top_level` reorder-and-toggle control with real labels.
    /// `None` for free-form scalars.
    pub choices: Option<&'static [ChoiceMeta]>,
}

pub const CATALOG: &[ConfigKey] = &[
    ConfigKey {
        key: "browse.top_level",
        label: "Top-level categories",
        description: "Which categories appear at the root, and in what order. \
                      Year / Decade / Composers / Conductors / Performers only \
                      show up when the library has matching data.",
        reload_tier: ReloadTier::Runtime,
        default: default_top_level,
        validate: validate_string_array,
        choices: Some(TOP_LEVEL_CHOICES),
    },
    ConfigKey {
        key: "browse.recently_added_limit",
        label: "Recently Added — max items",
        description: "Maximum number of albums shown in the Recently Added view. \
                      Leave empty for no cap.",
        reload_tier: ReloadTier::Runtime,
        default: default_recently_added_limit,
        validate: validate_nullable_positive_int,
        choices: None,
    },
    ConfigKey {
        key: "browse.recently_added_max_age_days",
        label: "Recently Added — max age (days)",
        description: "Hide albums first seen more than this many days ago. \
                      Leave empty for no age limit.",
        reload_tier: ReloadTier::Runtime,
        default: default_recently_added_max_age_days,
        validate: validate_nullable_positive_int,
        choices: None,
    },
    ConfigKey {
        key: "browse.random_albums_limit",
        label: "Random Albums — list size",
        description: "Number of albums included in the shuffled Random Albums list. \
                      Leave empty for no cap.",
        reload_tier: ReloadTier::Runtime,
        default: default_random_albums_limit,
        validate: validate_nullable_positive_int,
        choices: None,
    },
    ConfigKey {
        key: "browse.random_albums_shuffle_interval_hours",
        label: "Random Albums — auto re-roll (hours)",
        description: "Re-roll the Random Albums order lazily when the next \
                      Browse arrives this many hours after the previous \
                      reshuffle. Idle hours cost nothing. Leave empty to keep \
                      the order fixed between startup / scan / manual reshuffle.",
        reload_tier: ReloadTier::Runtime,
        default: default_random_albums_shuffle_interval_hours,
        validate: validate_nullable_positive_int,
        choices: None,
    },
    ConfigKey {
        key: "search.fuzzy_enabled",
        label: "Fuzzy Search (typo tolerance)",
        description: "Tolerate 1–2 character typos in Album / Artist / Track / \
                      Composer searches via FTS5 trigram. Exact and contains hits \
                      still rank above typo hits; turn off if a library produces \
                      too many false positives.",
        reload_tier: ReloadTier::Runtime,
        default: default_search_fuzzy_enabled,
        validate: validate_bool,
        choices: None,
    },
];

pub fn find(key: &str) -> Option<&'static ConfigKey> {
    CATALOG.iter().find(|c| c.key == key)
}

fn default_recently_added_limit(c: &Config) -> Value {
    match c.browse.recently_added_limit {
        Some(n) => serde_json::json!(n),
        None => Value::Null,
    }
}

fn default_recently_added_max_age_days(c: &Config) -> Value {
    match c.browse.recently_added_max_age_days {
        Some(n) => serde_json::json!(n),
        None => Value::Null,
    }
}

fn default_random_albums_limit(c: &Config) -> Value {
    match c.browse.random_albums_limit {
        Some(n) => serde_json::json!(n),
        None => Value::Null,
    }
}

fn default_random_albums_shuffle_interval_hours(c: &Config) -> Value {
    match c.browse.random_albums_shuffle_interval_hours {
        Some(n) => serde_json::json!(n),
        None => Value::Null,
    }
}

fn default_top_level(c: &Config) -> Value {
    serde_json::json!(c.browse.top_level)
}

fn default_search_fuzzy_enabled(c: &Config) -> Value {
    Value::Bool(c.search.fuzzy_enabled)
}

fn validate_bool(v: &Value) -> std::result::Result<Value, String> {
    match v.as_bool() {
        Some(b) => Ok(Value::Bool(b)),
        None => Err("must be a boolean".to_string()),
    }
}

fn validate_positive_int(v: &Value) -> std::result::Result<Value, String> {
    match v.as_u64() {
        Some(n) if n >= 1 => Ok(Value::from(n)),
        Some(_) => Err("must be >= 1".to_string()),
        None => Err("must be a positive integer".to_string()),
    }
}

fn validate_nullable_positive_int(v: &Value) -> std::result::Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    validate_positive_int(v)
}

/// Validator for `browse.top_level` (#8): array of strings, deduped while
/// preserving first occurrence. Unknown facet IDs are accepted here and
/// silently dropped at render time (so forward-compatible additions to
/// the facet catalog do not require a config rewrite).
fn validate_string_array(v: &Value) -> std::result::Result<Value, String> {
    let arr = v.as_array().ok_or_else(|| "must be an array".to_string())?;
    // Cap to keep abuse / accidental megabytes out of the override row.
    if arr.len() > 64 {
        return Err("at most 64 entries".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let s = item
            .as_str()
            .ok_or_else(|| format!("entry {i} must be a string"))?;
        if seen.insert(s.to_string()) {
            out.push(Value::from(s));
        }
    }
    Ok(Value::Array(out))
}

/// Capture toml defaults for every catalog key. Called once at startup; the
/// result is stored on `AppState` so handlers can report "default" alongside
/// effective values without holding the whole [`Config`].
pub fn precompute_defaults(cfg: &Config) -> DefaultsMap {
    CATALOG
        .iter()
        .map(|c| (c.key.to_string(), (c.default)(cfg)))
        .collect()
}

/// Resolve a single key's effective value by layering the override (if any)
/// over the captured toml default.
pub fn effective_value(defaults: &DefaultsMap, conn: &Connection, key: &str) -> Result<Value> {
    if let Some(raw) = config_overrides::get(conn, key)? {
        if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
            return Ok(parsed);
        }
    }
    Ok(defaults.get(key).cloned().unwrap_or(Value::Null))
}

/// Build a [`BrowseSettings`] reflecting toml defaults + saved overrides.
/// Called at startup and after a successful POST/DELETE so the in-memory
/// snapshot stays in sync with the DB.
pub fn build_browse_settings(defaults: &DefaultsMap, conn: &Connection) -> Result<BrowseSettings> {
    let recently = {
        let v = effective_value(defaults, conn, "browse.recently_added_limit")?;
        if v.is_null() {
            None
        } else {
            v.as_u64().map(|n| n as usize)
        }
    };
    let max_age_days = {
        let v = effective_value(defaults, conn, "browse.recently_added_max_age_days")?;
        if v.is_null() {
            None
        } else {
            v.as_u64().map(|n| n.min(u32::MAX as u64) as u32)
        }
    };
    let random = {
        let v = effective_value(defaults, conn, "browse.random_albums_limit")?;
        if v.is_null() {
            None
        } else {
            v.as_u64().map(|n| n as usize)
        }
    };
    let random_interval_hours = {
        let v = effective_value(
            defaults,
            conn,
            "browse.random_albums_shuffle_interval_hours",
        )?;
        if v.is_null() {
            None
        } else {
            v.as_u64().map(|n| n.min(u32::MAX as u64) as u32)
        }
    };
    let top_level = effective_value(defaults, conn, "browse.top_level")?
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(crate::config::default_top_level);
    // #28: missing key (older config_overrides table, or a fresh DB before the
    // first POST) defaults to true. `as_bool()` returns None for non-bool JSON,
    // which we also treat as the safe default rather than failing the build.
    let search_fuzzy_enabled = effective_value(defaults, conn, "search.fuzzy_enabled")?
        .as_bool()
        .unwrap_or(true);
    Ok(BrowseSettings::from_parts(
        recently,
        max_age_days,
        random,
        random_interval_hours,
        top_level,
        search_fuzzy_enabled,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    fn sample_config() -> Config {
        toml::from_str(
            r#"
[server]
friendly_name = "X"
http_port = 8200

[library]
root = "/x"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 1

[browse]
recently_added_limit = 50
random_albums_limit  = 100
"#,
        )
        .unwrap()
    }

    fn open_in_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn cc1_find_known_key() {
        assert!(find("browse.recently_added_limit").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn cc2_validate_positive_int_accepts_positive() {
        let v = validate_positive_int(&serde_json::json!(42)).unwrap();
        assert_eq!(v, serde_json::json!(42));
    }

    #[test]
    fn cc3_validate_positive_int_rejects_zero_and_negative() {
        assert!(validate_positive_int(&serde_json::json!(0)).is_err());
        assert!(validate_positive_int(&serde_json::json!(-1)).is_err());
        assert!(validate_positive_int(&serde_json::json!("x")).is_err());
    }

    #[test]
    fn cc5_effective_value_falls_back_to_default_when_no_override() {
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        let v = effective_value(&defaults, &conn, "browse.recently_added_limit").unwrap();
        assert_eq!(v, serde_json::json!(50));
    }

    #[test]
    fn cc6_effective_value_uses_override_when_present() {
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        config_overrides::set(&conn, "browse.recently_added_limit", "75", 0).unwrap();
        let v = effective_value(&defaults, &conn, "browse.recently_added_limit").unwrap();
        assert_eq!(v, serde_json::json!(75));
    }

    #[test]
    fn cc7_build_browse_settings_applies_overrides() {
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        config_overrides::set(&conn, "browse.recently_added_limit", "200", 0).unwrap();
        let s = build_browse_settings(&defaults, &conn).unwrap();
        assert_eq!(s.recently_added_limit, Some(200));
        assert_eq!(s.random_albums_limit, Some(100)); // toml default
    }

    #[test]
    fn cc8_build_browse_settings_uses_toml_defaults_when_no_overrides() {
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        let s = build_browse_settings(&defaults, &conn).unwrap();
        assert_eq!(s.recently_added_limit, Some(50));
        assert_eq!(s.random_albums_limit, Some(100));
        assert_eq!(s.top_level, crate::config::default_top_level());
    }

    #[test]
    fn cc13_build_browse_settings_treats_missing_limit_as_none() {
        // sample_config sets explicit limits; verify the unset path by stripping
        // them out. With both keys absent from toml the build snapshot should
        // surface None (= no cap), matching the new out-of-the-box default.
        let cfg: Config = toml::from_str(
            r#"
[server]
friendly_name = "X"
http_port = 8200

[library]
root = "/x"
extensions = ["flac"]

[scan]
on_startup = false
parallel = 1

[browse]
"#,
        )
        .unwrap();
        let conn = open_in_memory();
        let defaults = precompute_defaults(&cfg);
        let s = build_browse_settings(&defaults, &conn).unwrap();
        assert_eq!(s.recently_added_limit, None);
        assert_eq!(s.random_albums_limit, None);
    }

    #[test]
    fn cc14_admin_can_clear_a_set_limit_with_null() {
        // The admin UI sends `null` to mean "no cap"; that should round-trip
        // through the catalog validator and land as None in the snapshot, even
        // when the toml default is a positive integer.
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        config_overrides::set(&conn, "browse.recently_added_limit", "null", 0).unwrap();
        let s = build_browse_settings(&defaults, &conn).unwrap();
        assert_eq!(s.recently_added_limit, None);
    }

    #[test]
    fn cc9_validate_string_array_accepts_and_dedupes() {
        let v = validate_string_array(&serde_json::json!(["cat:aa", "cat:al", "cat:aa"])).unwrap();
        assert_eq!(v, serde_json::json!(["cat:aa", "cat:al"]));
    }

    #[test]
    fn cc10_validate_string_array_rejects_non_array_and_non_string() {
        assert!(validate_string_array(&serde_json::json!("nope")).is_err());
        assert!(validate_string_array(&serde_json::json!(["cat:aa", 1])).is_err());
    }

    #[test]
    fn cc11_validate_string_array_caps_length() {
        let many: Vec<String> = (0..65).map(|i| format!("cat:{i}")).collect();
        assert!(validate_string_array(&serde_json::json!(many)).is_err());
    }

    #[test]
    fn cc12_top_level_override_picked_up() {
        let conn = open_in_memory();
        let defaults = precompute_defaults(&sample_config());
        config_overrides::set(&conn, "browse.top_level", r#"["cat:aa","cat:played"]"#, 0).unwrap();
        let s = build_browse_settings(&defaults, &conn).unwrap();
        assert_eq!(s.top_level, vec!["cat:aa", "cat:played"]);
    }
}
