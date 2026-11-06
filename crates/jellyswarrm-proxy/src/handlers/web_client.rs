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

/// The configured plugin asset prefixes.
pub async fn plugin_asset_prefixes(state: &AppState) -> Vec<String> {
    state
        .config
        .read()
        .await
        .plugin_federation
        .plugin_asset_prefixes
        .clone()
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

/// Plugin asset and configuration routes, pinned to the same host as the client.
///
/// These must come from the same origin as the application that requests them — a script tag
/// injected by one server's plugin, answered by a different server's copy of that plugin, is how
/// version skew turns into a silently broken page.
pub async fn plugin_asset_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    let Some(host) = configured_host(&state).await else {
        return crate::proxy_handler(State(state), req).await;
    };

    // Guard the pin rather than trusting route registration alone. Pinning a data endpoint here
    // would silently make that surface single-server, which is precisely the defect being removed,
    // and a route added to the wrong list is an easy mistake to make. Fall through to the ordinary
    // request path rather than failing, so a misfiled route degrades to the unpinned behaviour.
    let prefixes = plugin_asset_prefixes(&state).await;
    let path = req.uri().path();
    if !matches_plugin_prefix(path, &prefixes) {
        error!("Refusing to pin {path} to the web client host: it is not a plugin asset route");
        return crate::proxy_handler(State(state), req).await;
    }

    relay_to_client_host(&state, &host, req).await
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
