//! Deciding whose Custom Tabs a browser sees.
//!
//! Custom Tabs stores its tabs on each media server, but a browser runs one application against one
//! origin, so the proxy has to answer `/CustomTabs/config` with a single list. Relaying it to the
//! pinned client host — the behaviour this replaces — means tabs configured on any other server are
//! invisible, with nothing logged to say so.
//!
//! Three policies are useful and they are genuinely different, so this is configuration rather than
//! a guess:
//!
//! * **Pinned** keeps the previous behaviour, and is the default so an upgrade changes nothing.
//! * **Merged** asks every server and concatenates, which is what an operator who already has tabs
//!   spread across servers wants — no re-entering them anywhere.
//! * **Proxy** serves only tabs defined here. Because the proxy answers the endpoint itself, such a
//!   tab exists for people arriving through the proxy and for nobody connecting to a server
//!   directly — which is the only way to get a proxy-only tab without standing up a server that
//!   exists purely to hold it.
//!
//! The plugin still has to be installed on the pinned host in every mode: the client-side code that
//! calls this endpoint is patched into the web bundle by File Transformation, and that bundle comes
//! from the pinned host. What changes here is only *which tabs* that code is told about.

use std::collections::HashSet;

use axum::{
    extract::{Request, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::{
    config::{CustomTabsMode, ProxyCustomTab},
    handlers::web_client::{configured_host, relay_to_client_host},
    AppState,
};

/// One tab, in the shape the injected client script reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomTab {
    #[serde(rename = "Title")]
    pub title: String,
    #[serde(rename = "ContentHtml", default)]
    pub content_html: String,
}

impl From<ProxyCustomTab> for CustomTab {
    fn from(tab: ProxyCustomTab) -> Self {
        CustomTab {
            title: tab.title,
            content_html: tab.content_html,
        }
    }
}

/// Concatenates tab lists, keeping the first occurrence of each title.
///
/// Servers are consulted in priority order, so "first wins" means the higher-priority server's
/// version of a duplicated tab survives. Titles are compared case- and whitespace-insensitively
/// because the same tab configured by hand on two servers rarely matches byte for byte, and showing
/// it twice looks like a bug in the proxy.
pub fn dedupe_by_title(lists: Vec<Vec<CustomTab>>) -> Vec<CustomTab> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged = Vec::new();

    for list in lists {
        for tab in list {
            if seen.insert(tab.title.trim().to_lowercase()) {
                merged.push(tab);
            }
        }
    }

    merged
}

/// Asks every configured server for its tabs, skipping any that cannot answer.
///
/// A server being down must cost its tabs, not the whole tab bar — the same reasoning as everywhere
/// else the proxy fans out.
async fn tabs_from_all_servers(state: &AppState) -> Vec<Vec<CustomTab>> {
    let servers = match state.server_storage.list_servers().await {
        Ok(servers) => servers,
        Err(error) => {
            warn!("Could not list servers for Custom Tabs merge: {error}");
            return Vec::new();
        }
    };

    let leg_timeout =
        std::time::Duration::from_secs(state.config.read().await.federated_leg_timeout);

    let mut legs = JoinSet::new();
    for (index, server) in servers.into_iter().enumerate() {
        let client = state.reqwest_client.clone();
        let url = format!("{}/CustomTabs/config", server.url.as_str().trim_end_matches('/'));
        let name = server.name.clone();
        legs.spawn(async move {
            let request = client.get(&url).send();
            let result = match tokio::time::timeout(leg_timeout, request).await {
                Ok(Ok(response)) if response.status().is_success() => {
                    response.json::<Vec<CustomTab>>().await.ok()
                }
                Ok(Ok(response)) => {
                    debug!("{name} answered {} for its custom tabs", response.status());
                    None
                }
                Ok(Err(error)) => {
                    debug!("{name} did not answer for its custom tabs: {error}");
                    None
                }
                Err(_) => {
                    warn!("{name} timed out returning its custom tabs");
                    None
                }
            };
            (index, result.unwrap_or_default())
        });
    }

    // Restore priority order, which JoinSet completion order does not preserve — otherwise which
    // server wins a duplicated title would depend on which happened to answer first.
    let mut collected: Vec<(usize, Vec<CustomTab>)> = Vec::new();
    while let Some(joined) = legs.join_next().await {
        if let Ok(leg) = joined {
            collected.push(leg);
        }
    }
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, tabs)| tabs).collect()
}

/// `GET /CustomTabs/config`
pub async fn custom_tabs_config(State(state): State<AppState>, req: Request) -> Response {
    let (mode, proxy_tabs) = {
        let config = state.config.read().await;
        (
            config.plugin_federation.custom_tabs_mode,
            config.plugin_federation.custom_tabs.clone(),
        )
    };
    let proxy_tabs: Vec<CustomTab> = proxy_tabs.into_iter().map(CustomTab::from).collect();

    match mode {
        CustomTabsMode::Pinned => {
            let Some(host) = configured_host(&state).await else {
                // No pinned host and no proxy tabs: an empty list is the honest answer, and is what
                // the client script expects when nothing is configured.
                return Json(proxy_tabs).into_response();
            };
            match relay_to_client_host(&state, &host, req).await {
                Ok(response) => response.into_response(),
                Err(status) => {
                    warn!("Pinned host {host} did not answer for custom tabs");
                    status.into_response()
                }
            }
        }
        CustomTabsMode::Proxy => {
            debug!("Serving {} proxy-defined custom tabs", proxy_tabs.len());
            Json(proxy_tabs).into_response()
        }
        CustomTabsMode::Merged => {
            let mut lists = tabs_from_all_servers(&state).await;
            lists.push(proxy_tabs);
            let merged = dedupe_by_title(lists);
            debug!("Merged {} custom tabs across servers", merged.len());
            Json(merged).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(title: &str, html: &str) -> CustomTab {
        CustomTab {
            title: title.to_string(),
            content_html: html.to_string(),
        }
    }

    /// Servers arrive in priority order, so the higher-priority copy of a duplicated tab has to be
    /// the one kept — otherwise which version a user sees depends on network timing.
    #[test]
    fn the_first_server_wins_a_duplicated_title() {
        let merged = dedupe_by_title(vec![
            vec![tab("Requests", "<b>alpha</b>")],
            vec![tab("Requests", "<b>beta</b>")],
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content_html, "<b>alpha</b>");
    }

    /// The same tab configured by hand on two servers rarely matches byte for byte, and rendering
    /// it twice reads as a proxy bug.
    #[test]
    fn titles_are_compared_ignoring_case_and_surrounding_space() {
        let merged = dedupe_by_title(vec![
            vec![tab("Requests", "a")],
            vec![tab("  requests  ", "b")],
            vec![tab("REQUESTS", "c")],
        ]);

        assert_eq!(merged.len(), 1, "one tab, however it was typed");
    }

    /// Merging must not lose tabs that are genuinely different.
    #[test]
    fn distinct_tabs_from_several_servers_are_all_kept() {
        let merged = dedupe_by_title(vec![
            vec![tab("Sweep Alpha", "a")],
            vec![tab("Sweep Beta", "b")],
            vec![tab("Proxy Only", "p")],
        ]);

        let titles: Vec<&str> = merged.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Sweep Alpha", "Sweep Beta", "Proxy Only"]);
    }

    /// A server that is down contributes nothing rather than emptying the bar.
    #[test]
    fn an_empty_contribution_does_not_remove_other_tabs() {
        let merged = dedupe_by_title(vec![vec![], vec![tab("Requests", "a")], vec![]]);
        assert_eq!(merged.len(), 1);
    }

    /// Proxy tabs are appended last, so a server-defined tab of the same name takes precedence —
    /// the proxy adds to what exists rather than quietly overriding it.
    #[test]
    fn a_server_tab_takes_precedence_over_a_proxy_tab_of_the_same_name() {
        let merged = dedupe_by_title(vec![
            vec![tab("Requests", "from-server")],
            vec![tab("Requests", "from-proxy")],
        ]);

        assert_eq!(merged[0].content_html, "from-server");
    }

    /// The wire shape is Jellyfin's, not Rust's: the injected script reads `Title` and
    /// `ContentHtml`, so the casing is load-bearing.
    #[test]
    fn tabs_serialize_in_the_shape_the_client_script_reads() {
        let json = serde_json::to_value(vec![tab("Requests", "<p>hi</p>")]).unwrap();
        assert_eq!(json[0]["Title"], "Requests");
        assert_eq!(json[0]["ContentHtml"], "<p>hi</p>");
    }

    /// And it has to round-trip what a real server returned, which is where merged tabs come from.
    #[test]
    fn a_servers_response_parses_into_tabs() {
        let body = r#"[{"ContentHtml":"<h1>Sweep Alpha</h1>","Title":"Sweep Alpha"}]"#;
        let parsed: Vec<CustomTab> = serde_json::from_str(body).unwrap();

        assert_eq!(parsed, vec![tab("Sweep Alpha", "<h1>Sweep Alpha</h1>")]);
    }

    /// A proxy-defined tab entered with no content is still a tab; it must not fail to parse.
    #[test]
    fn a_tab_without_content_is_accepted() {
        let parsed: Vec<CustomTab> = serde_json::from_str(r#"[{"Title":"Empty"}]"#).unwrap();
        assert_eq!(parsed[0].content_html, "");
    }
}
