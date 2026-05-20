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
}

pub type Result<T> = std::result::Result<T, Error>;
