use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::{config::save_config, AppState};

#[derive(Template)]
#[template(path = "admin/settings.html")]
pub struct SettingsPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/settings_form.html")]
pub struct SettingsFormTemplate {
    pub server_id: String,
    pub public_address: String,
    pub server_name: String,
    pub include_server_name_in_media: bool,
    pub auto_create_users_on_login: bool,
    pub web_client_host: String,
    /// URLs of the configured servers, offered as suggestions.
    ///
    /// A dropdown rather than a bare text field because the value is almost always one of these,
    /// and a typo saves silently and then serves nothing. It stays a free-text input so a dedicated
    /// plugin-host that is not a configured media server can still be entered.
    pub server_urls: Vec<String>,
    pub ui_route: String,
}

pub async fn settings_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = SettingsPageTemplate {
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render settings page: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn settings_form(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.config.read().await.clone();
    let form = SettingsFormTemplate {
        server_id: cfg.server_id,
        public_address: cfg.public_address,
        server_name: cfg.server_name,
        include_server_name_in_media: cfg.include_server_name_in_media,
        auto_create_users_on_login: cfg.auto_create_users_on_login,
        web_client_host: cfg.web_client_host,
        server_urls: state
            .server_storage
            .list_servers()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|server| server.url.as_str().trim_end_matches('/').to_string())
            .collect(),
        ui_route: state.get_ui_route().await,
    };
    match form.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render settings form: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Verifies that a candidate web client host actually answers with a client.
async fn probe_web_client_host(state: &AppState, host: &str) -> Result<(), String> {
    let url = format!("{host}/web/index.html");
    let response = state
        .reqwest_client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not reach it: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("it answered {}", response.status()));
    }

    Ok(())
}

#[derive(Deserialize)]
pub struct SaveForm {
    pub public_address: String,
    pub server_name: String,
    // When the checkbox is unchecked the field is absent; default to false.
    #[serde(default)]
    pub include_server_name_in_media: bool,
    #[serde(default)]
    pub auto_create_users_on_login: bool,
    // Optional: empty means "serve the bundled client".
    #[serde(default)]
    pub web_client_host: String,
}

pub async fn save_settings(State(state): State<AppState>, Form(form): Form<SaveForm>) -> Response {
    if form.public_address.trim().is_empty() || form.server_name.trim().is_empty() {
        return Html(
            "<div id=\"settings-messages\" class=\"alert alert-error\">All fields required</div>",
        )
        .into_response();
    }

    let web_client_host = form
        .web_client_host
        .trim()
        .trim_end_matches('/')
        .to_string();

    // Check the host actually serves a client before storing it. Saving an unreachable address is
    // not a harmless mistake: the browser client is served from it, so a typo makes the whole UI —
    // including this settings page — unavailable until someone edits the config file by hand.
    if !web_client_host.is_empty() {
        if let Err(reason) = probe_web_client_host(&state, &web_client_host).await {
            return Html(format!(
                "<div id=\"settings-messages\" class=\"alert alert-error\">                 Not saved: {web_client_host} does not serve a web client ({reason}).                  Leave it empty to use the bundled client.</div>"
            ))
            .into_response();
        }
    }

    let save_result = {
        let mut cfg = state.config.write().await;
        let mut updated = cfg.clone();
        updated.public_address = form.public_address.trim().to_string();
        updated.server_name = form.server_name.trim().to_string();
        updated.include_server_name_in_media = form.include_server_name_in_media;
        updated.auto_create_users_on_login = form.auto_create_users_on_login;
        // Stored without a trailing slash so it can be concatenated with request paths directly.
        updated.web_client_host = web_client_host;
        match save_config(&updated) {
            Ok(()) => {
                *cfg = updated;
                Ok(())
            }
            Err(error) => Err(error),
        }
    };
    if let Err(error) = save_result {
        error!("Failed to save settings: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<div id=\"settings-messages\" class=\"alert alert-error\">Could not save settings. The previous configuration is still active.</div>"),
        )
            .into_response();
    }

    settings_form(State(state)).await.into_response()
}

pub async fn reload_config(State(state): State<AppState>) -> impl IntoResponse {
    let new_cfg = crate::config::load_config();
    {
        let mut cfg = state.config.write().await;
        *cfg = new_cfg;
    }
    Html("<div class=\"alert\">Configuration reloaded</div>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(web_client_host: &str) -> String {
        SettingsFormTemplate {
            server_id: "server".to_string(),
            public_address: "http://localhost:8096".to_string(),
            server_name: "Jellyswarrm".to_string(),
            include_server_name_in_media: false,
            auto_create_users_on_login: true,
            web_client_host: web_client_host.to_string(),
            server_urls: vec![
                "http://one.example".to_string(),
                "http://two.example".to_string(),
            ],
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap()
    }

    #[test]
    fn settings_form_does_not_render_library_merging_control() {
        assert!(!form("").contains("name=\"merge_libraries\""));
    }

    /// Client-side plugins are injected into a *server's* web client, so the bundled copy carries
    /// none of them. Without a way to point at a server that has them, the whole plugin layer is
    /// unreachable from the UI and the setting can only be changed by hand-editing a TOML inside a
    /// container volume.
    #[test]
    fn settings_form_exposes_the_web_client_host() {
        let html = form("https://media.example.com");

        assert!(html.contains("name=\"web_client_host\""));
        assert!(
            html.contains("https://media.example.com"),
            "the current value must be shown so it can be seen and edited"
        );
    }

    /// Empty is a valid, meaningful value - it means "serve the bundled client" - so the field has
    /// to render when unset rather than being hidden.
    #[test]
    fn settings_form_shows_the_web_client_host_when_unset() {
        assert!(form("").contains("name=\"web_client_host\""));
    }
}
