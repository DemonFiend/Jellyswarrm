//! Serving the browser's Jellyfin web client, and the plugin surface that rides along with it.
//!
//! A browser runs one single-page application from one origin, so exactly one client build has to
//! serve the whole page. The proxy embeds its own copy of jellyfin-web and serves that by default,
//! which is at least self-consistent but carries no plugins at all: the proxy is not a Jellyfin
//! server, so there is no File Transformation to inject any. Every client-side plugin therefore
//! depends on the client coming from a real server instead.
//!
//! When `web_client_host` is configured, that build comes from the designated origin. The important
//! property is that the origin is **pinned**. Letting `/web/*` fall through to the catch-all proxy
//! handler re-resolves a server on every request from live health state, and unauthenticated asset
//! requests resolve through a different predicate than authenticated API calls. One page load could
//! therefore take `index.html` from one server and a lazily-loaded chunk from another. The two
//! builds have different webpack content hashes, so the chunk 404s and a whole route or home row
//! fails to render, with nothing logged as an error. That is the mechanism behind "it works
//! sometimes".
//!
//! Pinning to a configured origin makes that unrepresentable: every byte of the application comes
//! from the same place, for the whole page load and for every page load.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::Response,
};
use tracing::{debug, error, warn};

use crate::{proxy_headers::is_hop_by_hop_header, AppState};

/// Whether `path` falls under one of `prefixes`, matched on a path-segment boundary.
///
/// The prefixes are configuration rather than constants: they are top-level routes registered by
/// whichever client-side plugins are installed, so the correct lists depend on the deployment. The
/// shipped defaults are in [`crate::config::PluginFederationConfig`], which holds two separate
/// lists this is used against - `plugin_asset_prefixes`, relayed verbatim, and
/// `plugin_api_prefixes`, pinned but authenticated.
///
/// Only asset and configuration surfaces belong in the asset list. Endpoints that return media
/// items are data, not chrome, and federating those across servers is a separate concern - a route
/// added there would be pinned to one server and silently stop merging.
pub fn matches_plugin_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| path.eq_ignore_ascii_case(prefix) || starts_with_segment(path, prefix))
}

/// The pinned client origin to relay `path` to, when `path` is a configured plugin asset surface.
///
/// The host and the policy come from a single config read, so a reload cannot be observed
/// half-applied.
///
/// Consulted from the catch-all rather than from a route registered per prefix. Registering a route
/// per prefix makes the policy a build-time constant: a prefix *added* to the list has no route to
/// reach the handler that reads the list, so only a removal ever takes effect. Deciding it here is
/// what makes the configured list actually govern the behaviour - which is what the Plugins page
/// has always implied, and what lets a plugin surface be added without rebuilding the image.
///
/// Only asset and configuration surfaces belong in this list. An endpoint that returns media items
/// is data, not chrome: pinning it would quietly make that surface single-server, which is the
/// defect this whole module exists to remove.
pub async fn plugin_asset_host(state: &AppState, path: &str) -> Option<String> {
    let config = state.config.read().await;

    let host = config.web_client_host.trim().trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    if !matches_plugin_prefix(path, &config.plugin_federation.plugin_asset_prefixes) {
        return None;
    }

    debug!("Pinning plugin asset {path} to the web client host {host}");
    Some(host.to_string())
}

/// Prefix match on a path-segment boundary, so `/MediaBarSomethingElse` does not match `/MediaBar`.
fn starts_with_segment(path: &str, prefix: &str) -> bool {
    path.len() > prefix.len()
        && path.as_bytes()[prefix.len()] == b'/'
        && path[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The configured client origin, if any, normalised without a trailing slash.
pub async fn configured_host(state: &AppState) -> Option<String> {
    let host = state.config.read().await.web_client_host.clone();
    let host = host.trim().trim_end_matches('/').to_string();
    (!host.is_empty()).then_some(host)
}

/// Relays a request to the pinned client host, preserving method, headers and body.
pub async fn relay_to_client_host(
    state: &AppState,
    host: &str,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let target = format!("{host}{path_and_query}");

    let url = target.parse::<Uri>().map_err(|e| {
        error!("Invalid web client host URL {target}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let method = req.method().clone();
    let mut headers = req.headers().clone();

    // The upstream must see its own host, and must not be asked for an encoding we cannot relay.
    headers.remove(header::HOST);
    headers.remove(header::ACCEPT_ENCODING);
    let hop_by_hop: Vec<_> = headers
        .keys()
        .filter(|name| is_hop_by_hop_header(name))
        .cloned()
        .collect();
    for name in hop_by_hop {
        headers.remove(name);
    }

    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|e| {
            error!("Failed to read request body for {target}: {e}");
            StatusCode::BAD_REQUEST
        })?;

    debug!("Relaying {method} {path_and_query} to pinned web client host {host}");

    let response = state
        .reqwest_client
        .request(method, url.to_string())
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| {
            // A failure here is the whole application failing to load, so say so plainly rather
            // than letting it surface as an opaque 502 with no cause.
            error!("Pinned web client host {host} did not answer: {e}");
            StatusCode::BAD_GATEWAY
        })?;

    let status = response.status();
    let upstream_headers = response.headers().clone();
    let bytes = response.bytes().await.map_err(|e| {
        error!("Failed to read response body from {host}: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_headers.iter() {
        if !is_hop_by_hop_header(name) {
            builder = builder.header(name, value);
        }
    }

    builder.body(Body::from(bytes)).map_err(|e| {
        error!("Failed to build relayed response: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// `GET /web/*` — the client application itself.
///
/// With no host configured this defers to the ordinary request path, which serves the client
/// embedded in the proxy. Registering these routes must not change what an unconfigured deployment
/// does.
///
/// Falls back to the bundled client when a configured host cannot be reached. Without that, a host
/// that is merely restarting takes the entire UI down — including the admin page needed to change
/// this setting — which is a worse failure than losing the plugins for a minute.
pub async fn web_client_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    let Some(host) = configured_host(&state).await else {
        return crate::proxy_handler(State(state), req).await;
    };

    let path = req.uri().path().to_string();

    match relay_to_client_host(&state, &host, req).await {
        Ok(response) => Ok(response),
        Err(_) => {
            warn!(
                "Web client host {host} did not answer for {path}; serving the bundled client. \
                 Plugin UIs will be missing until it returns."
            );
            bundled_asset(&path).ok_or(StatusCode::BAD_GATEWAY)
        }
    }
}

/// Serves a file from the client bundled into the proxy, for use when the pinned host is down.
fn bundled_asset(path: &str) -> Option<Response<Body>> {
    // `/web/foo` maps onto `foo` in the embedded bundle; a bare directory means the app entry point.
    let relative = path.trim_start_matches("/web").trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };

    let asset = crate::Asset::get(relative).or_else(|| crate::Asset::get("index.html"))?;
    let mime = mime_guess::from_path(relative).first_or_octet_stream();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        // Never let a fallback response be cached as though it were the real client.
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(asset.data.into_owned()))
        .ok()
}

/// Redirect target for `/` when a pinned client host is configured.
pub fn web_root_redirect() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/web/"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped defaults, which are what every existing deployment gets.
    fn prefixes() -> Vec<String> {
        crate::config::PluginFederationConfig::default().plugin_asset_prefixes
    }

    #[test]
    fn plugin_asset_paths_match_on_segment_boundaries() {
        let p = prefixes();
        assert!(matches_plugin_prefix("/MediaBar", &p));
        assert!(matches_plugin_prefix("/MediaBar/WebConfig", &p));
        assert!(matches_plugin_prefix("/mediabar/webconfig", &p));
        assert!(matches_plugin_prefix("/PluginPages/inject.js", &p));
        assert!(matches_plugin_prefix("/CustomTabs/Config", &p));
        assert!(matches_plugin_prefix(
            "/HomeScreen/home-screen-sections.js",
            &p
        ));

        // A prefix that merely starts with the same letters is a different route.
        assert!(!matches_plugin_prefix("/MediaBarSomethingElse", &p));
        assert!(!matches_plugin_prefix("/PluginPagesExtra/thing", &p));
    }

    /// The shipped list is what makes the named plugins work with no configuration at all, so it is
    /// worth pinning down rather than leaving to whoever edits the defaults next.
    #[test]
    fn the_default_prefixes_cover_the_shipped_plugin_surfaces() {
        assert_eq!(
            prefixes(),
            vec![
                "/PluginPages",
                "/MediaBar",
                "/CustomTabs",
                "/HomeScreen/home-screen-sections.js",
                "/HomeScreen/home-screen-sections.css",
            ]
        );
    }

    /// An operator removing a prefix has to actually stop it being pinned, or the setting is a lie.
    #[test]
    fn a_removed_prefix_is_no_longer_pinned() {
        let without_mediabar: Vec<String> = prefixes()
            .into_iter()
            .filter(|p| p != "/MediaBar")
            .collect();

        assert!(!matches_plugin_prefix(
            "/MediaBar/WebConfig",
            &without_mediabar
        ));
        assert!(matches_plugin_prefix(
            "/CustomTabs/Config",
            &without_mediabar
        ));
    }

    /// Section *data* must never be pinned here. These endpoints return media items and have to be
    /// merged across servers; routing them to one host would silently make every row single-server,
    /// which is the exact defect this work exists to remove.
    #[test]
    fn section_data_endpoints_are_not_treated_as_assets() {
        let p = prefixes();
        assert!(!matches_plugin_prefix("/HomeScreen/Sections", &p));
        assert!(!matches_plugin_prefix(
            "/HomeScreen/Section/RecentlyAddedMovies",
            &p
        ));
        assert!(!matches_plugin_prefix("/HomeScreen/Meta", &p));
        assert!(!matches_plugin_prefix("/HomeScreen/CachedImage/abc", &p));
    }

    /// A pinned host that is merely restarting must not take the whole UI down. The fallback maps
    /// `/web/...` onto the bundled client, and anything unrecognised onto its entry point so the
    /// single-page app can still boot and route itself.
    #[test]
    fn the_bundled_fallback_maps_web_paths_onto_the_embedded_client() {
        // Only meaningful when a client is actually embedded; skipped on UI-less builds.
        if crate::Asset::get("index.html").is_none() {
            return;
        }

        assert!(bundled_asset("/web/").is_some(), "the app entry point");
        assert!(bundled_asset("/web").is_some(), "no trailing slash");
        assert!(
            bundled_asset("/web/does-not-exist.chunk.js").is_some(),
            "an unknown path still boots the app rather than 502-ing"
        );
    }

    /// The fallback is a degraded response and must never be cached in place of the real client,
    /// or a single blip would persist until the browser cache is cleared.
    #[test]
    fn the_bundled_fallback_is_never_cached() {
        if crate::Asset::get("index.html").is_none() {
            return;
        }

        let response = bundled_asset("/web/").expect("entry point");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    /// The two prefix lists are different mechanisms, and confusing them is the easy mistake. An
    /// asset prefix is relayed verbatim, which sends the *proxy's* own access token, so any
    /// endpoint that identifies the caller sees an unknown user - the reason pinning
    /// `/HomeScreen/Meta` as an asset failed. Enhanced's endpoints do identify the caller, so they
    /// belong in the API list and must stay out of the asset one.
    #[test]
    fn plugin_api_prefixes_are_not_asset_prefixes() {
        let config = crate::config::PluginFederationConfig::default();

        assert_eq!(
            config.plugin_api_prefixes,
            vec!["/JellyfinEnhanced", "/JellyTweaks"]
        );
        for prefix in &config.plugin_api_prefixes {
            assert!(
                !matches_plugin_prefix(prefix, &config.plugin_asset_prefixes),
                "{prefix} must not be relayed as an asset"
            );
        }
    }

    /// The API list is matched by the same segment-boundary rule, so a plugin whose route merely
    /// starts with the same letters is not swept onto the pinned server.
    #[test]
    fn plugin_api_paths_match_on_segment_boundaries() {
        let api = crate::config::PluginFederationConfig::default().plugin_api_prefixes;

        assert!(matches_plugin_prefix(
            "/JellyfinEnhanced/tag-cache/abc",
            &api
        ));
        assert!(matches_plugin_prefix("/jellyfinenhanced/plugin.js", &api));
        assert!(!matches_plugin_prefix("/JellyfinEnhancedExtra/thing", &api));
    }
}

#[cfg(test)]
mod plugin_asset_policy_tests {
    use super::*;
    use crate::config::{AppConfig, MIGRATOR};
    use crate::handlers::quick_connect::QuickConnectStorage;
    use crate::media_storage_service::MediaStorageService;
    use crate::server_storage::ServerStorageService;
    use crate::session_storage::SessionStorage;
    use crate::user_authorization_service::UserAuthorizationService;
    use crate::virtual_library_service::VirtualLibraryService;
    use crate::{DataContext, ProxyProcessors};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn test_state() -> AppState {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool,
                server_storage,
                media_storage,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let processors = ProxyProcessors::new(data_context.clone());

        AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            data_context,
            processors,
            QuickConnectStorage::new(),
        )
    }

    async fn set_host(state: &AppState, host: &str) {
        state.config.write().await.web_client_host = host.to_string();
    }

    async fn set_asset_prefixes(state: &AppState, prefixes: &[&str]) {
        state
            .config
            .write()
            .await
            .plugin_federation
            .plugin_asset_prefixes = prefixes.iter().map(|p| p.to_string()).collect();
    }

    /// The point of deciding this from the catch-all. While each prefix had its own route, a prefix
    /// added to the list had no route to reach the handler that reads the list, so the setting
    /// could only ever *remove* pinning - it described what it could not change. An operator
    /// installing a plugin the shipped defaults have never heard of must be able to pin it from the
    /// Plugins page, without a rebuild.
    #[tokio::test]
    async fn a_prefix_added_at_runtime_is_pinned_without_a_rebuild() {
        let state = test_state().await;
        set_host(&state, "https://client.example").await;

        assert_eq!(
            plugin_asset_host(&state, "/SomeNewPlugin/script.js").await,
            None,
            "not pinned while the policy does not mention it"
        );

        set_asset_prefixes(&state, &["/SomeNewPlugin"]).await;

        assert_eq!(
            plugin_asset_host(&state, "/SomeNewPlugin/script.js").await,
            Some("https://client.example".to_string()),
            "adding the prefix must take effect with no route and no restart"
        );
    }

    /// And the converse, which already worked and must keep working.
    #[tokio::test]
    async fn a_removed_prefix_stops_being_pinned() {
        let state = test_state().await;
        set_host(&state, "https://client.example").await;

        assert!(plugin_asset_host(&state, "/MediaBar/slideshowpure.js")
            .await
            .is_some());

        set_asset_prefixes(&state, &["/PluginPages"]).await;

        assert_eq!(
            plugin_asset_host(&state, "/MediaBar/slideshowpure.js").await,
            None
        );
    }

    /// A deployment that pins no host must behave exactly as it did before any of this existed:
    /// the catch-all handles everything and nothing is relayed anywhere.
    #[tokio::test]
    async fn nothing_is_pinned_without_a_configured_host() {
        let state = test_state().await;

        for path in [
            "/MediaBar/slideshowpure.js",
            "/PluginPages/inject.js",
            "/CustomTabs/anything",
        ] {
            assert_eq!(
                plugin_asset_host(&state, path).await,
                None,
                "{path} must not be pinned when no host is configured"
            );
        }
    }

    /// Whitespace and a trailing slash are what an operator actually types into the field.
    #[tokio::test]
    async fn the_configured_host_is_normalised() {
        let state = test_state().await;
        set_host(&state, "  https://client.example/  ").await;

        assert_eq!(
            plugin_asset_host(&state, "/MediaBar/slideshowpure.js").await,
            Some("https://client.example".to_string())
        );
    }

    /// The guard that matters most. These endpoints return media items and have to be merged
    /// across servers; pinning one sends the whole surface to a single server, and the symptom is
    /// a page that renders with part of the catalogue missing rather than an error.
    #[tokio::test]
    async fn section_data_is_never_pinned_by_the_shipped_policy() {
        let state = test_state().await;
        set_host(&state, "https://client.example").await;

        for path in [
            "/HomeScreen/Sections",
            "/HomeScreen/Section/RecentlyAddedMovies",
            "/HomeScreen/Meta",
            "/HomeScreen/CachedImage/abc",
            "/JellyfinEnhanced/tag-data/user",
            "/Items",
        ] {
            assert_eq!(
                plugin_asset_host(&state, path).await,
                None,
                "{path} returns data and must stay federated"
            );
        }
    }

    /// The tests above prove what the policy decides. This one proves the catch-all actually asks
    /// it: without the call in `proxy_handler`, every assertion above still passes while nothing is
    /// pinned at all, because the routes that used to reach the relay are gone.
    #[tokio::test]
    async fn the_catch_all_relays_a_pinned_asset_to_the_client_host() {
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;
        use http_body_util::BodyExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let client_host = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/MediaBar/slideshowpure.js"))
            .respond_with(ResponseTemplate::new(200).set_body_string("/* from the pinned host */"))
            .expect(1)
            .mount(&client_host)
            .await;

        let state = test_state().await;
        set_host(&state, &client_host.uri()).await;

        let request = AxumRequest::builder()
            .uri("/MediaBar/slideshowpure.js")
            .body(Body::empty())
            .unwrap();

        let response = crate::proxy_handler(State(state), request)
            .await
            .expect("the relay must answer");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            body.as_ref(),
            b"/* from the pinned host */",
            "the body must come from the pinned client host, not the proxy"
        );

        // `expect(1)` above is asserted on drop: the pinned host must have been asked exactly once.
        drop(client_host);
    }

    /// The shipped surfaces must keep working with no configuration beyond the host, since that is
    /// what every existing deployment has.
    #[tokio::test]
    async fn the_shipped_surfaces_are_pinned_out_of_the_box() {
        let state = test_state().await;
        set_host(&state, "https://client.example").await;

        for path in [
            "/MediaBar/slideshowpure.js",
            "/MediaBar/slideshowpure.css",
            "/MediaBar/WebConfig",
            "/PluginPages/inject.js",
            "/CustomTabs/Config",
            "/HomeScreen/home-screen-sections.js",
            "/HomeScreen/home-screen-sections.css",
            "/mediabar/webconfig",
        ] {
            assert!(
                plugin_asset_host(&state, path).await.is_some(),
                "{path} is a shipped plugin asset surface"
            );
        }
    }
}
