//! Admin page for inspecting and installing plugins across the media servers.
//!
//! Plugins live on the servers, never on the proxy. What this adds is the cross-server view that no
//! individual server can give you: which servers have a plugin, at which versions, and where those
//! disagree.

use askama::Template;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Response},
    Form,
};
use hyper::StatusCode;
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    plugin_service::{
        add_repository, install_plugin, normalize_guid, read_all_inventories, read_catalogue,
        AvailablePlugin, InstallOutcome, PluginRepository, ServerPluginInventory,
    },
    server_id::ServerId,
    AppState,
};

#[derive(Template)]
#[template(path = "admin/plugins.html")]
pub struct PluginsPageTemplate {
    pub ui_route: String,
}

pub struct RepositoryView {
    pub name: String,
    pub url: String,
}

pub struct ServerView {
    pub id: i64,
    pub name: String,
    pub unreadable: bool,
    pub error: String,
    pub repositories: Vec<RepositoryView>,
}

pub struct CellView {
    pub server_id: i64,
    pub installed: bool,
    pub unreadable: bool,
    pub version: String,
}

pub struct RowView {
    pub name: String,
    pub guid: String,
    pub has_drift: bool,
    pub cells: Vec<CellView>,
}

#[derive(Template)]
#[template(path = "admin/plugin_inventory.html")]
pub struct PluginInventoryTemplate {
    pub ui_route: String,
    pub servers: Vec<ServerView>,
    pub rows: Vec<RowView>,
    pub unreadable_count: usize,
    pub message: String,
}

pub async fn plugins_page(State(state): State<AppState>) -> Response {
    let ui_route = state.config.read().await.ui_route.to_string();
    match (PluginsPageTemplate { ui_route }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render plugins page: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Builds the cross-server matrix from each server's inventory.
///
/// Rows are keyed on the plugin GUID rather than the display name: the same plugin can be named
/// slightly differently between versions, and merging on the name would split one plugin into two
/// rows that each look half-installed.
fn build_inventory_view(
    ui_route: String,
    inventories: &[ServerPluginInventory],
    catalogue: &[AvailablePlugin],
    message: String,
) -> PluginInventoryTemplate {
    let servers: Vec<ServerView> = inventories
        .iter()
        .map(|inv| ServerView {
            id: inv.server_id.as_i64(),
            name: inv.server_name.clone(),
            unreadable: !inv.is_readable(),
            error: inv.error.clone().unwrap_or_default(),
            repositories: inv
                .repositories
                .iter()
                .map(|r| RepositoryView {
                    name: r.name.clone(),
                    url: r.url.clone(),
                })
                .collect(),
        })
        .collect();

    // Collect every plugin seen anywhere, keyed by normalized guid, keeping first-seen display name.
    let mut order: Vec<(String, String, String)> = Vec::new(); // (key, name, raw guid)
    for inv in inventories {
        let Some(plugins) = inv.plugins.as_ref() else {
            continue;
        };
        for plugin in plugins {
            let key = normalize_guid(&plugin.id);
            if !order.iter().any(|(existing, _, _)| existing == &key) {
                order.push((key, plugin.name.clone(), plugin.id.clone()));
            }
        }
    }
    order.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

    let rows = order
        .into_iter()
        .map(|(key, name, raw_guid)| {
            let cells: Vec<CellView> = inventories
                .iter()
                .map(|inv| {
                    let version = inv.installed_version(&key).unwrap_or_default().to_string();
                    CellView {
                        server_id: inv.server_id.as_i64(),
                        installed: inv.has_plugin(&key),
                        unreadable: !inv.is_readable(),
                        version,
                    }
                })
                .collect();

            // Drift is only meaningful between servers that actually have the plugin. A server that
            // deliberately does not have it is not "behind", and reporting it as such would make
            // the warning meaningless on any deployment with per-server plugin choices.
            let mut versions: Vec<&str> = cells
                .iter()
                .filter(|c| c.installed && !c.version.is_empty())
                .map(|c| c.version.as_str())
                .collect();
            versions.sort_unstable();
            versions.dedup();

            RowView {
                name,
                guid: raw_guid,
                has_drift: versions.len() > 1,
                cells,
            }
        })
        .collect();

    // Plugins offered by a repository but not installed anywhere yet. Without these the page can
    // only ever propagate what some server already has, which would make it useless for setting up
    // a new server from scratch.
    let mut rows: Vec<RowView> = rows;
    for available in catalogue {
        let key = normalize_guid(&available.guid);
        if key.is_empty() || rows.iter().any(|r| normalize_guid(&r.guid) == key) {
            continue;
        }
        rows.push(RowView {
            name: available.name.clone(),
            guid: available.guid.clone(),
            has_drift: false,
            cells: inventories
                .iter()
                .map(|inv| CellView {
                    server_id: inv.server_id.as_i64(),
                    installed: false,
                    unreadable: !inv.is_readable(),
                    version: String::new(),
                })
                .collect(),
        });
    }
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let unreadable_count = inventories.iter().filter(|i| !i.is_readable()).count();

    PluginInventoryTemplate {
        ui_route,
        servers,
        rows,
        unreadable_count,
        message,
    }
}

async fn render_inventory(state: &AppState, message: String) -> Response {
    let ui_route = state.config.read().await.ui_route.to_string();
    let inventories = read_all_inventories(state).await;

    // The catalogue is the union of what each readable server's repositories offer. Reading it from
    // every server rather than just the first matters because repository lists can differ.
    let mut catalogue: Vec<AvailablePlugin> = Vec::new();
    for inv in inventories.iter().filter(|i| i.is_readable()) {
        if let Ok(available) = read_catalogue(state, inv.server_id).await {
            for plugin in available {
                let key = normalize_guid(&plugin.guid);
                if !key.is_empty()
                    && !catalogue
                        .iter()
                        .any(|existing| normalize_guid(&existing.guid) == key)
                {
                    catalogue.push(plugin);
                }
            }
        }
    }

    let template = build_inventory_view(ui_route, &inventories, &catalogue, message);

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render plugin inventory: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn plugin_inventory(State(state): State<AppState>) -> Response {
    render_inventory(&state, String::new()).await
}

#[derive(Deserialize)]
pub struct InstallForm {
    pub server_id: i64,
    pub plugin_name: String,
    pub plugin_guid: String,
}

pub async fn install(State(state): State<AppState>, Form(form): Form<InstallForm>) -> Response {
    let server_id = ServerId::new(form.server_id);
    let outcome = install_plugin(
        &state,
        server_id,
        &form.plugin_name,
        &form.plugin_guid,
        None,
    )
    .await;

    let message = match outcome {
        InstallOutcome::Installing => {
            info!(
                "Requested install of {} on {:?}",
                form.plugin_name, server_id
            );
            format!(
                "{} is installing. It stays inactive until that server restarts.",
                form.plugin_name
            )
        }
        InstallOutcome::AlreadyInstalled { version } => format!(
            "{} is already installed at {version}; nothing was sent.",
            form.plugin_name
        ),
        InstallOutcome::Failed { reason } => {
            error!("Install of {} failed: {reason}", form.plugin_name);
            format!("Could not install {}: {reason}", form.plugin_name)
        }
    };

    render_inventory(&state, message).await
}

#[derive(Deserialize)]
pub struct RepositoryForm {
    pub name: String,
    pub url: String,
    /// A server id, or `all` to apply to every readable server.
    pub server_id: String,
}

pub async fn add_repository_form(
    State(state): State<AppState>,
    Form(form): Form<RepositoryForm>,
) -> Response {
    let repository = PluginRepository {
        name: form.name.clone(),
        url: form.url.clone(),
        enabled: true,
    };

    let targets: Vec<ServerId> = if form.server_id.eq_ignore_ascii_case("all") {
        read_all_inventories(&state)
            .await
            .into_iter()
            .filter(|inv| inv.is_readable())
            .map(|inv| inv.server_id)
            .collect()
    } else {
        match form.server_id.parse::<i64>() {
            Ok(id) => vec![ServerId::new(id)],
            Err(_) => {
                return render_inventory(&state, "Invalid server selection.".to_string()).await;
            }
        }
    };

    let mut added = 0usize;
    let mut failures = Vec::new();
    for server_id in targets {
        match add_repository(&state, server_id, repository.clone()).await {
            Ok(()) => added += 1,
            Err(e) => failures.push(format!("{:?}: {e}", server_id)),
        }
    }

    let message = if failures.is_empty() {
        format!("Repository '{}' added to {added} server(s).", form.name)
    } else {
        format!(
            "Repository '{}' added to {added} server(s); {} failed ({}).",
            form.name,
            failures.len(),
            failures.join("; ")
        )
    };

    render_inventory(&state, message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_service::InstalledPlugin;

    fn plugin(id: &str, name: &str, version: &str) -> InstalledPlugin {
        InstalledPlugin {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            status: "Active".to_string(),
        }
    }

    fn inv(id: i64, name: &str, plugins: Option<Vec<InstalledPlugin>>) -> ServerPluginInventory {
        ServerPluginInventory {
            server_id: ServerId::new(id),
            server_name: name.to_string(),
            plugins,
            repositories: Vec::new(),
            error: None,
        }
    }

    /// The whole point of the page: a plugin present on one server and absent on another shows as
    /// exactly that, with an install offered only where it is missing.
    #[test]
    fn a_plugin_on_one_server_offers_install_on_the_other() {
        let inventories = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("abc-123", "Home Screen Sections", "2.5.11.0")]),
            ),
            inv(2, "Beta", Some(Vec::new())),
        ];

        let view = build_inventory_view("ui".into(), &inventories, &[], String::new());

        assert_eq!(view.rows.len(), 1);
        let row = &view.rows[0];
        assert!(row.cells[0].installed, "Alpha has it");
        assert!(!row.cells[1].installed, "Beta does not");
        assert!(!row.has_drift, "one version in play is not drift");
        assert_eq!(view.unreadable_count, 0);
    }

    /// Drift is only meaningful between servers that actually have the plugin. Counting a
    /// deliberately-absent server as "behind" would make the warning fire constantly on any
    /// deployment that chooses plugins per server.
    #[test]
    fn version_drift_ignores_servers_that_do_not_have_the_plugin() {
        let absent = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("abc", "Media Bar", "2.4.12.0")]),
            ),
            inv(2, "Beta", Some(Vec::new())),
        ];
        assert!(!build_inventory_view("ui".into(), &absent, &[], String::new()).rows[0].has_drift);

        let mismatched = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("abc", "Media Bar", "2.4.12.0")]),
            ),
            inv(2, "Beta", Some(vec![plugin("abc", "Media Bar", "2.4.0.0")])),
        ];
        assert!(
            build_inventory_view("ui".into(), &mismatched, &[], String::new()).rows[0].has_drift,
            "two different versions across servers is drift"
        );
    }

    /// An unreachable server must never render an Install button: it may already have the plugin,
    /// and offering to install would be acting on an unknown.
    #[test]
    fn an_unreadable_server_is_never_offered_for_install() {
        let inventories = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("abc", "Custom Tabs", "0.2.10.0")]),
            ),
            inv(2, "Beta", None),
        ];

        let view = build_inventory_view("ui".into(), &inventories, &[], String::new());
        let beta = &view.rows[0].cells[1];

        assert!(beta.unreadable);
        assert!(!beta.installed);
        assert_eq!(view.unreadable_count, 1);
    }

    /// Rows key on the guid, so the same plugin reported with differing id formatting or a renamed
    /// display string stays one row rather than splitting into two half-installed ones.
    #[test]
    fn the_same_plugin_stays_one_row_across_id_formatting() {
        let inventories = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin(
                    "5e87cc92-8f8a-4b2f-9c3d-1a2b3c4d5e6f",
                    "File Transformation",
                    "2.5.11.0",
                )]),
            ),
            inv(
                2,
                "Beta",
                Some(vec![plugin(
                    "5E87CC928F8A4B2F9C3D1A2B3C4D5E6F",
                    "File Transformation",
                    "2.5.11.0",
                )]),
            ),
        ];

        let view = build_inventory_view("ui".into(), &inventories, &[], String::new());

        assert_eq!(view.rows.len(), 1, "one plugin, one row");
        assert!(view.rows[0].cells.iter().all(|c| c.installed));
    }
}
