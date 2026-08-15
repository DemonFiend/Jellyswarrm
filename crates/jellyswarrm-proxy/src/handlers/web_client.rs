//! Serving the browser's Jellyfin web client, and the plugin surface that rides along with it.
//!
//! A browser runs one single-page application from one origin, so exactly one client build has to
//! serve the whole page. When `web_client_host` is configured, that build comes from a designated
//! origin instead of the copy bundled into the proxy.
//!
//! The important property is that the origin is **pinned**. Previously `/web/*` fell through to the
//! catch-all proxy handler, which re-resolved a server on every request from live health state —
//! and unauthenticated asset requests resolved differently from authenticated API calls. A page
//! load could therefore take `index.html` from one server and a lazily-loaded chunk from another.
//! Since the two builds have different webpack content hashes, the chunk 404s and a whole route or
//! home row silently fails to render, with nothing logged as an error.
//!
//! Pinning to a configured origin makes that unrepresentable: every byte of the application comes
//! from the same place, for the whole page load and every page load.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::Response,
};
use tracing::{debug, error, warn};

use crate::{proxy_headers::is_hop_by_hop_header, AppState};

/// Top-level route prefixes owned by the client-side plugins.
///
/// These are registered by plugins as their own controllers, so they are siblings of `/web` rather
/// than children of it. The injected scripts request them with relative URLs, meaning they arrive
/// at the proxy's origin and have to be forwarded to the host that actually runs the plugin.
///
/// Only asset and configuration surfaces belong here. Endpoints that return media items are data,
/// not chrome, and federating those across servers is a separate concern handled elsewhere — a
/// route added here would be pinned to one server and silently stop merging.
pub const PLUGIN_ASSET_PREFIXES: &[&str] = &[
    "/PluginPages",
    "/MediaBar",
    "/CustomTabs",
    "/HomeScreen/home-screen-sections.js",
    "/HomeScreen/home-screen-sections.css",
];

/// Whether the configured client host should serve this path.
pub fn is_plugin_asset_path(path: &str) -> bool {
    PLUGIN_ASSET_PREFIXES
        .iter()
        .any(|prefix| path.eq_ignore_ascii_case(prefix) || starts_with_segment(path, prefix))
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
/// Falls back to the bundled client when the pinned host cannot be reached. Without that, a host
/// that is merely restarting takes the entire UI down — including the admin page needed to change
/// this setting — which is a worse failure than losing the plugins for a minute.
pub async fn web_client_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    let Some(host) = configured_host(&state).await else {
        // No pinned host: fall through to the bundled client rather than inventing a target.
        return Err(StatusCode::NOT_FOUND);
    };

    let path = req.uri().path().to_string();

    match relay_to_client_host(&state, &host, req).await {
        Ok(response) => Ok(response),
        Err(_) => {
            warn!(
                "Web client host {host} did not answer for {path}; serving the bundled client.                  Plugin UIs will be missing until it returns."
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
        return Err(StatusCode::NOT_FOUND);
    };

    // Guard the pin rather than trusting route registration alone. Pinning a data endpoint here
    // would silently make that surface single-server, which is precisely the defect being removed,
    // and a route added to the wrong list is an easy mistake to make.
    let path = req.uri().path();
    if !is_plugin_asset_path(path) {
        error!("Refusing to pin {path} to the web client host: it is not a plugin asset route");
        return Err(StatusCode::NOT_FOUND);
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

/// Strips headers that would leak the pinned host's identity to the browser.
#[allow(dead_code)]
pub fn scrub_upstream_identity(headers: &mut HeaderMap) {
    for name in ["x-powered-by", "server"] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_asset_paths_match_on_segment_boundaries() {
        assert!(is_plugin_asset_path("/MediaBar"));
        assert!(is_plugin_asset_path("/MediaBar/WebConfig"));
        assert!(is_plugin_asset_path("/mediabar/webconfig"));
        assert!(is_plugin_asset_path("/PluginPages/inject.js"));
        assert!(is_plugin_asset_path("/CustomTabs/Config"));
        assert!(is_plugin_asset_path("/HomeScreen/home-screen-sections.js"));

        // A prefix that merely starts with the same letters is a different route.
        assert!(!is_plugin_asset_path("/MediaBarSomethingElse"));
        assert!(!is_plugin_asset_path("/PluginPagesExtra/thing"));
    }

    /// Section *data* must never be pinned here. These endpoints return media items and have to be
    /// merged across servers; routing them to one host would silently make every row single-server,
    /// which is the exact defect this work exists to remove.
    #[test]
    fn section_data_endpoints_are_not_treated_as_assets() {
        assert!(!is_plugin_asset_path("/HomeScreen/Sections"));
        assert!(!is_plugin_asset_path(
            "/HomeScreen/Section/RecentlyAddedMovies"
        ));
        assert!(!is_plugin_asset_path("/HomeScreen/Meta"));
        assert!(!is_plugin_asset_path("/HomeScreen/CachedImage/abc"));
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
}
