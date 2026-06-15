use axum::{
    extract::State,
    http::{header::HOST, HeaderMap},
    Json,
};
use hyper::StatusCode;
use tracing::warn;

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
    // Snapshot the config-owned identity up front so we never hold the config lock across
    // the upstream round-trip, and can still answer if that round-trip fails.
    let (server_id, server_name, public_address) = {
        let cfg = state.config.read().await;
        (
            cfg.server_id.clone(),
            cfg.server_name.clone(),
            cfg.public_address.clone(),
        )
    };
    let local_address = forwarded_public_url(&headers).unwrap_or(public_address);

    match execute_json_request::<crate::models::ServerInfo>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(mut server_info) => {
            server_info.id = server_id;
            server_info.server_name = server_name;
            server_info.local_address = local_address;
            Ok(Json(server_info))
        }
        // `/System/Info` is the client's "am I still signed in?" probe on every reconnect /
        // app-foreground; failing it drops the client to the login screen. The identity fields
        // here are config-owned anyway, so a flaky, slow, or token-less upstream round-trip must
        // not be fatal — answer locally with a 200 instead.
        Err(e) => {
            warn!("Upstream /System/Info failed ({e:?}); serving config-derived server info");
            Ok(Json(local_server_info(server_id, server_name, local_address)))
        }
    }
}

/// A fully-local `ServerInfo` built from proxy config, used when the upstream round-trip
/// fails so `/System/Info` can never force the client to re-login.
fn local_server_info(
    id: String,
    server_name: String,
    local_address: String,
) -> crate::models::ServerInfo {
    crate::models::ServerInfo {
        operating_system_display_name: None,
        has_pending_restart: None,
        is_shutting_down: None,
        supports_library_monitor: None,
        web_socket_port_number: None,
        completed_installations: None,
        can_self_restart: None,
        can_launch_web_browser: None,
        program_data_path: None,
        web_path: None,
        items_by_name_path: None,
        cache_path: None,
        log_path: None,
        internal_metadata_path: None,
        transcoding_temp_path: None,
        cast_receiver_applications: None,
        has_update_available: None,
        encoder_location: None,
        system_architecture: None,
        local_address,
        server_name,
        version: Some(JELLYFIN_UI_VERSION.clone().unwrap_or_default().version),
        operating_system: Some(std::env::consts::OS.to_string()),
        id,
        startup_wizard_completed: Some(true),
    }
}
