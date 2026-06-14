use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    Json,
};
use tracing::{debug, error, warn};

use crate::{request_preprocessing::resolve_request_identity_from_headers_uri, AppState};

/// Resolve the Jellyswarrm virtual user id from the request's auth header or
/// `userId` query param, without consuming the body (so a body extractor can run too).
async fn resolve_user_id(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<String, StatusCode> {
    let identity = resolve_request_identity_from_headers_uri(headers, uri, state)
        .await
        .map_err(|e| {
            error!("DisplayPreferences: failed to resolve request identity: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    identity.user.map(|u| u.id).ok_or(StatusCode::UNAUTHORIZED)
}

/// Read a raw (non-decoded) query-string value. Jellyfin client/userId values
/// don't require percent-decoding, so this avoids pulling in a parser and avoids
/// the 400-on-malformed-query behavior of the `Query` extractor.
fn query_param(uri: &Uri, key: &str) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return parts.next().map(|v| v.to_string());
        }
    }
    None
}

/// Minimal-but-complete default returned the first time a client asks for a
/// DisplayPreferences id we have nothing stored for. The client then renders
/// defaults and POSTs back its customization, which we persist.
fn default_display_preferences(prefs_id: &str, client: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": prefs_id,
        "SortBy": "SortName",
        "RememberIndexing": false,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250,
        "CustomPrefs": {},
        "ScrollDirection": "Horizontal",
        "ShowBackdrop": true,
        "RememberSorting": false,
        "SortOrder": "Ascending",
        "ShowSidebar": false,
        "Client": client,
    })
}

/// `GET /DisplayPreferences/{id}` — served locally from the proxy's own store.
pub async fn handle_get(
    State(state): State<AppState>,
    Path(prefs_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let user_id = resolve_user_id(&state, &headers, &uri).await?;
    let client = query_param(&uri, "client").unwrap_or_default();

    match state
        .display_preferences
        .get_display_preferences(&user_id, &client, &prefs_id)
        .await
    {
        Ok(Some(data)) => Ok(Json(data)),
        Ok(None) => {
            // Fall back to the admin-defined default Home layout (home sections), if
            // one is set — so un-customized users inherit it on every device.
            if prefs_id == crate::display_preferences_service::USERSETTINGS_PREFS_ID {
                if let Ok(Some(mut def)) =
                    state.display_preferences.get_default_display_preferences().await
                {
                    debug!(
                        "Serving admin default DisplayPreferences to user {} client {}",
                        user_id, client
                    );
                    def["Id"] = serde_json::Value::String(prefs_id.clone());
                    def["Client"] = serde_json::Value::String(client.clone());
                    return Ok(Json(def));
                }
            }
            debug!(
                "No stored DisplayPreferences for user {} client {} id {}; returning generic default",
                user_id, client, prefs_id
            );
            Ok(Json(default_display_preferences(&prefs_id, &client)))
        }
        Err(e) => {
            error!("Failed to read DisplayPreferences: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /DisplayPreferences/{id}` — stored locally; never forwarded upstream.
pub async fn handle_set(
    State(state): State<AppState>,
    Path(prefs_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> StatusCode {
    let user_id = match resolve_user_id(&state, &headers, &uri).await {
        Ok(id) => id,
        Err(code) => return code,
    };
    let client = query_param(&uri, "client").unwrap_or_default();

    let data: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!("DisplayPreferences POST body was not valid JSON: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };

    match state
        .display_preferences
        .set_display_preferences(&user_id, &client, &prefs_id, &data)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(e) => {
            error!("Failed to store DisplayPreferences: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// `POST /Users/{user_id}/Configuration` — persists the user's configuration
/// (notably `OrderedViews`, the library order) on the proxy instead of
/// forwarding it to a single upstream.
pub async fn handle_post_user_configuration(
    State(state): State<AppState>,
    Path(_user_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> StatusCode {
    let user_id = match resolve_user_id(&state, &headers, &uri).await {
        Ok(id) => id,
        Err(code) => return code,
    };

    let data: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!("UserConfiguration POST body was not valid JSON: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };

    match state
        .display_preferences
        .set_user_configuration(&user_id, &data)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(e) => {
            error!("Failed to store UserConfiguration: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
