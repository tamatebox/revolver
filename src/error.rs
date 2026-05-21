use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config file not found or unreadable: {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config parse error in {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json serialize/deserialize: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("db pool: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("internal lock poisoned: {what}")]
    LockPoisoned { what: &'static str },

    #[error("semaphore closed: {what}")]
    SemaphoreClosed { what: &'static str },

    #[error("not found: {kind} {key}")]
    NotFound { kind: &'static str, key: String },
}

impl Error {
    /// Map a `rusqlite::Error` to either `NotFound` (only on `QueryReturnedNoRows`)
    /// or the generic `Sqlite` variant. Use at single-row lookup sites where a
    /// "no such ID" miss should propagate as `NotFound` so the SOAP handler can
    /// emit `701 NoSuchObject` instead of `500 InternalError`.
    pub fn sqlite_or_not_found(e: rusqlite::Error, kind: &'static str, key: impl ToString) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => Self::NotFound {
                kind,
                key: key.to_string(),
            },
            other => Self::Sqlite(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
