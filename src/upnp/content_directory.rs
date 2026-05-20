//! ContentDirectory:1 SOAP action dispatch (SPEC §5.3, §5.4).

use std::collections::HashMap;

use rusqlite::Connection;

use crate::browse::{self, search as browse_search, BrowseContext};
use crate::db::{state_kv, Pool};
use crate::state::AppState;
use crate::upnp::{didl, object_id, search, soap};

const CD_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
/// Upper bound when `RequestedCount = 0` (all results) (SPEC §6.5).
const MAX_REQUESTED_COUNT: usize = 1000;

pub fn handle(
    pool: &Pool,
    state: &AppState,
    request: &soap::SoapRequest,
) -> Result<String, soap::SoapFault> {
    match request.action.as_str() {
        "Browse" => handle_browse(pool, state, &request.args),
        "Search" => handle_search(pool, state, &request.args),
        "GetSearchCapabilities" => Ok(soap::build_response_body(
            "GetSearchCapabilities",
            CD_SERVICE_TYPE,
            &[("SearchCaps", "dc:title,upnp:artist,upnp:album")],
        )),
        "GetSortCapabilities" => Ok(soap::build_response_body(
            "GetSortCapabilities",
            CD_SERVICE_TYPE,
            &[("SortCaps", "dc:title,dc:date,upnp:originalTrackNumber")],
        )),
        "GetSystemUpdateID" => handle_system_update_id(pool),
        _ => Err(soap::SoapFault::invalid_action()),
    }
}

fn handle_browse(
    pool: &Pool,
    state: &AppState,
    args: &HashMap<String, String>,
) -> Result<String, soap::SoapFault> {
    let object_id_str = args
        .get("ObjectID")
        .ok_or_else(soap::SoapFault::invalid_args)?;
    let browse_flag = args
        .get("BrowseFlag")
        .ok_or_else(soap::SoapFault::invalid_args)?;
    let starting_index: usize = args
        .get("StartingIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let requested_count_raw: usize = args
        .get("RequestedCount")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let count = if requested_count_raw == 0 {
        MAX_REQUESTED_COUNT
    } else {
        requested_count_raw.min(MAX_REQUESTED_COUNT)
    };

    let object_id = object_id::parse(object_id_str).ok_or_else(soap::SoapFault::no_such_object)?;

    let conn = pool.get().map_err(|_| soap::SoapFault::internal_error())?;
    let art_base = format!("http://{}:{}/art", state.local_ip, state.http_port);
    let stream_base = format!("http://{}:{}/stream", state.local_ip, state.http_port);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let browse_settings = state
        .browse
        .read()
        .map_err(|_| soap::SoapFault::internal_error())?
        .clone();
    let ctx = BrowseContext {
        conn: &conn,
        art_base_url: &art_base,
        stream_base_url: &stream_base,
        random_state: &state.random_state,
        now_secs,
        settings: &browse_settings,
    };
    let update_id = read_update_id(&conn).unwrap_or(0);

    let (output, num_returned, total_matches) = match browse_flag.as_str() {
        "BrowseMetadata" => {
            let output = browse::browse_metadata(&ctx, &object_id)
                .map_err(|_| soap::SoapFault::no_such_object())?;
            let n = output.containers.len() + output.items.len();
            (output, n, n)
        }
        "BrowseDirectChildren" => {
            let result = browse::browse_children(&ctx, &object_id, starting_index, count)
                .map_err(|_| soap::SoapFault::no_such_object())?;
            let n = result.didl.containers.len() + result.didl.items.len();
            (result.didl, n, result.total_matches)
        }
        _ => return Err(soap::SoapFault::invalid_args()),
    };

    let didl_xml = didl::build_didl(&output.containers, &output.items);
    Ok(soap::build_response_body(
        "Browse",
        CD_SERVICE_TYPE,
        &[
            ("Result", &didl_xml),
            ("NumberReturned", &num_returned.to_string()),
            ("TotalMatches", &total_matches.to_string()),
            ("UpdateID", &update_id.to_string()),
        ],
    ))
}

/// Search action implementation (SPEC §5.4).
/// Supports only `contains` (NOCASE) against `dc:title` / `upnp:artist` / `upnp:album`.
/// Any other SearchCriteria returns an empty result (prefer no-op over misbehavior).
fn handle_search(
    pool: &Pool,
    state: &AppState,
    args: &HashMap<String, String>,
) -> Result<String, soap::SoapFault> {
    let criteria = args
        .get("SearchCriteria")
        .ok_or_else(soap::SoapFault::invalid_args)?;
    let starting_index: usize = args
        .get("StartingIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let requested_count_raw: usize = args
        .get("RequestedCount")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let count = if requested_count_raw == 0 {
        MAX_REQUESTED_COUNT
    } else {
        requested_count_raw.min(MAX_REQUESTED_COUNT)
    };

    let expr = search::parse_criteria(criteria);
    tracing::info!(
        target: "revolver::search",
        criteria = %criteria,
        starting_index = starting_index,
        requested_count = requested_count_raw,
        parsed = ?expr,
        "ContentDirectory Search received"
    );

    let conn = pool.get().map_err(|_| soap::SoapFault::internal_error())?;
    let art_base = format!("http://{}:{}/art", state.local_ip, state.http_port);
    let stream_base = format!("http://{}:{}/stream", state.local_ip, state.http_port);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let browse_settings = state
        .browse
        .read()
        .map_err(|_| soap::SoapFault::internal_error())?
        .clone();
    let ctx = BrowseContext {
        conn: &conn,
        art_base_url: &art_base,
        stream_base_url: &stream_base,
        random_state: &state.random_state,
        now_secs,
        settings: &browse_settings,
    };

    let result = browse_search::search_tracks(&ctx, &expr, starting_index, count)
        .map_err(|_| soap::SoapFault::internal_error())?;
    let update_id = read_update_id(&conn).unwrap_or(0);

    let didl_xml = didl::build_didl(&result.didl.containers, &result.didl.items);
    let num_returned = result.didl.items.len();
    Ok(soap::build_response_body(
        "Search",
        CD_SERVICE_TYPE,
        &[
            ("Result", &didl_xml),
            ("NumberReturned", &num_returned.to_string()),
            ("TotalMatches", &result.total_matches.to_string()),
            ("UpdateID", &update_id.to_string()),
        ],
    ))
}

fn handle_system_update_id(pool: &Pool) -> Result<String, soap::SoapFault> {
    let conn = pool.get().map_err(|_| soap::SoapFault::internal_error())?;
    let id = read_update_id(&conn).unwrap_or(0);
    Ok(soap::build_response_body(
        "GetSystemUpdateID",
        CD_SERVICE_TYPE,
        &[("Id", &id.to_string())],
    ))
}

fn read_update_id(conn: &Connection) -> Option<u32> {
    state_kv::get(conn, "system_update_id")
        .ok()?
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn pool_with_update_id(id: &str) -> Pool {
        let tmp = tempfile::TempDir::new().unwrap();
        let pool = db::pool(&tmp.path().join("test.db")).unwrap();
        // We want to return the pool but also keep tmp alive, so leak it for the
        // duration of the test (cleaned up when the test process exits).
        std::mem::forget(tmp);
        let conn = pool.get().unwrap();
        state_kv::set(&conn, "system_update_id", id).unwrap();
        drop(conn);
        pool
    }

    #[test]
    fn cd1_get_system_update_id_returns_value_from_state_kv() {
        let pool = pool_with_update_id("42");
        let body = handle_system_update_id(&pool).unwrap();
        assert!(body.contains("<u:GetSystemUpdateIDResponse"));
        assert!(body.contains("<Id>42</Id>"));
    }

    #[test]
    fn cd3_search_with_unsupported_criteria_returns_empty_didl() {
        // Unsupported criteria like `*` return empty results (SPEC §5.4).
        let (state, _db) = crate::state::test_helpers::test_state();
        let pool = state.db_pool.clone();
        let mut args = HashMap::new();
        args.insert("SearchCriteria".to_string(), "*".to_string());
        args.insert("ContainerID".to_string(), "0".to_string());
        let req = soap::SoapRequest {
            action: "Search".to_string(),
            args,
        };
        let body = handle(&pool, &state, &req).unwrap();
        assert!(body.contains("<u:SearchResponse"));
        assert!(body.contains("<NumberReturned>0</NumberReturned>"));
        assert!(body.contains("<TotalMatches>0</TotalMatches>"));
    }

    #[test]
    fn cd2_get_search_capabilities_returns_supported_props() {
        let req = soap::SoapRequest {
            action: "GetSearchCapabilities".to_string(),
            args: HashMap::new(),
        };
        let (state, _db) = crate::state::test_helpers::test_state();
        let pool = state.db_pool.clone();
        let body = handle(&pool, &state, &req).unwrap();
        assert!(body.contains("dc:title"));
        assert!(body.contains("upnp:artist"));
        assert!(body.contains("upnp:album"));
    }
}
