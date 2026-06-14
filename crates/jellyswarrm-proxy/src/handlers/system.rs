use axum::{
    extract::State,
    http::{header::HOST, HeaderMap},
    Json,
};
use hyper::StatusCode;
use tracing::error;

use crate::{
    extractors::RequireUser, handlers::common::execute_json_request, ui::JELLYFIN_UI_VERSION,
    AppState,
};

/// Build the externally-reachable base URL from the forwarding headers a reverse
/// proxy (NPM, Caddy, Traefik, Cloudflare, …) sets — i.e. the address the client
/// actually used to reach us. Returns `None` for a direct connection (no
/// forwarding header), so the caller falls back to the configured Public Address.
///
/// Reporting the real reach address (instead of a static setting) is what lets
/// Jellyfin clients auto-reconnect: on startup they re-test the same URL they
/// originally connected on, so a stale/placeholder address can't strand them on
/// the server-select screen.
fn forwarded_public_url(headers: &HeaderMap) -> Option<String> {
    let first = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let xf_host = first("x-forwarded-host");
    let xf_proto = first("x-forwarded-proto");

    // Only override when a reverse proxy is clearly in front of us; a direct
    // connection keeps the explicit Public Address setting.
    if xf_host.is_none() && xf_proto.is_none() {
        return None;
    }

    let host = xf_host.or_else(|| {
        headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })?;

    let scheme = xf_proto.unwrap_or_else(|| "https".to_string());
    Some(format!("{scheme}://{host}"))
}

pub async fn info_public(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let cfg = state.config.read().await;

    let local_address =
        forwarded_public_url(&headers).unwrap_or_else(|| cfg.public_address.clone());

    Ok(Json(crate::models::PublicServerInfo {
        id: cfg.server_id.clone(),
        server_name: cfg.server_name.clone(),
        local_address,
        version: JELLYFIN_UI_VERSION.clone().unwrap_or_default().version,
        product_name: "Jellyfin Server".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        startup_wizard_completed: true,
    }))
}

pub async fn info(
    State(state): State<AppState>,
    headers: HeaderMap,
    RequireUser { preprocessed, .. }: RequireUser,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
    match execute_json_request::<crate::models::ServerInfo>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut server_info) => {
            let cfg = state.config.read().await;
            server_info.id = cfg.server_id.clone();
            server_info.server_name = cfg.server_name.clone();
            server_info.local_address =
                forwarded_public_url(&headers).unwrap_or_else(|| cfg.public_address.clone());

            Ok(Json(server_info))
        }
        Err(e) => {
            error!("Failed to get server info: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
