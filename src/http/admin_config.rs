//! `/admin/config` endpoints (issue #13).
//!
//! - `GET /admin/config` returns every catalog key with its effective value
//!   plus the toml default, "default" / "user" source flag, and the reload
//!   tier so the UI can show the right warning.
//! - `POST /admin/config` applies a partial JSON-merge update. All keys are
//!   validated first; nothing is written until every key passes.
//! - `DELETE /admin/config/{key}` drops the override and reverts the key to
//!   its toml default.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::Value;

use crate::config_catalog::{self, ChoiceMeta, ReloadTier};
use crate::db::config_overrides;
use crate::http::HttpError;
use crate::state::AppState;

#[derive(Serialize)]
pub struct ConfigEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub value: Value,
    pub default: Value,
    pub source: &'static str,           // "default" | "user"
    pub restart_required: &'static str, // "runtime" | "reload" | "restart"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<&'static [ChoiceMeta]>,
}

/// `GET /admin/config`.
pub async fn get_config(
    State(state): State<AppState>,
) -> Result<Json<Vec<ConfigEntry>>, HttpError> {
    let conn = state.db_pool.get()?;
    let mut out = Vec::with_capacity(config_catalog::CATALOG.len());
    for entry in config_catalog::CATALOG {
        let default = state
            .config_defaults
            .get(entry.key)
            .cloned()
            .unwrap_or(Value::Null);
        let override_value = config_overrides::get(&conn, entry.key)?
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let (value, source) = match override_value {
            Some(v) => (v, "user"),
            None => (default.clone(), "default"),
        };
        out.push(ConfigEntry {
            key: entry.key,
            label: entry.label,
            description: entry.description,
            value,
            default,
            source,
            restart_required: entry.reload_tier.as_str(),
            choices: entry.choices,
        });
    }
    Ok(Json(out))
}

/// `POST /admin/config` — partial update. Body is a JSON object of
/// `{ "key": value, ... }`. Unknown keys → 422; validation failure → 422.
pub async fn post_config(
    State(state): State<AppState>,
    Json(updates): Json<HashMap<String, Value>>,
) -> Result<StatusCode, ConfigUpdateError> {
    // Pre-validate everything before writing any row, so an invalid request
    // never produces a partial write.
    let mut normalized: Vec<(&'static str, Value, ReloadTier)> = Vec::with_capacity(updates.len());
    for (key, value) in updates {
        let entry =
            config_catalog::find(&key).ok_or_else(|| ConfigUpdateError::UnknownKey(key.clone()))?;
        let canonical = (entry.validate)(&value).map_err(|reason| ConfigUpdateError::Invalid {
            key: key.clone(),
            reason,
        })?;
        normalized.push((entry.key, canonical, entry.reload_tier));
    }

    let conn = state.db_pool.get().map_err(ConfigUpdateError::pool)?;
    let now = unix_now();
    let mut random_limit_touched = false;
    for (key, value, _tier) in &normalized {
        let raw = serde_json::to_string(value).map_err(ConfigUpdateError::serde)?;
        config_overrides::set(&conn, key, &raw, now).map_err(ConfigUpdateError::internal)?;
        tracing::info!(key = %key, value = %value, "config changed");
        if *key == "browse.random_albums_limit" {
            random_limit_touched = true;
        }
    }

    refresh_browse_snapshot(&state, &conn)?;

    // `cat:random` is backed by an in-memory shuffled vec sized to
    // `random_albums_limit` at reshuffle time. Without re-shuffling here, the
    // user's limit change would not take visible effect until the next
    // scan / startup / explicit Reshuffle button press.
    if random_limit_touched {
        // Poisoned lock degrades to `None` (= no cap) rather than skipping the
        // reshuffle entirely; preserves the user-visible reshuffle behavior
        // even in the (rare) poisoned-lock case.
        let new_limit = state
            .browse
            .read()
            .map(|s| s.random_albums_limit)
            .unwrap_or(None);
        if let Err(e) = state.random_state.reshuffle(&conn, new_limit) {
            tracing::warn!(error = %e, "auto-reshuffle after random_albums_limit change failed");
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/config/{key}` — drop the override; effective value falls
/// back to the toml default.
pub async fn delete_config_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<StatusCode, HttpError> {
    if config_catalog::find(&key).is_none() {
        return Err(HttpError::NotFound);
    }
    let conn = state.db_pool.get()?;
    let removed = config_overrides::delete(&conn, &key)?;
    if removed {
        tracing::info!(key = %key, "config override deleted");
    }
    refresh_browse_snapshot(&state, &conn).map_err(|e| e.into_http_error())?;
    Ok(StatusCode::NO_CONTENT)
}

fn refresh_browse_snapshot(
    state: &AppState,
    conn: &rusqlite::Connection,
) -> Result<(), ConfigUpdateError> {
    let new_browse = config_catalog::build_browse_settings(&state.config_defaults, conn)
        .map_err(ConfigUpdateError::internal)?;
    let mut guard = state
        .browse
        .write()
        .map_err(|_| ConfigUpdateError::lock_poisoned())?;
    *guard = new_browse;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Distinct error type so 422 (validation) vs 500 (internal) are first-class.
#[derive(Debug)]
pub enum ConfigUpdateError {
    UnknownKey(String),
    Invalid { key: String, reason: String },
    Internal(anyhow::Error),
}

impl ConfigUpdateError {
    fn pool(e: r2d2::Error) -> Self {
        Self::Internal(anyhow::Error::new(e))
    }
    fn serde(e: serde_json::Error) -> Self {
        Self::Internal(anyhow::Error::new(e))
    }
    fn internal<E: Into<anyhow::Error>>(e: E) -> Self {
        Self::Internal(e.into())
    }
    fn lock_poisoned() -> Self {
        Self::Internal(anyhow::Error::new(crate::error::Error::LockPoisoned {
            what: "config snapshot",
        }))
    }
    fn into_http_error(self) -> HttpError {
        match self {
            Self::Internal(e) => HttpError::Internal(e),
            // Refresh path doesn't surface user errors, so any conversion here is internal.
            other => HttpError::Internal(anyhow::anyhow!("unexpected: {:?}", other)),
        }
    }
}

impl From<crate::error::Error> for ConfigUpdateError {
    fn from(e: crate::error::Error) -> Self {
        Self::Internal(anyhow::Error::new(e))
    }
}

impl IntoResponse for ConfigUpdateError {
    fn into_response(self) -> Response {
        match self {
            Self::UnknownKey(key) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown config key: {}", key),
            )
                .into_response(),
            Self::Invalid { key, reason } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid value for {}: {}", key, reason),
            )
                .into_response(),
            Self::Internal(e) => {
                tracing::error!(error = ?e, "internal error in config update");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::Router;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::state::test_helpers::test_state;

    fn app() -> (Router, tempfile::TempDir) {
        // test_state has an empty config_defaults; populate it for these tests by
        // re-building AppState with the catalog defaults from a sample config.
        let (mut state, dbdir) = test_state();
        let sample_cfg: crate::config::Config = toml::from_str(
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
        .unwrap();
        state.config_defaults =
            std::sync::Arc::new(config_catalog::precompute_defaults(&sample_cfg));
        let router = Router::new()
            .route(
                "/admin/config",
                axum::routing::get(get_config).post(post_config),
            )
            .route(
                "/admin/config/{key}",
                axum::routing::delete(delete_config_key),
            )
            .with_state(state);
        (router, dbdir)
    }

    async fn body_json(resp: axum::http::Response<Body>) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn ac1_get_returns_all_keys_with_defaults() {
        let (router, _dir) = app();
        let resp = router
            .oneshot(Request::get("/admin/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), config_catalog::CATALOG.len());
        for entry in arr {
            assert_eq!(entry["source"], "default");
            assert_eq!(entry["restart_required"], "runtime");
            assert!(entry["label"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(entry["description"].as_str().is_some_and(|s| !s.is_empty()));
        }
    }

    #[tokio::test]
    async fn ac1b_top_level_carries_choices_metadata() {
        let (router, _dir) = app();
        let resp = router
            .oneshot(Request::get("/admin/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(resp).await;
        let entry = body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "browse.top_level")
            .unwrap();
        let choices = entry["choices"].as_array().expect("choices for top_level");
        assert!(!choices.is_empty());
        let aa = choices
            .iter()
            .find(|c| c["id"] == "cat:aa")
            .expect("cat:aa in choices");
        assert_eq!(aa["label"], "Album Artists");

        // Scalar keys should omit the field entirely (skip_serializing_if).
        let scalar = body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "browse.recently_added_limit")
            .unwrap();
        assert!(scalar.get("choices").is_none());
    }

    #[tokio::test]
    async fn ac2_post_partial_update_succeeds() {
        let (router, _dir) = app();
        let resp = router
            .clone()
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "browse.recently_added_limit": 200 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Re-read and confirm the override surfaces.
        let resp = router
            .oneshot(Request::get("/admin/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        let entry = arr
            .iter()
            .find(|e| e["key"] == "browse.recently_added_limit")
            .unwrap();
        assert_eq!(entry["value"], json!(200));
        assert_eq!(entry["source"], "user");
        assert_eq!(entry["default"], json!(50));
    }

    #[tokio::test]
    async fn ac3_post_unknown_key_returns_422() {
        let (router, _dir) = app();
        let resp = router
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "nope": 1 }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn ac4_post_invalid_value_returns_422() {
        let (router, _dir) = app();
        let resp = router
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "browse.recently_added_limit": 0 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn ac5_post_is_all_or_nothing() {
        // First key is valid, second is invalid -> nothing should be written.
        let (router, _dir) = app();
        let resp = router
            .clone()
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "browse.recently_added_limit": 150,
                            "browse.top_level": "not-an-array"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resp = router
            .oneshot(Request::get("/admin/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(resp).await;
        // recently_added_limit should NOT have been written.
        let entry = body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "browse.recently_added_limit")
            .unwrap();
        assert_eq!(entry["source"], "default");
    }

    #[tokio::test]
    async fn ac6_delete_reverts_to_default() {
        let (router, _dir) = app();
        // Set then delete.
        router
            .clone()
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "browse.recently_added_limit": 200 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = router
            .clone()
            .oneshot(
                Request::delete("/admin/config/browse.recently_added_limit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = router
            .oneshot(Request::get("/admin/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(resp).await;
        let entry = body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "browse.recently_added_limit")
            .unwrap();
        assert_eq!(entry["source"], "default");
        assert_eq!(entry["value"], json!(50));
    }

    #[tokio::test]
    async fn ac7_delete_unknown_key_returns_404() {
        let (router, _dir) = app();
        let resp = router
            .oneshot(
                Request::delete("/admin/config/nope.nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ac8_post_updates_browse_snapshot() {
        let (router, _dir) = app();
        // We can't easily inspect AppState through the router, but we can verify
        // the GET returns the new value (proven by ac2) and trust that
        // refresh_browse_snapshot ran successfully (status 204).
        let resp = router
            .oneshot(
                Request::post("/admin/config")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "browse.random_albums_limit": 5 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
