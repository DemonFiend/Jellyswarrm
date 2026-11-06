//! Reading and managing each media server's Jellyfin plugin inventory.
//!
//! Jellyfin plugins are .NET assemblies loaded by a Jellyfin server, so the proxy can never host
//! one itself. What it can do is act as a control panel for the plugin systems that already exist:
//! every plugin endpoint requires administrator rights, and the proxy already stores an admin
//! credential per server for user federation.
//!
//! Two things this exists to make possible, beyond saving a round of clicking:
//!
//! * **Version drift is visible.** The same plugin at different versions on two servers serves two
//!   different clients, which is a silent and confusing failure. Comparing inventories surfaces it.
//! * **Absence becomes a recorded decision.** A plugin route answering 404 is ambiguous — never
//!   installed, or installed and broken? Knowing which servers were deliberately chosen turns that
//!   into a fact instead of a guess, so the proxy can stay quiet about the former and complain
//!   about the latter.

use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};

use crate::{
    encryption::{decrypt_password, HashedPassword},
    server_id::ServerId,
    AppState,
};

/// A plugin installed on one server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstalledPlugin {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version", default)]
    pub version: String,
    #[serde(rename = "Status", default)]
    pub status: String,
}

/// A plugin offered by one of a server's configured repositories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailablePlugin {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "guid", default)]
    pub guid: String,
    #[serde(rename = "description", default)]
    pub description: String,
    #[serde(rename = "versions", default)]
    pub versions: Vec<AvailableVersion>,
    /// Which repository offered this plugin. Populated by the proxy, not by Jellyfin: the catalogue
    /// response flattens every repository together, so the grouping has to be reconstructed from
    /// each version entry's source URL.
    #[serde(skip)]
    pub repository: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableVersion {
    #[serde(rename = "version", default)]
    pub version: String,
    #[serde(rename = "repositoryName", default)]
    pub repository_name: String,
    #[serde(rename = "repositoryUrl", default)]
    pub repository_url: String,
}

/// A repository a server fetches plugin manifests from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginRepository {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Url")]
    pub url: String,
    #[serde(rename = "Enabled", default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// What the proxy knows about one server's plugin state.
#[derive(Debug, Clone)]
pub struct ServerPluginInventory {
    pub server_id: ServerId,
    pub server_name: String,
    /// `None` means the inventory could not be read at all — no admin credential, the server is
    /// unreachable, or authentication failed. Deliberately distinct from an empty list, which means
    /// "asked successfully, nothing installed": conflating the two would report a server that is
    /// merely unreachable as one with no plugins, and quietly invite a duplicate install.
    pub plugins: Option<Vec<InstalledPlugin>>,
    pub repositories: Vec<PluginRepository>,
    pub error: Option<String>,
}

impl ServerPluginInventory {
    pub fn is_readable(&self) -> bool {
        self.plugins.is_some()
    }

    /// Whether a plugin with this id is installed, matched case-insensitively on the GUID.
    pub fn has_plugin(&self, plugin_id: &str) -> bool {
        self.plugins.as_ref().is_some_and(|plugins| {
            plugins
                .iter()
                .any(|p| normalize_guid(&p.id) == normalize_guid(plugin_id))
        })
    }

    pub fn installed_version(&self, plugin_id: &str) -> Option<&str> {
        self.plugins.as_ref()?.iter().find_map(|p| {
            (normalize_guid(&p.id) == normalize_guid(plugin_id)).then_some(p.version.as_str())
        })
    }
}

/// Jellyfin reports plugin ids both dashed and undashed depending on the endpoint, so compare on a
/// canonical form rather than the raw string.
pub fn normalize_guid(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// An authenticated admin session against one server's HTTP API.
struct AdminSession {
    base_url: String,
    token: String,
}

impl AdminSession {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

/// Authenticates as the stored admin for a server.
async fn admin_session(state: &AppState, server_id: ServerId) -> Result<AdminSession, String> {
    let server = state
        .server_storage
        .get_server_by_id(server_id)
        .await
        .map_err(|e| format!("failed to load server: {e}"))?
        .ok_or_else(|| "server not found".to_string())?;

    let admin = state
        .server_storage
        .get_server_admin(server_id)
        .await
        .map_err(|e| format!("failed to load admin credential: {e}"))?
        .ok_or_else(|| {
            "no admin credential stored for this server - add one on the Servers page".to_string()
        })?;

    let proxy_password: HashedPassword = state.config.read().await.password.clone().into();
    let password = decrypt_password(&admin.password, &proxy_password)
        .map_err(|e| format!("failed to decrypt admin password: {e}"))?;

    let base_url = server.url.as_str().trim_end_matches('/').to_string();
    let auth_header = format!(
        "MediaBrowser Client=\"Jellyswarrm\", Device=\"Proxy\", DeviceId=\"jellyswarrm-plugin-admin\", Version=\"{}\"",
        env!("CARGO_PKG_VERSION")
    );

    let response = state
        .reqwest_client
        .post(format!("{base_url}/Users/AuthenticateByName"))
        .header("X-Emby-Authorization", &auth_header)
        .json(&serde_json::json!({
            "Username": admin.username,
            "Pw": password.as_str(),
        }))
        .send()
        .await
        .map_err(|e| format!("could not reach server: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "admin authentication rejected with status {}",
            response.status()
        ));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("unreadable authentication response: {e}"))?;

    let token = body
        .get("AccessToken")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "authentication response carried no access token".to_string())?
        .to_string();

    Ok(AdminSession { base_url, token })
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    state: &AppState,
    session: &AdminSession,
    path: &str,
) -> Result<T, String> {
    let response = state
        .reqwest_client
        .get(session.url(path))
        .header("X-Emby-Token", &session.token)
        .send()
        .await
        .map_err(|e| format!("request to {path} failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("{path} answered {}", response.status()));
    }

    response
        .json::<T>()
        .await
        .map_err(|e| format!("could not parse {path}: {e}"))
}

/// Reads one server's plugin inventory. Never fails the caller — an unreachable or
/// credential-less server is reported as such so the page can say *why* it is blank.
pub async fn read_inventory(
    state: &AppState,
    server_id: ServerId,
    server_name: String,
) -> ServerPluginInventory {
    let session = match admin_session(state, server_id).await {
        Ok(session) => session,
        Err(e) => {
            debug!("No plugin inventory for server {server_name}: {e}");
            return ServerPluginInventory {
                server_id,
                server_name,
                plugins: None,
                repositories: Vec::new(),
                error: Some(e),
            };
        }
    };

    let plugins = match get_json::<Vec<InstalledPlugin>>(state, &session, "/Plugins").await {
        Ok(plugins) => plugins,
        Err(e) => {
            warn!("Failed to read plugins from {server_name}: {e}");
            return ServerPluginInventory {
                server_id,
                server_name,
                plugins: None,
                repositories: Vec::new(),
                error: Some(e),
            };
        }
    };

    let repositories = get_json::<Vec<PluginRepository>>(state, &session, "/Repositories")
        .await
        .unwrap_or_else(|e| {
            warn!("Failed to read repositories from {server_name}: {e}");
            Vec::new()
        });

    ServerPluginInventory {
        server_id,
        server_name,
        plugins: Some(plugins),
        repositories,
        error: None,
    }
}

/// Plugins offered by a server's repositories, used to populate the catalogue.
pub async fn read_catalogue(
    state: &AppState,
    server_id: ServerId,
) -> Result<Vec<AvailablePlugin>, String> {
    let session = admin_session(state, server_id).await?;
    let mut catalogue = get_json::<Vec<AvailablePlugin>>(state, &session, "/Packages").await?;

    for plugin in &mut catalogue {
        plugin.repository = plugin
            .versions
            .iter()
            .map(|v| v.repository_name.trim())
            .find(|name| !name.is_empty())
            .unwrap_or("Other")
            .to_string();
    }

    Ok(catalogue)
}

/// Asks a server to restart itself.
///
/// Jellyfin only loads plugins at start-up, so an install is inert until this happens. Exposing it
/// here keeps the whole flow inside the admin page rather than requiring shell access to the host
/// running the container.
pub async fn restart_server(state: &AppState, server_id: ServerId) -> Result<(), String> {
    let session = admin_session(state, server_id).await?;

    let response = state
        .reqwest_client
        .post(session.url("/System/Restart"))
        .header("X-Emby-Token", &session.token)
        .send()
        .await
        .map_err(|e| format!("restart request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("server answered {}", response.status()));
    }

    Ok(())
}

/// Whether any plugin on this server is staged but not yet loaded.
pub fn awaiting_restart(inventory: &ServerPluginInventory) -> usize {
    inventory
        .plugins
        .as_ref()
        .map(|plugins| {
            plugins
                .iter()
                .filter(|p| p.status.eq_ignore_ascii_case("Restart"))
                .count()
        })
        .unwrap_or(0)
}

/// Adds a repository to a server, preserving the ones already configured.
///
/// Jellyfin's endpoint replaces the whole list rather than appending, so the existing entries are
/// read first — posting only the new one would silently delete every other repository.
pub async fn add_repository(
    state: &AppState,
    server_id: ServerId,
    repository: PluginRepository,
) -> Result<(), String> {
    let session = admin_session(state, server_id).await?;

    let mut repositories = get_json::<Vec<PluginRepository>>(state, &session, "/Repositories")
        .await
        .unwrap_or_default();

    if repositories
        .iter()
        .any(|r| r.url.eq_ignore_ascii_case(&repository.url))
    {
        debug!(
            "Repository {} already present, nothing to do",
            repository.url
        );
        return Ok(());
    }

    repositories.push(repository);

    let response = state
        .reqwest_client
        .post(session.url("/Repositories"))
        .header("X-Emby-Token", &session.token)
        .json(&repositories)
        .send()
        .await
        .map_err(|e| format!("failed to update repositories: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "server rejected the repository list with status {}",
            response.status()
        ));
    }

    Ok(())
}

/// Outcome of asking one server to install a plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallOutcome {
    /// The server was asked to install and accepted.
    Installing,
    /// Already present at this version, so nothing was sent.
    AlreadyInstalled { version: String },
    /// The server could not be asked.
    Failed { reason: String },
}

/// Installs a plugin on one server, skipping the request entirely if it is already there.
///
/// The pre-check is what makes repeating an install a no-op: without it a second run re-downloads
/// and re-stages the plugin, and the operator cannot tell from the result whether anything changed.
pub async fn install_plugin(
    state: &AppState,
    server_id: ServerId,
    plugin_name: &str,
    plugin_guid: &str,
    version: Option<&str>,
) -> InstallOutcome {
    let session = match admin_session(state, server_id).await {
        Ok(session) => session,
        Err(reason) => return InstallOutcome::Failed { reason },
    };

    match get_json::<Vec<InstalledPlugin>>(state, &session, "/Plugins").await {
        Ok(installed) => {
            if let Some(existing) = installed
                .iter()
                .find(|p| normalize_guid(&p.id) == normalize_guid(plugin_guid))
            {
                return InstallOutcome::AlreadyInstalled {
                    version: existing.version.clone(),
                };
            }
        }
        Err(reason) => {
            // Refuse rather than install blind: without knowing the current state we cannot honour
            // the promise that repeating an install changes nothing.
            return InstallOutcome::Failed {
                reason: format!("could not check what is already installed: {reason}"),
            };
        }
    }

    // Build the URL through `url` so the plugin name is percent-encoded correctly; several plugin
    // names contain spaces.
    let mut url = match url::Url::parse(&format!(
        "{}/Packages/Installed/",
        session.base_url.trim_end_matches('/')
    ))
    .and_then(|base| base.join(plugin_name))
    {
        Ok(url) => url,
        Err(e) => {
            return InstallOutcome::Failed {
                reason: format!("could not build install URL for {plugin_name}: {e}"),
            }
        }
    };
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("assemblyGuid", plugin_guid);
        if let Some(version) = version {
            query.append_pair("version", version);
        }
    }

    let response = state
        .reqwest_client
        .post(url.as_str())
        .header("X-Emby-Token", &session.token)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => InstallOutcome::Installing,
        Ok(response) => InstallOutcome::Failed {
            reason: format!("server answered {}", response.status()),
        },
        Err(e) => InstallOutcome::Failed {
            reason: format!("request failed: {e}"),
        },
    }
}

/// Reads every configured server's inventory.
pub async fn read_all_inventories(state: &AppState) -> Vec<ServerPluginInventory> {
    let servers = match state.server_storage.list_servers().await {
        Ok(servers) => servers,
        Err(e) => {
            error!("Failed to list servers for plugin inventory: {e}");
            return Vec::new();
        }
    };

    let mut inventories = Vec::with_capacity(servers.len());
    for server in servers {
        inventories.push(read_inventory(state, server.id, server.name.clone()).await);
    }
    inventories
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(plugins: Option<Vec<InstalledPlugin>>) -> ServerPluginInventory {
        ServerPluginInventory {
            server_id: ServerId::new(1),
            server_name: "Test".to_string(),
            plugins,
            repositories: Vec::new(),
            error: None,
        }
    }

    fn plugin(id: &str, name: &str, version: &str) -> InstalledPlugin {
        InstalledPlugin {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            status: "Active".to_string(),
        }
    }

    /// Jellyfin returns plugin ids in both dashed and undashed forms depending on the endpoint, so
    /// a raw string comparison reports an installed plugin as missing and invites a second install.
    #[test]
    fn plugin_ids_compare_regardless_of_dashes_or_case() {
        let inv = inventory(Some(vec![plugin(
            "5e87cc92-8f8a-4b2f-9c3d-1a2b3c4d5e6f",
            "File Transformation",
            "2.5.11.0",
        )]));

        assert!(inv.has_plugin("5e87cc92-8f8a-4b2f-9c3d-1a2b3c4d5e6f"));
        assert!(inv.has_plugin("5E87CC928F8A4B2F9C3D1A2B3C4D5E6F"));
        assert!(!inv.has_plugin("08f615ea-1111-2222-3333-444444444444"));
    }

    /// "Asked, and nothing is installed" and "could not ask" must not look the same. Treating an
    /// unreachable server as empty would show it as a candidate for installation and hide the real
    /// problem.
    #[test]
    fn an_unreadable_server_is_distinct_from_one_with_no_plugins() {
        let empty = inventory(Some(Vec::new()));
        let unreachable = inventory(None);

        assert!(empty.is_readable());
        assert!(!empty.has_plugin("anything"));

        assert!(!unreachable.is_readable());
        assert!(
            !unreachable.has_plugin("anything"),
            "an unreadable inventory must never claim a plugin is present"
        );
    }

    #[test]
    fn installed_version_is_reported_for_drift_comparison() {
        let inv = inventory(Some(vec![plugin(
            "abc",
            "Home Screen Sections",
            "2.5.11.0",
        )]));

        assert_eq!(inv.installed_version("ABC"), Some("2.5.11.0"));
        assert_eq!(inv.installed_version("other"), None);
    }
}
