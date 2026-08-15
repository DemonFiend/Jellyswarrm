use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::error;

use crate::{
    extractors::RequireUser, handlers::common::execute_json_request,
    request_preprocessing::JellyfinAuthorization, ui::JELLYFIN_UI_VERSION, AppState,
};

/// Whether a string is shaped like a Jellyfin server version - dot-separated numbers.
///
/// jellyfin-web parses this value and refuses to start with "Update Required" if it cannot, so a
/// malformed value is indistinguishable to the user from a genuinely unsupported server. The
/// version is read from a generated `ui-version.env`, and a build that skips UI generation can
/// leave a stale or malformed file behind. One observed case embedded a literal backslash-n
/// instead of a newline, so the whole remainder of the file parsed as the "version" and the client
/// would not load at all.
///
/// Validating the shape here means a bad file degrades to the fallback rather than bricking the
/// client.
fn is_version_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').count() >= 2
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn reported_server_version() -> String {
    // Jellyfin Web refuses to load when Version is empty/unknown ("Update Required").
    // Always report the embedded web client's version so the UI stays compatible.
    let version = JELLYFIN_UI_VERSION
        .clone()
        .unwrap_or_default()
        .version
        .trim()
        .to_string();
    if !is_version_shaped(&version) {
        // Fallback for test builds that skip ui-version.env generation, and for a malformed file.
        "10.11.0".to_string()
    } else {
        version
    }
}

pub async fn info_public(
    State(state): State<AppState>,
) -> Result<Json<crate::models::PublicServerInfo>, StatusCode> {
    let cfg = state.config.read().await;

    Ok(Json(crate::models::PublicServerInfo {
        id: cfg.server_id.clone(),
        server_name: cfg.server_name.clone(),
        local_address: cfg.public_address.clone(),
        version: reported_server_version(),
        product_name: "Jellyfin Server".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        startup_wizard_completed: true,
    }))
}

pub async fn info(
    State(state): State<AppState>,
    RequireUser { preprocessed, .. }: RequireUser,
) -> Result<Json<crate::models::ServerInfo>, StatusCode> {
    let is_seerr = matches!(
        preprocessed.auth.as_ref(),
        Some(JellyfinAuthorization::Authorization(authorization))
            if authorization.client.eq_ignore_ascii_case("Seerr")
                && authorization.device.eq_ignore_ascii_case("Seerr")
    );
    if !is_seerr {
        let mut server_info = execute_json_request::<crate::models::ServerInfo>(
            &state.reqwest_client,
            preprocessed.request,
        )
        .await
        .inspect_err(|status| {
            error!("Failed to get upstream server info: {status}");
        })?;
        let cfg = state.config.read().await;
        server_info.id = cfg.server_id.clone();
        server_info.server_name = cfg.server_name.clone();
        server_info.local_address = cfg.public_address.clone();
        server_info.version = Some(reported_server_version());
        return Ok(Json(server_info));
    }

    let cfg = state.config.read().await;
    Ok(Json(crate::models::ServerInfo {
        operating_system_display_name: Some(std::env::consts::OS.to_string()),
        has_pending_restart: Some(false),
        is_shutting_down: Some(false),
        supports_library_monitor: Some(false),
        web_socket_port_number: None,
        completed_installations: None,
        can_self_restart: Some(false),
        can_launch_web_browser: Some(false),
        program_data_path: None,
        web_path: None,
        items_by_name_path: None,
        cache_path: None,
        log_path: None,
        internal_metadata_path: None,
        transcoding_temp_path: None,
        cast_receiver_applications: None,
        has_update_available: Some(false),
        encoder_location: None,
        system_architecture: Some(std::env::consts::ARCH.to_string()),
        local_address: cfg.public_address.clone(),
        server_name: cfg.server_name.clone(),
        version: Some(reported_server_version()),
        operating_system: Some(std::env::consts::OS.to_string()),
        id: cfg.server_id.clone(),
        startup_wizard_completed: Some(true),
    }))
}
