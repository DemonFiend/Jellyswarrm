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
use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::{
    config::{CustomTabsMode, ProxyCustomTab},
    handlers::plugin_data::one_session_per_server,
    handlers::web_client::{configured_host, relay_to_client_host},
    request_preprocessing::resolve_request_identity_from_headers_uri,
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    AppState,
};

/// Every spelling of the tab-list path that must reach this handler.
///
/// The plugin's controller is `[Route("[controller]")]` with `[HttpGet("Config")]`, which ASP.NET
/// matches case-insensitively, and `addCustomTabs.js` asks for `CustomTabs/Config`. Axum matches
/// byte for byte. Registering only the lowercase spelling therefore left this handler unreachable:
/// the request fell through to the catch-all, was claimed by the asset-prefix relay and sent
/// verbatim to the pinned host - so the mode setting had no effect in any mode, because the code
/// that reads it never ran. Both spellings are registered because either is a legitimate way to
/// ask a Jellyfin server this question, and the proxy has to answer whatever the plugin's own
/// routing would have.
pub const CONFIG_PATHS: [&str; 2] = ["/CustomTabs/Config", "/CustomTabs/config"];

/// The path the plugin's client script requests, exactly as `addCustomTabs.js` builds it.
#[cfg(test)]
const PATH_THE_CLIENT_SCRIPT_ASKS_FOR: &str = "/CustomTabs/Config";

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

/// Normalises a server or host URL for comparison. Only the trailing slash differs in practice,
/// and it differs often enough to matter: the pinned host is typed by hand on the settings page.
fn same_host(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The caller's session on each server, one per server, in server priority order.
///
/// Priority order is what makes a duplicated title resolve the same way every time, and
/// [`one_session_per_server`] preserves it while dropping the extra sessions a user accumulates
/// per device.
///
/// Takes the headers and URI rather than the request, because `Body` is `Send` but not `Sync`:
/// holding a `&Request` across an await makes this future non-`Send`, which axum reports as the
/// handler not being a handler at all.
async fn caller_sessions(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Vec<(AuthorizationSession, Server)> {
    let identity = match resolve_request_identity_from_headers_uri(headers, uri, state).await {
        Ok(identity) => identity,
        Err(error) => {
            warn!("Could not identify the caller asking for custom tabs: {error}");
            return Vec::new();
        }
    };

    let Some(user) = identity.user else {
        // The plugin's own script only asks once a user is signed in, so this is a guest or a
        // stale token; either way there is nobody whose tabs to fetch.
        debug!("Custom tabs asked for by an unauthenticated caller");
        return Vec::new();
    };

    match state
        .user_authorization
        .get_user_sessions(&user.id, None)
        .await
    {
        Ok(sessions) => one_session_per_server(&sessions),
        Err(error) => {
            warn!("Could not list the caller's sessions for custom tabs: {error}");
            Vec::new()
        }
    }
}

/// Asks the given servers for their tabs, skipping any that cannot answer.
///
/// Each leg carries the token the proxy holds for this user **on that server**. The plugin's
/// controller is `[Authorize]`, so an unauthenticated ask is answered 401 and the server
/// contributes nothing - and relaying the caller's own token is no better, because that token is
/// the proxy's own and the upstream has never seen it. Swapping it per leg is what ordinary
/// request processing does for every other authenticated call; this route only has to do it by
/// hand because it is answered here rather than forwarded.
///
/// A server being down must cost its tabs, not the whole tab bar - the same reasoning as
/// everywhere else the proxy fans out.
async fn tabs_from(
    state: &AppState,
    legs: &[(AuthorizationSession, Server)],
    leg_timeout: Duration,
) -> Vec<Vec<CustomTab>> {
    let mut set = JoinSet::new();
    for (index, (session, server)) in legs.iter().enumerate() {
        let client = state.reqwest_client.clone();
        let url = format!(
            "{}/CustomTabs/Config",
            server.url.as_str().trim_end_matches('/')
        );
        let name = server.name.clone();
        let token = session.jellyfin_token.clone();
        set.spawn(async move {
            let request = client.get(&url).header("X-Emby-Token", &token).send();
            let result = match tokio::time::timeout(leg_timeout, request).await {
                Ok(Ok(response)) if response.status().is_success() => {
                    match response.json::<Vec<CustomTab>>().await {
                        Ok(tabs) => Some(tabs),
                        Err(error) => {
                            debug!("{name} returned custom tabs that did not parse: {error}");
                            None
                        }
                    }
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

    // Restore priority order, which JoinSet completion order does not preserve - otherwise which
    // server wins a duplicated title would depend on which happened to answer first.
    let mut collected: Vec<(usize, Vec<CustomTab>)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(leg) = joined {
            collected.push(leg);
        }
    }
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, tabs)| tabs).collect()
}

/// `GET /CustomTabs/Config` - see [`CONFIG_PATHS`] for why both spellings arrive here.
///
/// All three modes are the same question asked of a different set of servers: the pinned one, all
/// of them, or none. Writing it that way keeps the authentication in one place, which is where it
/// was missing.
pub async fn custom_tabs_config(State(state): State<AppState>, req: Request) -> Response {
    let (mode, proxy_tabs, leg_timeout) = {
        let config = state.config.read().await;
        (
            config.plugin_federation.custom_tabs_mode,
            config.plugin_federation.custom_tabs.clone(),
            Duration::from_secs(config.federated_leg_timeout),
        )
    };
    let proxy_tabs: Vec<CustomTab> = proxy_tabs.into_iter().map(CustomTab::from).collect();
    let headers = req.headers().clone();
    let uri = req.uri().clone();

    if mode == CustomTabsMode::Proxy {
        debug!("Serving {} proxy-defined custom tabs", proxy_tabs.len());
        return Json(proxy_tabs).into_response();
    }

    let sessions = caller_sessions(&state, &headers, &uri).await;

    let legs: Vec<(AuthorizationSession, Server)> = match mode {
        CustomTabsMode::Merged => sessions,
        CustomTabsMode::Pinned => {
            let Some(host) = configured_host(&state).await else {
                // Nothing is pinned, so no server's tabs are the authoritative ones. An empty list
                // is the honest answer, and is what the client script expects when nothing is
                // configured.
                return Json(proxy_tabs).into_response();
            };
            match sessions
                .into_iter()
                .find(|(_, server)| same_host(server.url.as_str(), &host))
            {
                Some(leg) => vec![leg],
                None => {
                    // The client host is pinned to something that is not one of the configured
                    // servers, or the caller has no session on it yet. There is no token to swap
                    // in, so relaying verbatim is the only thing left to try - and it is what this
                    // route did before, so an unusual deployment is no worse off than it was.
                    warn!(
                        "Pinned host {host} is not a server this caller has a session on; \
                         relaying the custom tabs request as-is"
                    );
                    return match relay_to_client_host(&state, &host, req).await {
                        Ok(response) => response.into_response(),
                        Err(status) => {
                            warn!("Pinned host {host} did not answer for custom tabs");
                            status.into_response()
                        }
                    };
                }
            }
        }
        CustomTabsMode::Proxy => unreachable!("returned above"),
    };

    let asked = legs.len();
    let mut lists = tabs_from(&state, &legs, leg_timeout).await;
    lists.push(proxy_tabs);
    let merged = dedupe_by_title(lists);
    debug!(
        "Serving {} custom tabs from {asked} server(s) in {mode:?} mode",
        merged.len()
    );
    Json(merged).into_response()
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

    /// The bug that made every mode look broken. The injected script asks for `CustomTabs/Config`
    /// and axum matches paths byte for byte, so a handler registered only at the lowercase
    /// spelling was never reached: the request fell through to the asset relay and went verbatim
    /// to the pinned host, whatever the configured mode said.
    #[test]
    fn the_path_the_client_script_asks_for_is_one_we_answer() {
        assert!(
            CONFIG_PATHS.contains(&PATH_THE_CLIENT_SCRIPT_ASKS_FOR),
            "the plugin asks for {PATH_THE_CLIENT_SCRIPT_ASKS_FOR}, which none of {CONFIG_PATHS:?} \
             matches - the handler is unreachable and the mode setting does nothing"
        );
    }

    /// ASP.NET serves the controller case-insensitively, so both spellings are requests a real
    /// server would answer, and the proxy has to answer either.
    #[test]
    fn both_spellings_of_the_tab_list_path_are_registered() {
        assert!(CONFIG_PATHS.contains(&"/CustomTabs/Config"));
        assert!(CONFIG_PATHS.contains(&"/CustomTabs/config"));
    }

    /// The pinned host is typed by hand on the settings page, so it routinely differs from the
    /// stored server URL by a trailing slash. Failing to match it there costs the caller their
    /// token and drops the request onto the verbatim relay, which is the path that 401s.
    #[test]
    fn a_trailing_slash_does_not_stop_the_pinned_host_matching_its_server() {
        assert!(same_host(
            "https://stream.example.com/",
            "https://stream.example.com"
        ));
        assert!(!same_host(
            "https://stream.example.com",
            "https://other.example.com"
        ));
    }
}

/// The fan-out itself, against servers that insist on being asked properly.
#[cfg(test)]
mod leg_tests {
    use super::*;
    use crate::{
        config::{AppConfig, MIGRATOR},
        handlers::quick_connect::QuickConnectStorage,
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::{Device, UserAuthorizationService},
        virtual_library_service::VirtualLibraryService,
        DataContext, ProxyProcessors,
    };
    use std::sync::Arc;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn state() -> AppState {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool.clone(),
                server_storage,
                media_storage,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let processors = ProxyProcessors::new(context.clone());
        AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            context,
            processors,
            QuickConnectStorage::new(),
        )
    }

    fn leg(url: &str, name: &str, token: &str) -> (AuthorizationSession, Server) {
        use crate::{config::MediaStreamingMode, server_id::ServerId, server_url::ServerUrl};

        let now = chrono::Utc::now();
        let server = Server {
            id: ServerId::new(1),
            name: name.to_string(),
            url: ServerUrl::parse(url).unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Proxy,
            created_at: now,
            updated_at: now,
        };
        let session = AuthorizationSession {
            id: 1,
            user_id: "viewer".into(),
            mapping_id: 1,
            server_url: url.to_string(),
            device: Device {
                client: "test".into(),
                device: "test".into(),
                device_id: "test".into(),
                version: "1".into(),
            },
            jellyfin_token: token.to_string(),
            original_user_id: "backend-user".into(),
            expires_at: None,
            created_at: now,
            updated_at: now,
        };
        (session, server)
    }

    /// The plugin's controller is `[Authorize]`, so a leg that carries no token is answered 401 and
    /// the server contributes nothing. The caller's own token is no better: it is the proxy's, and
    /// the upstream has never seen it. The mock here only answers a request bearing the token the
    /// proxy holds for this user on this server, so an unauthenticated leg reads as a server with
    /// no tabs - which is exactly how this failed in the field, silently and with an empty tab bar.
    #[tokio::test]
    async fn a_leg_carries_the_token_the_proxy_holds_for_that_server() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/CustomTabs/Config"))
            .and(header("X-Emby-Token", "upstream-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "Title": "Requests", "ContentHtml": "<b>from the server</b>" }
            ])))
            .mount(&upstream)
            .await;

        let state = state().await;
        let legs = vec![leg(&upstream.uri(), "Sweep Alpha", "upstream-token")];
        let lists = tabs_from(&state, &legs, Duration::from_secs(5)).await;

        assert_eq!(
            lists,
            vec![vec![tab("Requests", "<b>from the server</b>")]],
            "the server's tabs must come back, which means the leg was authenticated"
        );
    }

    /// And the path has to be the one the plugin's controller serves. Asking for the wrong spelling
    /// is indistinguishable from a server with no tabs configured.
    #[tokio::test]
    async fn a_leg_asks_for_the_path_the_controller_serves() {
        let upstream = MockServer::start().await;
        // Answers only the exact path the plugin registers; anything else 404s.
        Mock::given(method("GET"))
            .and(path("/CustomTabs/Config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "Title": "Requests", "ContentHtml": "x" }
            ])))
            .mount(&upstream)
            .await;

        let state = state().await;
        let legs = vec![leg(&upstream.uri(), "Sweep Alpha", "t")];
        let lists = tabs_from(&state, &legs, Duration::from_secs(5)).await;

        assert_eq!(
            lists[0].len(),
            1,
            "the leg asked for a path the server serves"
        );
    }

    /// A server that refuses contributes nothing rather than failing the whole tab bar.
    #[tokio::test]
    async fn a_server_that_rejects_the_leg_costs_only_its_own_tabs() {
        let refusing = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/CustomTabs/Config"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&refusing)
            .await;

        let giving = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/CustomTabs/Config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "Title": "Kept", "ContentHtml": "y" }
            ])))
            .mount(&giving)
            .await;

        let state = state().await;
        let legs = vec![
            leg(&refusing.uri(), "Refusing", "t"),
            leg(&giving.uri(), "Giving", "t"),
        ];
        let merged = dedupe_by_title(tabs_from(&state, &legs, Duration::from_secs(5)).await);

        assert_eq!(merged, vec![tab("Kept", "y")]);
    }

    fn tab(title: &str, html: &str) -> CustomTab {
        CustomTab {
            title: title.to_string(),
            content_html: html.to_string(),
        }
    }
}
