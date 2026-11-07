//! The Plugins page's view of what the pinned client actually serves.
//!
//! A plugin's assets classify themselves — appearing in the client document as a script or a
//! stylesheet is what proves they are presentation — so they are pinned without asking. A plugin's
//! own endpoints prove nothing of the sort, and the wrong guess is expensive: pinning one that
//! returns media items makes that surface single-server, and the symptom is a page that renders
//! with part of the catalogue missing. So this page reports them and asks.

use askama::Template;
use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use tracing::error;

use crate::{
    plugin_discovery::{self, DataPolicy},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/plugin_surfaces.html")]
pub struct PluginSurfacesTemplate {
    pub ui_route: String,
    /// Without a pinned host there is no client document to read, and no client-side plugin
    /// reaches a browser at all — so say that rather than showing an empty table.
    pub host_configured: bool,
    pub surfaces: Vec<SurfaceView>,
    pub any_surfaces: bool,
    pub unclassified_count: usize,
    pub message: String,
}

pub struct SurfaceView {
    pub namespace: String,
    pub assets: Vec<String>,
    pub asset_count: usize,
    /// Assets with no policy yet. Normally empty, because a pass pins them; a non-empty list means
    /// the pinned host changed since the last pass, or saving the policy failed.
    pub unpinned: Vec<String>,
    pub any_unpinned: bool,
    pub policy_label: String,
    pub unclassified: bool,
    pub merged_paths: Vec<String>,
    pub any_merged_paths: bool,
}

async fn render(state: &AppState, message: String) -> Response {
    let (ui_route, host_configured) = {
        let config = state.config.read().await;
        (
            config.ui_route.to_string(),
            !config.web_client_host.trim().is_empty(),
        )
    };

    let surfaces: Vec<SurfaceView> = if host_configured {
        plugin_discovery::survey(state)
            .await
            .into_iter()
            .map(|status| SurfaceView {
                asset_count: status.assets.len(),
                any_unpinned: !status.unpinned_assets.is_empty(),
                policy_label: status.data_policy.label().to_string(),
                unclassified: status.data_policy == DataPolicy::Unclassified,
                any_merged_paths: !status.merged_paths.is_empty(),
                namespace: status.namespace,
                assets: status.assets,
                unpinned: status.unpinned_assets,
                merged_paths: status.merged_paths,
            })
            .collect()
    } else {
        Vec::new()
    };

    let template = PluginSurfacesTemplate {
        ui_route,
        host_configured,
        unclassified_count: surfaces.iter().filter(|s| s.unclassified).count(),
        any_surfaces: !surfaces.is_empty(),
        surfaces,
        message,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            error!("Failed to render the plugin surfaces view: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn alert(kind: &str, body: &str) -> String {
    format!("<div id=\"plugin-surfaces-messages\" class=\"alert alert-{kind}\">{body}</div>")
}

/// `GET /{ui}/plugins/surfaces` — read-only.
pub async fn surfaces_fragment(State(state): State<AppState>) -> Response {
    render(&state, String::new()).await
}

/// `POST /{ui}/plugins/surfaces/scan` — re-read the client document and pin anything new.
///
/// The same pass that runs at start-up. Offered as an action because a plugin installed since then
/// is not visible until the document is read again.
pub async fn scan(State(state): State<AppState>) -> Response {
    let outcome = plugin_discovery::discover_and_enable_assets(&state).await;

    let message = if !outcome.enabled.is_empty() {
        alert(
            "success",
            &format!(
                "Pinned {} newly discovered asset path(s). {} plugin(s) still need their data \
                 endpoints classified below.",
                outcome.enabled.len(),
                outcome.awaiting_classification.len()
            ),
        )
    } else if outcome.surfaces.is_empty() {
        alert(
            "error",
            "No plugin assets were found in the client. Either no client-side plugin is installed \
             on the pinned server, or File Transformation is not injecting into its web client.",
        )
    } else {
        alert(
            "success",
            &format!(
                "{} plugin surface(s) found, all already covered by policy.",
                outcome.surfaces.len()
            ),
        )
    };

    render(&state, message).await
}

#[derive(Deserialize)]
pub struct ClassifyForm {
    pub namespace: String,
    pub policy: String,
}

/// `POST /{ui}/plugins/surfaces/classify` — decide how a plugin's own endpoints are answered.
pub async fn classify(State(state): State<AppState>, Form(form): Form<ClassifyForm>) -> Response {
    let policy = match form.policy.as_str() {
        "pinned" => DataPolicy::Pinned,
        "merged" => DataPolicy::Merged,
        "unclassified" => DataPolicy::Unclassified,
        other => {
            return render(
                &state,
                alert("error", &format!("Unknown policy {other:?}.")),
            )
            .await
        }
    };

    let message = match plugin_discovery::classify_namespace(&state, &form.namespace, policy).await
    {
        Ok(()) => alert(
            "success",
            &format!(
                "{} is now answered: {}.",
                form.namespace,
                policy.label().to_lowercase()
            ),
        ),
        Err(error) => alert("error", &format!("Not saved: {error}")),
    };

    render(&state, message).await
}
