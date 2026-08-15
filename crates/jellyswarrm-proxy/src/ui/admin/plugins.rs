//! Admin page for inspecting and installing plugins across the media servers.
//!
//! Plugins live on the servers, never on the proxy. What this adds is the cross-server view no
//! individual server can give you: which servers have a plugin, at which versions, and where those
//! disagree.
//!
//! The page is organised around the observation that most installs are the same action repeated
//! across servers. Rather than a cell per server per plugin — which grows unusably wide, and which
//! the official catalogue alone would blow up to hundreds of rows — the target servers are chosen
//! once at the top, plugins are grouped by the repository offering them, and each plugin is a
//! single row whose Install button applies to every selected server that lacks it.

use std::collections::BTreeMap;

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
        add_repository, awaiting_restart, install_plugin, normalize_guid, read_all_inventories,
        read_catalogue, restart_server, AvailablePlugin, InstallOutcome, PluginRepository,
        ServerPluginInventory,
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
    pub installed_count: usize,
    pub restart_count: usize,
    pub repositories: Vec<RepositoryView>,
}

pub struct RowView {
    pub name: String,
    pub guid: String,
    pub has_drift: bool,
    /// Human-readable per-server state, e.g. "Alpha 2.5.11.0 · Beta 2.4.0.0".
    pub state_summary: String,
    /// Whether at least one readable server lacks it, which is what makes Install meaningful.
    pub missing_anywhere: bool,
}

pub struct GroupView {
    pub name: String,
    pub rows: Vec<RowView>,
    pub total: usize,
    pub installed_anywhere: usize,
    pub drift_count: usize,
    /// Groups start collapsed; only ones with a version disagreement open on their own.
    pub expanded: bool,
}

#[derive(Template)]
#[template(path = "admin/plugin_inventory.html")]
pub struct PluginInventoryTemplate {
    pub ui_route: String,
    pub servers: Vec<ServerView>,
    pub groups: Vec<GroupView>,
    pub unreadable_count: usize,
    pub restart_needed: bool,
    pub restart_count: usize,
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

/// Installed plugins carry no repository information — Jellyfin only records where something came
/// from in the catalogue — so one that no repository still offers gets its own bucket rather than
/// being filed under an arbitrary repository or disappearing from the page entirely.
const INSTALLED_ONLY_GROUP: &str = "Installed (no longer offered by a repository)";

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
            installed_count: inv.plugins.as_ref().map(|p| p.len()).unwrap_or(0),
            restart_count: awaiting_restart(inv),
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

    let mut installed_names: BTreeMap<String, String> = BTreeMap::new();
    for inv in inventories {
        let Some(plugins) = inv.plugins.as_ref() else {
            continue;
        };
        for plugin in plugins {
            installed_names
                .entry(normalize_guid(&plugin.id))
                .or_insert_with(|| plugin.name.clone());
        }
    }

    let mut catalogue_index: BTreeMap<String, (String, String)> = BTreeMap::new();
    for plugin in catalogue {
        let key = normalize_guid(&plugin.guid);
        if key.is_empty() {
            continue;
        }
        catalogue_index
            .entry(key)
            .or_insert_with(|| (plugin.name.clone(), plugin.repository.clone()));
    }

    let mut all_keys: Vec<String> = installed_names.keys().cloned().collect();
    for key in catalogue_index.keys() {
        if !all_keys.contains(key) {
            all_keys.push(key.clone());
        }
    }

    let mut grouped: BTreeMap<String, Vec<RowView>> = BTreeMap::new();

    for key in all_keys {
        let (name, group) = match catalogue_index.get(&key) {
            Some((name, repo)) => (name.clone(), repo.clone()),
            None => (
                installed_names.get(&key).cloned().unwrap_or_default(),
                INSTALLED_ONLY_GROUP.to_string(),
            ),
        };

        let mut versions: Vec<&str> = Vec::new();
        let mut parts: Vec<String> = Vec::new();
        let mut missing_anywhere = false;

        for inv in inventories {
            // An unreadable server is unknown, not known-missing. Counting it as missing would
            // offer an install against a server that may already have the plugin.
            if !inv.is_readable() {
                continue;
            }
            if inv.has_plugin(&key) {
                let version = inv.installed_version(&key).unwrap_or_default();
                let shown = if version.is_empty() {
                    "installed"
                } else {
                    version
                };
                parts.push(format!("{} {}", inv.server_name, shown));
                if !version.is_empty() {
                    versions.push(version);
                }
            } else {
                missing_anywhere = true;
            }
        }

        versions.sort_unstable();
        versions.dedup();

        grouped.entry(group).or_default().push(RowView {
            name,
            guid: key,
            has_drift: versions.len() > 1,
            state_summary: parts.join(" · "),
            missing_anywhere,
        });
    }

    let mut groups: Vec<GroupView> = grouped
        .into_iter()
        .map(|(name, mut rows)| {
            rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            let installed_anywhere = rows.iter().filter(|r| !r.state_summary.is_empty()).count();
            let drift_count = rows.iter().filter(|r| r.has_drift).count();
            GroupView {
                // Collapsed by default. The summary line already carries the counts, so a group can
                // be judged without opening it; only something actually wrong — versions
                // disagreeing across servers — is worth forcing into view.
                expanded: drift_count > 0,
                total: rows.len(),
                installed_anywhere,
                drift_count,
                rows,
                name,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        b.installed_anywhere
            .cmp(&a.installed_anywhere)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let unreadable_count = inventories.iter().filter(|i| !i.is_readable()).count();
    let restart_count = inventories
        .iter()
        .filter(|i| awaiting_restart(i) > 0)
        .count();

    PluginInventoryTemplate {
        ui_route,
        servers,
        groups,
        unreadable_count,
        restart_needed: restart_count > 0,
        restart_count,
        message,
    }
}

async fn gather(state: &AppState) -> (Vec<ServerPluginInventory>, Vec<AvailablePlugin>) {
    let inventories = read_all_inventories(state).await;

    // Union of what each readable server's repositories offer; the lists can differ per server.
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

    (inventories, catalogue)
}

async fn render_inventory(state: &AppState, message: String) -> Response {
    let ui_route = state.config.read().await.ui_route.to_string();
    let (inventories, catalogue) = gather(state).await;
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
    /// `guid|display name`, so one button carries both without a second lookup.
    pub plugin: String,
    /// Repeated checkbox values naming the servers to install on.
    #[serde(default)]
    pub target: Vec<i64>,
}

pub async fn install(State(state): State<AppState>, Form(form): Form<InstallForm>) -> Response {
    let Some((guid, name)) = form.plugin.split_once('|') else {
        return render_inventory(&state, "Malformed plugin selection.".to_string()).await;
    };
    let (guid, name) = (guid.to_string(), name.to_string());

    if form.target.is_empty() {
        return render_inventory(
            &state,
            "No target servers selected - tick at least one server above.".to_string(),
        )
        .await;
    }

    let mut installing = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for server_id in form.target.iter().copied().map(ServerId::new) {
        let server_name = state
            .server_storage
            .get_server_by_id(server_id)
            .await
            .ok()
            .flatten()
            .map(|s| s.name)
            .unwrap_or_else(|| format!("server {}", server_id.as_i64()));

        match install_plugin(&state, server_id, &name, &guid, None).await {
            InstallOutcome::Installing => {
                info!("Installing {name} on {server_name}");
                installing.push(server_name);
            }
            InstallOutcome::AlreadyInstalled { .. } => skipped.push(server_name),
            InstallOutcome::Failed { reason } => {
                error!("Install of {name} on {server_name} failed: {reason}");
                failed.push(format!("{server_name} ({reason})"));
            }
        }
    }

    let mut message = String::new();
    if !installing.is_empty() {
        message.push_str(&format!(
            "{name} installing on {}. Not active until those servers restart. ",
            installing.join(", ")
        ));
    }
    if !skipped.is_empty() {
        message.push_str(&format!("Already present on {}. ", skipped.join(", ")));
    }
    if !failed.is_empty() {
        message.push_str(&format!("Failed on {}.", failed.join("; ")));
    }

    render_inventory(&state, message.trim().to_string()).await
}

/// Restarts every server that has a plugin staged but not loaded.
///
/// Scoped to those servers deliberately: restarting one with nothing pending would interrupt
/// playback for no benefit.
pub async fn restart_pending(State(state): State<AppState>) -> Response {
    let inventories = read_all_inventories(&state).await;
    let pending: Vec<&ServerPluginInventory> = inventories
        .iter()
        .filter(|inv| awaiting_restart(inv) > 0)
        .collect();

    if pending.is_empty() {
        return render_inventory(&state, "No server is waiting on a restart.".to_string()).await;
    }

    let mut restarted = Vec::new();
    let mut failed = Vec::new();
    for inv in pending {
        match restart_server(&state, inv.server_id).await {
            Ok(()) => {
                info!("Requested restart of {}", inv.server_name);
                restarted.push(inv.server_name.clone());
            }
            Err(e) => failed.push(format!("{} ({e})", inv.server_name)),
        }
    }

    let mut message = String::new();
    if !restarted.is_empty() {
        message.push_str(&format!(
            "Restarting {}. They will be briefly unavailable; refresh in a moment to confirm the plugins loaded. ",
            restarted.join(", ")
        ));
    }
    if !failed.is_empty() {
        message.push_str(&format!("Could not restart {}.", failed.join("; ")));
    }

    render_inventory(&state, message.trim().to_string()).await
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
    use crate::plugin_service::{AvailableVersion, InstalledPlugin};

    fn plugin(id: &str, name: &str, version: &str, status: &str) -> InstalledPlugin {
        InstalledPlugin {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            status: status.to_string(),
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

    fn available(guid: &str, name: &str, repo: &str) -> AvailablePlugin {
        AvailablePlugin {
            name: name.to_string(),
            guid: guid.to_string(),
            description: String::new(),
            versions: vec![AvailableVersion {
                version: "1.0.0".to_string(),
                repository_name: repo.to_string(),
                repository_url: String::new(),
            }],
            repository: repo.to_string(),
        }
    }

    #[test]
    fn plugins_are_grouped_by_repository() {
        let inventories = vec![inv(1, "Alpha", Some(Vec::new()))];
        let catalogue = vec![
            available("aaa", "Media Bar", "IAmParadox"),
            available("bbb", "Home Screen Sections", "IAmParadox"),
            available("ccc", "Some Official Thing", "Jellyfin Stable"),
        ];

        let view = build_inventory_view("ui".into(), &inventories, &catalogue, String::new());
        let names: Vec<&str> = view.groups.iter().map(|g| g.name.as_str()).collect();

        assert!(names.contains(&"IAmParadox"));
        assert!(names.contains(&"Jellyfin Stable"));
        assert_eq!(
            view.groups
                .iter()
                .find(|g| g.name == "IAmParadox")
                .unwrap()
                .total,
            2
        );
    }

    /// Everything starts collapsed - the summary line carries the counts, so a group can be judged
    /// without opening it. Only a version disagreement, which is a real problem, forces itself open.
    #[test]
    fn groups_start_collapsed_unless_something_disagrees() {
        let ordinary = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(2, "Beta", Some(Vec::new())),
        ];
        let catalogue = vec![available("aaa", "Media Bar", "IAmParadox")];

        let view = build_inventory_view("ui".into(), &ordinary, &catalogue, String::new());
        assert!(
            !view.groups[0].expanded,
            "an ordinary group stays collapsed even with installs in it"
        );

        let drifting = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(
                2,
                "Beta",
                Some(vec![plugin("aaa", "Media Bar", "2.4.0.0", "Active")]),
            ),
        ];
        let view = build_inventory_view("ui".into(), &drifting, &catalogue, String::new());
        assert!(
            view.groups[0].expanded,
            "a version disagreement opens itself so it is not missed"
        );
    }

    /// Install is only offered while some readable server still lacks it, so the button disappears
    /// once every target has it rather than becoming a no-op the user keeps pressing.
    #[test]
    fn install_is_offered_only_while_a_server_still_lacks_it() {
        let everywhere = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(
                2,
                "Beta",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
        ];
        let view = build_inventory_view(
            "ui".into(),
            &everywhere,
            &[available("aaa", "Media Bar", "IAmParadox")],
            String::new(),
        );
        let row = &view.groups[0].rows[0];
        assert!(!row.missing_anywhere);
        assert_eq!(row.state_summary, "Alpha 2.4.12.0 · Beta 2.4.12.0");

        let partial = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(2, "Beta", Some(Vec::new())),
        ];
        let view = build_inventory_view(
            "ui".into(),
            &partial,
            &[available("aaa", "Media Bar", "IAmParadox")],
            String::new(),
        );
        assert!(view.groups[0].rows[0].missing_anywhere);
    }

    /// An unreadable server must not make Install appear: it may already have the plugin, and the
    /// page would be inviting an action against an unknown.
    #[test]
    fn an_unreadable_server_does_not_count_as_missing() {
        let inventories = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(2, "Beta", None),
        ];

        let view = build_inventory_view(
            "ui".into(),
            &inventories,
            &[available("aaa", "Media Bar", "IAmParadox")],
            String::new(),
        );

        assert!(
            !view.groups[0].rows[0].missing_anywhere,
            "an unreadable server is unknown, not known-missing"
        );
        assert_eq!(view.unreadable_count, 1);
    }

    /// The restart call-out is scoped to servers with something actually pending, so it never
    /// suggests interrupting playback for no reason.
    #[test]
    fn restart_is_offered_only_when_a_plugin_is_actually_staged() {
        let settled = vec![inv(
            1,
            "Alpha",
            Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
        )];
        assert!(!build_inventory_view("ui".into(), &settled, &[], String::new()).restart_needed);

        let staged = vec![
            inv(
                1,
                "Alpha",
                Some(vec![plugin("aaa", "Media Bar", "2.4.12.0", "Active")]),
            ),
            inv(
                2,
                "Beta",
                Some(vec![plugin("bbb", "JellyTag", "2.0.2.0", "Restart")]),
            ),
        ];
        let view = build_inventory_view("ui".into(), &staged, &[], String::new());
        assert!(view.restart_needed);
        assert_eq!(view.restart_count, 1, "only the server with pending work");
    }

    /// An installed plugin no longer offered by any repository still has to appear, or it would
    /// vanish from the page while remaining on the server.
    #[test]
    fn installed_plugins_absent_from_every_catalogue_still_appear() {
        let inventories = vec![inv(
            1,
            "Alpha",
            Some(vec![plugin("orphan", "Retired Plugin", "1.0.0", "Active")]),
        )];

        let view = build_inventory_view("ui".into(), &inventories, &[], String::new());

        let group = view
            .groups
            .iter()
            .find(|g| g.name == INSTALLED_ONLY_GROUP)
            .expect("orphaned installs get their own group");
        assert_eq!(group.rows[0].name, "Retired Plugin");
    }
}
