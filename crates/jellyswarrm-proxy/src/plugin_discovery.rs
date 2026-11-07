//! Learning which plugin surfaces a server's web client actually serves.
//!
//! The shipped policy in [`crate::config::PluginFederationConfig`] names the surfaces of the
//! plugins that existed when it was written. Install anything else and nothing routes it, because
//! Jellyfin's `/Plugins` endpoint reports a plugin's id, name, version and status but **says
//! nothing about the HTTP routes it registers** — so the inventory cannot tell you that Media Bar
//! answers `/MediaBar/*`.
//!
//! One thing does declare it exactly: the client document the server hands out. File Transformation
//! injects a `<script>` or `<link>` for every asset a plugin contributes, so the entry document is
//! ground truth, and it tracks a plugin's own version changes without a registry to maintain.
//!
//! Appearing there is also *evidence of what the path is*. A reference the browser loads as a
//! script or a stylesheet is presentation by construction, which is what makes it safe to pin
//! automatically. An endpoint a plugin only calls at runtime never proves itself that way, so it is
//! left for an operator to classify — pinning one that returns media items would quietly make that
//! surface single-server, and the symptom is a page that renders with part of the catalogue
//! missing rather than an error.

use std::collections::{BTreeMap, BTreeSet};

use tracing::{debug, info, warn};

use crate::{
    config::{save_config, PluginFederationConfig},
    handlers::web_client::matches_plugin_prefix,
    AppState,
};

/// A plugin's surface, as found in the client document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSurface {
    /// The plugin's namespace: the leading path segment, such as `/MediaBar`.
    pub namespace: String,
    /// The exact asset paths referenced, with query strings stripped.
    pub assets: Vec<String>,
}

/// What a discovery pass found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiscoveryOutcome {
    /// Every plugin surface the client document references.
    pub surfaces: Vec<DiscoveredSurface>,
    /// Asset paths this pass added to `plugin_asset_prefixes`.
    pub enabled: Vec<String>,
    /// Namespaces that were previously unknown. Their assets now work; whether their *data*
    /// endpoints should be pinned, federated or left alone is an operator decision.
    pub awaiting_classification: Vec<String>,
}

/// The plugin asset paths a jellyfin-web entry document references.
///
/// References resolve against `/web/`, which is where the client is served from: `../MediaBar/x.js`
/// and `/MediaBar/x.js` both mean `/MediaBar/x.js`, while a bare `main.jellyfin.bundle.js` means
/// `/web/main.jellyfin.bundle.js` and belongs to the client itself.
pub fn surfaces_in_client_html(html: &str) -> Vec<DiscoveredSurface> {
    let mut by_namespace: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    let mut references = attribute_values(html, "src");
    references.extend(attribute_values(html, "href"));

    for reference in references {
        let Some(path) = resolve_against_web_root(reference) else {
            continue;
        };

        // The client's own bundles live under the web root. They are the one thing in this document
        // that is certainly not a plugin's.
        if path.len() > 5 && path[..5].eq_ignore_ascii_case("/web/") {
            continue;
        }

        let mut segments = path.trim_start_matches('/').split('/');
        let Some(namespace) = segments.next().filter(|segment| !segment.is_empty()) else {
            continue;
        };
        // A plugin surface is always `/Namespace/file`. A single segment is a site-level document
        // such as `/favicon.ico` or `/manifest.json`, which belongs to no plugin and must never be
        // pinned to one server.
        if segments.next().is_none() {
            continue;
        }

        by_namespace
            .entry(format!("/{namespace}"))
            .or_default()
            .insert(path);
    }

    by_namespace
        .into_iter()
        .map(|(namespace, assets)| DiscoveredSurface {
            namespace,
            assets: assets.into_iter().collect(),
        })
        .collect()
}

/// The values of every `attr="…"` in `html`.
///
/// Deliberately a scan rather than a parse. The input is one known document shape and the only
/// thing being extracted is attribute values, so a dependency on an HTML parser buys nothing — but
/// the attribute name still has to match at a boundary, or `data-src=` would be read as `src=`.
fn attribute_values<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let needle = format!("{attr}=");
    let mut values = Vec::new();
    let mut offset = 0usize;

    while let Some(found) = html[offset..].find(needle.as_str()) {
        let name_start = offset + found;
        let value_start = name_start + needle.len();
        offset = value_start;

        if !html[..name_start]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
        {
            continue;
        }

        let tail = &html[value_start..];
        let Some(quote) = tail.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let body = &tail[quote.len_utf8()..];
        let Some(end) = body.find(quote) else {
            continue;
        };

        values.push(&body[..end]);
        offset = value_start + quote.len_utf8() + end + quote.len_utf8();
    }

    values
}

/// Resolves a document reference to an absolute path on this proxy, or `None` if it does not name
/// one.
fn resolve_against_web_root(reference: &str) -> Option<String> {
    let reference = reference.split(['?', '#']).next().unwrap_or_default();
    if reference.is_empty() {
        return None;
    }
    // Not a path on this proxy: another origin, or an inline payload.
    if reference.starts_with("//") || reference.contains("://") || reference.starts_with("data:") {
        return None;
    }

    let joined = if reference.starts_with('/') {
        reference.to_string()
    } else {
        format!("/web/{reference}")
    };

    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }

    (!segments.is_empty()).then(|| format!("/{}", segments.join("/")))
}

/// Whether any of the federation policy lists already govern `path`.
///
/// All three are consulted, not just the asset list, because a path already classified as a plugin
/// **API** must not be re-classified as an asset. An asset is relayed verbatim, which sends the
/// proxy's own access token — harmless for a static file, wrong for anything that identifies the
/// caller, which is exactly what Jellyfin Enhanced's endpoints do.
fn already_classified(path: &str, federation: &PluginFederationConfig) -> bool {
    matches_plugin_prefix(path, &federation.plugin_asset_prefixes)
        || matches_plugin_prefix(path, &federation.plugin_api_prefixes)
        || matches_plugin_prefix(path, &federation.federated_plugin_api_prefixes)
}

/// What a pass should do about the surfaces it found, given the policy already in force.
///
/// Returns the asset paths to pin and the namespaces whose data endpoints still need an operator
/// decision. Kept pure so the decision can be tested without reaching the network or writing a
/// config file - `DATA_DIR` resolves once per process, so a test cannot redirect where
/// `save_config` writes.
///
/// Exact paths are pinned, never the namespace. That distinction is load-bearing: `/HomeScreen`
/// holds both `home-screen-sections.js` and `/HomeScreen/Sections`, and the latter returns media
/// items, so pinning the namespace would quietly reduce every home row to a single server.
fn classify_surfaces(
    surfaces: &[DiscoveredSurface],
    federation: &PluginFederationConfig,
) -> (Vec<String>, Vec<String>) {
    let mut to_pin = Vec::new();
    let mut to_classify = Vec::new();

    for surface in surfaces {
        let unknown: Vec<String> = surface
            .assets
            .iter()
            .filter(|asset| !already_classified(asset, federation))
            .cloned()
            .collect();

        if unknown.is_empty() {
            continue;
        }

        // The namespace was unknown, so its data endpoints have no policy either. Its assets are
        // safe to pin because the document proves what they are; its API is not, so say so.
        to_classify.push(surface.namespace.clone());
        to_pin.extend(unknown);
    }

    (to_pin, to_classify)
}

/// Fetches the client entry document from the pinned host.
pub async fn fetch_client_html(state: &AppState) -> Option<String> {
    let host = crate::handlers::web_client::configured_host(state).await?;
    let url = format!("{host}/web/index.html");

    let response = match state.reqwest_client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            warn!("Cannot read the client document from {url} to discover plugins: {error}");
            return None;
        }
    };
    if !response.status().is_success() {
        warn!(
            "{url} answered {} when asked for the client document; skipping plugin discovery",
            response.status()
        );
        return None;
    }

    response.text().await.ok()
}

/// Reads the pinned host's client document and pins any plugin asset it references that no policy
/// list covers yet.
///
/// Does nothing at all when no host is pinned: there is no client document to read, and no
/// client-side plugin exists in that configuration anyway.
pub async fn discover_and_enable_assets(state: &AppState) -> DiscoveryOutcome {
    let Some(html) = fetch_client_html(state).await else {
        return DiscoveryOutcome::default();
    };

    let surfaces = surfaces_in_client_html(&html);
    if surfaces.is_empty() {
        debug!(
            "The pinned client references no plugin assets; either nothing is installed or \
             File Transformation is not injecting"
        );
        return DiscoveryOutcome::default();
    }

    let mut config_guard = state.config.write().await;
    let mut updated = config_guard.clone();

    let (enabled, awaiting_classification) =
        classify_surfaces(&surfaces, &updated.plugin_federation);

    for asset in &enabled {
        info!("Pinning newly discovered plugin asset {asset} to the web client host");
        updated
            .plugin_federation
            .plugin_asset_prefixes
            .push(asset.clone());
    }

    if enabled.is_empty() {
        debug!(
            "Plugin discovery found {} surface(s), all already covered by policy",
            surfaces.len()
        );
        return DiscoveryOutcome {
            surfaces,
            ..Default::default()
        };
    }

    match save_config(&updated) {
        Ok(()) => {
            *config_guard = updated;
            info!(
                "Plugin discovery pinned {} new asset path(s); {} namespace(s) need their data \
                 endpoints classified: {}",
                enabled.len(),
                awaiting_classification.len(),
                awaiting_classification.join(", ")
            );
        }
        Err(error) => {
            // The in-memory config is left untouched, so the next pass retries rather than
            // routing against a policy that was never written down.
            warn!("Discovered plugin assets could not be saved: {error}");
            return DiscoveryOutcome {
                surfaces,
                ..Default::default()
            };
        }
    }

    DiscoveryOutcome {
        surfaces,
        enabled,
        awaiting_classification,
    }
}

/// How the policy treats a plugin's own endpoints - as distinct from its assets, which the client
/// document can classify by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPolicy {
    /// Answered by the server that served the client.
    Pinned,
    /// Fanned out to every server and merged.
    Merged,
    /// No policy of its own, so its endpoints take the ordinary federated path.
    Unclassified,
}

impl DataPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned to the web client host",
            Self::Merged => "Merged across all servers",
            Self::Unclassified => "Not classified",
        }
    }
}

/// A discovered surface, reported against the policy currently in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceStatus {
    pub namespace: String,
    /// Every asset path the client document references under this namespace.
    pub assets: Vec<String>,
    /// Those with no policy yet. Empty once a pass has run, so a non-empty list means the pinned
    /// host changed since the last pass or saving the policy failed.
    pub unpinned_assets: Vec<String>,
    /// How this namespace's own endpoints are treated.
    pub data_policy: DataPolicy,
    /// Paths under this namespace that are federated regardless of the namespace's own policy -
    /// how Jellyfin Enhanced ends up pinned for its scripts and merged for its tag data.
    pub merged_paths: Vec<String>,
    /// The server an explicit route sends this namespace to, overriding `data_policy`.
    pub routed_server: Option<String>,
}

/// How the policy in force treats `namespace`.
fn data_policy_for(namespace: &str, federation: &PluginFederationConfig) -> DataPolicy {
    if matches_plugin_prefix(namespace, &federation.federated_plugin_api_prefixes) {
        DataPolicy::Merged
    } else if matches_plugin_prefix(namespace, &federation.plugin_api_prefixes) {
        DataPolicy::Pinned
    } else {
        DataPolicy::Unclassified
    }
}

/// Federated prefixes that live under `namespace`.
fn merged_paths_under(namespace: &str, federation: &PluginFederationConfig) -> Vec<String> {
    federation
        .federated_plugin_api_prefixes
        .iter()
        .filter(|prefix| {
            prefix.len() > namespace.len()
                && prefix.as_bytes()[namespace.len()] == b'/'
                && prefix[..namespace.len()].eq_ignore_ascii_case(namespace)
        })
        .cloned()
        .collect()
}

/// The server an explicit route sends `namespace` to, if one covers the namespace itself.
///
/// A route on a path *inside* the namespace is deliberately not reported here: it does not describe
/// where the plugin goes, only where one of its endpoints does, and conflating the two would make
/// the row say something false about the rest of the plugin.
fn routed_server_for(namespace: &str, federation: &PluginFederationConfig) -> Option<String> {
    federation
        .plugin_server_routes
        .iter()
        .filter(|route| matches_plugin_prefix(namespace, std::slice::from_ref(&route.prefix)))
        .max_by_key(|route| route.prefix.len())
        .map(|route| route.server.clone())
}

/// Reads the client document and reports every surface against the policy in force.
///
/// Read-only: it writes no configuration, so it is safe to call from a page load. Enabling assets
/// is [`discover_and_enable_assets`], which runs at start-up and from an explicit action.
pub async fn survey(state: &AppState) -> Vec<SurfaceStatus> {
    let Some(html) = fetch_client_html(state).await else {
        return Vec::new();
    };

    let federation = state.config.read().await.plugin_federation.clone();

    surfaces_in_client_html(&html)
        .into_iter()
        .map(|surface| {
            let unpinned_assets = surface
                .assets
                .iter()
                .filter(|asset| !already_classified(asset, &federation))
                .cloned()
                .collect();

            SurfaceStatus {
                data_policy: data_policy_for(&surface.namespace, &federation),
                merged_paths: merged_paths_under(&surface.namespace, &federation),
                routed_server: routed_server_for(&surface.namespace, &federation),
                unpinned_assets,
                namespace: surface.namespace,
                assets: surface.assets,
            }
        })
        .collect()
}

/// Sends a plugin surface to a named server, or clears the route when `server` is empty.
///
/// The server is checked against the configured ones before saving, because the failure mode of a
/// name that does not resolve is silent: the route is ignored at request time and the surface falls
/// back to the default policy, which looks like the setting never took.
pub async fn set_server_route(state: &AppState, prefix: &str, server: &str) -> Result<(), String> {
    let prefix = prefix.trim();
    if prefix.is_empty() || !prefix.starts_with('/') {
        return Err(format!(
            "{prefix:?} is not a path prefix; expected one starting with /"
        ));
    }
    let server = server.trim();

    if !server.is_empty() {
        match state.server_storage.get_server_by_name(server).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(format!("No server named {server:?} is configured")),
            Err(error) => return Err(format!("Could not check that server: {error}")),
        }
    }

    let mut config_guard = state.config.write().await;
    let mut updated = config_guard.clone();
    let routes = &mut updated.plugin_federation.plugin_server_routes;

    routes.retain(|route| !route.prefix.eq_ignore_ascii_case(prefix));
    if !server.is_empty() {
        routes.push(crate::config::PluginServerRoute {
            prefix: prefix.to_string(),
            server: server.to_string(),
        });
    }

    save_config(&updated).map_err(|error| format!("Could not save the route: {error}"))?;
    *config_guard = updated;

    if server.is_empty() {
        info!("Cleared the server route for {prefix}");
    } else {
        info!("Routed {prefix} to {server}");
    }

    Ok(())
}

/// Classifies a namespace's own endpoints.
///
/// Additive: the namespace is appended to whichever list the operator chose and removed from the
/// other, so repeating a choice is a no-op and changing one's mind takes effect. Assets are
/// untouched - they are classified by the document, not by this.
pub async fn classify_namespace(
    state: &AppState,
    namespace: &str,
    policy: DataPolicy,
) -> Result<(), String> {
    let namespace = namespace.trim();
    if namespace.is_empty() || !namespace.starts_with('/') || namespace[1..].contains('/') {
        return Err(format!(
            "{namespace:?} is not a plugin namespace; expected a single leading segment such as              /MediaBar"
        ));
    }

    let mut config_guard = state.config.write().await;
    let mut updated = config_guard.clone();
    let federation = &mut updated.plugin_federation;

    let drop_from = |list: &mut Vec<String>| {
        list.retain(|existing| !existing.eq_ignore_ascii_case(namespace));
    };

    drop_from(&mut federation.plugin_api_prefixes);
    drop_from(&mut federation.federated_plugin_api_prefixes);

    match policy {
        DataPolicy::Pinned => federation.plugin_api_prefixes.push(namespace.to_string()),
        DataPolicy::Merged => federation
            .federated_plugin_api_prefixes
            .push(namespace.to_string()),
        // Leaving it unclassified is the absence of both entries, which the removals above achieve.
        DataPolicy::Unclassified => {}
    }

    save_config(&updated).map_err(|error| format!("Could not save the policy: {error}"))?;
    *config_guard = updated;
    info!("Classified {namespace} as {}", policy.label());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful reduction of what Jellyfin 12.4 plus the IAmParadox27 plugins and Jellyfin
    /// Enhanced actually serve: the plugin references are relative or root-relative, some carry
    /// cache-busting queries, some have no file extension at all, and the client's own bundles sit
    /// alongside them with no prefix.
    const REAL_CLIENT_HTML: &str = r##"<!DOCTYPE html>
<html><head>
<link rel="manifest" href="manifest.json">
<link rel="apple-touch-icon" href="/favicon.ico">
<link rel="stylesheet" href="../MediaBar/slideshowpure.css">
<link rel="stylesheet" href="/HomeScreen/home-screen-sections.css?v=3.0.4.0&c=14">
<script src="node_modules.jellyfin-apiclient.bundle.js?d66e3c0638b9381c83a7" defer></script>
<script src="main.jellyfin.bundle.js?d66e3c0638b9381c83a7" defer></script>
<script data-src="/NotReal/decoy.js"></script>
<script src="https://cdn.jsdelivr.net/gh/someone/thing@main/thing.js"></script>
<script src='../JellyTweaks/script'></script>
<script src="../JellyfinEnhanced/script?v=12.14.0.0-639286215860000000"></script>
<script src="../MediaBar/slideshowpure.js"></script>
<script src="/HomeScreen/home-screen-sections.js?v=3.0.4.0&c=14"></script>
<script src="/PluginPages/inject.js?v=3.0.3.0"></script>
</head><body></body></html>"##;

    fn namespaces(html: &str) -> Vec<String> {
        surfaces_in_client_html(html)
            .into_iter()
            .map(|surface| surface.namespace)
            .collect()
    }

    #[test]
    fn the_real_client_document_yields_exactly_the_plugin_namespaces() {
        assert_eq!(
            namespaces(REAL_CLIENT_HTML),
            vec![
                "/HomeScreen",
                "/JellyTweaks",
                "/JellyfinEnhanced",
                "/MediaBar",
                "/PluginPages",
            ]
        );
    }

    /// Relative and root-relative references have to land on the same path, since plugins use both
    /// in the same document, and the query string is a cache-buster rather than part of the route.
    #[test]
    fn references_resolve_to_exact_paths_without_their_queries() {
        let surfaces = surfaces_in_client_html(REAL_CLIENT_HTML);
        let by_namespace: BTreeMap<_, _> = surfaces
            .iter()
            .map(|s| (s.namespace.as_str(), &s.assets))
            .collect();

        assert_eq!(
            by_namespace["/MediaBar"],
            &vec![
                "/MediaBar/slideshowpure.css".to_string(),
                "/MediaBar/slideshowpure.js".to_string(),
            ],
            "`../MediaBar/x` resolves against /web/ to /MediaBar/x"
        );
        assert_eq!(
            by_namespace["/HomeScreen"],
            &vec![
                "/HomeScreen/home-screen-sections.css".to_string(),
                "/HomeScreen/home-screen-sections.js".to_string(),
            ],
            "the ?v= cache-buster is not part of the route"
        );
        assert_eq!(
            by_namespace["/JellyfinEnhanced"],
            &vec!["/JellyfinEnhanced/script".to_string()],
            "a plugin asset need not look like a file"
        );
    }

    /// The client's own bundles are the bulk of the document and must never be mistaken for a
    /// plugin's: pinning them would be pinning the application to itself.
    #[test]
    fn the_clients_own_bundles_are_not_plugin_surfaces() {
        let found = namespaces(REAL_CLIENT_HTML);

        assert!(!found.iter().any(|n| n.eq_ignore_ascii_case("/web")));
        for bundle in ["main.jellyfin", "node_modules", "runtime"] {
            assert!(
                !found.iter().any(|n| n.contains(bundle)),
                "{bundle} belongs to the client"
            );
        }
    }

    /// A single-segment path is a site-level document. Pinning `/favicon.ico` to one server would
    /// be harmless; pinning `/manifest.json` or some future root document would not, and neither
    /// belongs to a plugin.
    #[test]
    fn site_level_documents_belong_to_no_plugin() {
        let found = namespaces(REAL_CLIENT_HTML);

        assert!(!found.iter().any(|n| n == "/favicon.ico"));
        assert!(!found.iter().any(|n| n == "/manifest.json"));
    }

    /// Another origin's asset is not a route on this proxy, and must not become a prefix.
    #[test]
    fn other_origins_and_inline_payloads_are_ignored() {
        assert!(!namespaces(REAL_CLIENT_HTML)
            .iter()
            .any(|n| n.contains("cdn") || n.contains("jsdelivr")));

        for reference in [
            "https://cdn.example/x.js",
            "//cdn.example/x.js",
            "data:text/javascript,void 0",
            "#",
            "",
        ] {
            assert_eq!(
                resolve_against_web_root(reference),
                None,
                "{reference} does not name a path on this proxy"
            );
        }
    }

    /// The attribute name has to match at a boundary. `data-src` is a real pattern in lazy-loading
    /// markup, and reading it as `src` would invent a prefix from something the browser never
    /// fetches.
    #[test]
    fn an_attribute_that_merely_ends_in_src_is_not_src() {
        assert!(
            !namespaces(REAL_CLIENT_HTML).iter().any(|n| n == "/NotReal"),
            "data-src must not be read as src"
        );
        assert_eq!(
            attribute_values(r#"<img data-src="/a/b.js">"#, "src"),
            Vec::<&str>::new()
        );
        assert_eq!(
            attribute_values(r#"<img src="/a/b.js">"#, "src"),
            vec!["/a/b.js"]
        );
    }

    /// Both quote styles appear in generated markup.
    #[test]
    fn single_and_double_quoted_values_are_both_read() {
        assert!(namespaces(REAL_CLIENT_HTML)
            .iter()
            .any(|n| n == "/JellyTweaks"));
    }

    /// Nothing in the shipped policy may be re-classified by a discovery pass. Enhanced's
    /// endpoints identify the caller, so they live in the API list; adding `/JellyfinEnhanced/script`
    /// to the asset list would relay it verbatim with the proxy's own token.
    #[test]
    fn the_shipped_policy_already_covers_every_shipped_surface() {
        let federation = PluginFederationConfig::default();

        for surface in surfaces_in_client_html(REAL_CLIENT_HTML) {
            for asset in &surface.assets {
                assert!(
                    already_classified(asset, &federation),
                    "{asset} is a shipped surface and must not be re-discovered"
                );
            }
        }
    }

    /// The case a discovery pass exists for: a plugin the shipped policy has never heard of.
    #[test]
    fn an_unknown_plugin_is_discovered_as_an_asset_and_flagged_for_classification() {
        let html = r#"<script src="../BrandNewPlugin/widget.js"></script>
                      <link rel="stylesheet" href="/BrandNewPlugin/widget.css">"#;
        let federation = PluginFederationConfig::default();

        let surfaces = surfaces_in_client_html(html);

        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].namespace, "/BrandNewPlugin");
        assert_eq!(
            surfaces[0].assets,
            vec![
                "/BrandNewPlugin/widget.css".to_string(),
                "/BrandNewPlugin/widget.js".to_string()
            ]
        );
        for asset in &surfaces[0].assets {
            assert!(
                !already_classified(asset, &federation),
                "{asset} is exactly what a pass should pin"
            );
        }
    }

    /// Exact paths are pinned rather than the namespace, and that distinction is load-bearing.
    /// `/HomeScreen` holds both `home-screen-sections.js` and `/HomeScreen/Sections`, which returns
    /// media items and has to stay federated — so pinning the namespace would silently reduce every
    /// home row to one server.
    #[test]
    fn pinning_is_per_path_so_a_namespace_can_hold_both_assets_and_data() {
        let federation = PluginFederationConfig::default();

        assert!(already_classified(
            "/HomeScreen/home-screen-sections.js",
            &federation
        ));
        assert!(
            !already_classified("/HomeScreen/Sections", &federation),
            "section data must remain unclassified as an asset"
        );
        assert!(
            !already_classified("/HomeScreen/Section/RecentlyAddedMovies", &federation),
            "section data must remain unclassified as an asset"
        );
    }

    /// Nothing to do on a deployment running only the plugins the policy already names - which is
    /// every deployment that has not installed anything new, and the case a pass runs most often.
    #[test]
    fn a_pass_over_only_shipped_surfaces_changes_nothing() {
        let federation = PluginFederationConfig::default();
        let surfaces = surfaces_in_client_html(REAL_CLIENT_HTML);

        let (to_pin, to_classify) = classify_surfaces(&surfaces, &federation);

        assert!(to_pin.is_empty(), "nothing to pin: {to_pin:?}");
        assert!(
            to_classify.is_empty(),
            "nothing to ask about: {to_classify:?}"
        );
    }

    /// The case the pass exists for.
    #[test]
    fn an_unknown_plugins_assets_are_pinned_and_its_namespace_flagged() {
        let federation = PluginFederationConfig::default();
        let surfaces = surfaces_in_client_html(
            r#"<script src="../BrandNewPlugin/widget.js"></script>
               <link rel="stylesheet" href="/BrandNewPlugin/widget.css">"#,
        );

        let (to_pin, to_classify) = classify_surfaces(&surfaces, &federation);

        assert_eq!(
            to_pin,
            vec![
                "/BrandNewPlugin/widget.css".to_string(),
                "/BrandNewPlugin/widget.js".to_string()
            ],
            "the exact paths, not the namespace"
        );
        assert_eq!(
            to_classify,
            vec!["/BrandNewPlugin".to_string()],
            "its data endpoints are still an operator decision"
        );
    }

    /// A path already governed as a plugin **API** must never be re-pinned as an asset. An asset is
    /// relayed verbatim, which sends the proxy's own access token; Jellyfin Enhanced's endpoints
    /// identify the caller, so doing that would authenticate every user as nobody.
    #[test]
    fn a_path_already_governed_as_an_api_is_not_repinned_as_an_asset() {
        let federation = PluginFederationConfig::default();
        let surfaces =
            surfaces_in_client_html(r#"<script src="../JellyfinEnhanced/script"></script>"#);

        let (to_pin, to_classify) = classify_surfaces(&surfaces, &federation);

        assert!(
            to_pin.is_empty(),
            "an API surface must not be relayed verbatim: {to_pin:?}"
        );
        assert!(to_classify.is_empty(), "and it is already classified");
    }

    /// A plugin that gains a second asset in a later version: only the new path is pinned, and the
    /// namespace is not re-flagged, since its data endpoints were classified when it was first seen.
    #[test]
    fn only_the_unknown_paths_of_a_known_plugin_are_pinned() {
        let federation = PluginFederationConfig {
            plugin_asset_prefixes: vec!["/HomeScreen/home-screen-sections.js".to_string()],
            ..Default::default()
        };

        let surfaces = surfaces_in_client_html(
            r#"<script src="/HomeScreen/home-screen-sections.js"></script>
               <link rel="stylesheet" href="/HomeScreen/home-screen-sections.css">"#,
        );

        let (to_pin, _) = classify_surfaces(&surfaces, &federation);

        assert_eq!(
            to_pin,
            vec!["/HomeScreen/home-screen-sections.css".to_string()],
            "the already-pinned script must not be added twice"
        );
    }

    /// Section data must survive a pass untouched however often it runs, because the pass sees the
    /// namespace in the document every time.
    #[test]
    fn repeated_passes_never_pin_section_data() {
        let mut federation = PluginFederationConfig::default();
        let surfaces = surfaces_in_client_html(REAL_CLIENT_HTML);

        for _ in 0..3 {
            let (to_pin, _) = classify_surfaces(&surfaces, &federation);
            federation.plugin_asset_prefixes.extend(to_pin);
        }

        for data_path in [
            "/HomeScreen/Sections",
            "/HomeScreen/Section/RecentlyAddedMovies",
            "/HomeScreen/Meta",
        ] {
            assert!(
                !matches_plugin_prefix(data_path, &federation.plugin_asset_prefixes),
                "{data_path} must never become a pinned asset"
            );
        }
    }

    /// Enhanced is the shape that makes a single label per namespace insufficient: its scripts are
    /// pinned to the client host while its tag data is merged across servers, so the report has to
    /// show both.
    #[test]
    fn a_namespace_can_be_pinned_while_paths_under_it_are_merged() {
        let federation = PluginFederationConfig::default();

        assert_eq!(
            data_policy_for("/JellyfinEnhanced", &federation),
            DataPolicy::Pinned
        );
        assert_eq!(
            merged_paths_under("/JellyfinEnhanced", &federation),
            vec![
                "/JellyfinEnhanced/tag-cache".to_string(),
                "/JellyfinEnhanced/tag-data".to_string(),
            ]
        );
    }

    /// A plugin whose assets are pinned but whose endpoints have no policy must read as
    /// unclassified, because that is the decision an operator is being asked for.
    #[test]
    fn an_asset_only_namespace_is_unclassified() {
        let federation = PluginFederationConfig::default();

        // MediaBar's assets are pinned by the shipped policy, but it has no API entry.
        assert_eq!(
            data_policy_for("/MediaBar", &federation),
            DataPolicy::Unclassified
        );
        assert!(merged_paths_under("/MediaBar", &federation).is_empty());
    }

    /// A namespace must not be reported as merged because some unrelated prefix merely starts with
    /// the same letters.
    #[test]
    fn merged_paths_match_on_a_segment_boundary() {
        let federation = PluginFederationConfig {
            federated_plugin_api_prefixes: vec![
                "/Tags/data".to_string(),
                "/TagsExtra/data".to_string(),
            ],
            ..Default::default()
        };

        assert_eq!(
            merged_paths_under("/Tags", &federation),
            vec!["/Tags/data".to_string()]
        );
    }

    /// A route on the namespace describes where the plugin goes; a route on a path inside it does
    /// not, and must not be reported as though it did.
    #[test]
    fn only_a_route_covering_the_namespace_describes_the_namespace() {
        let federation = PluginFederationConfig {
            plugin_server_routes: vec![crate::config::PluginServerRoute {
                prefix: "/JellyfinEnhanced/requests".to_string(),
                server: "Seerr Host".to_string(),
            }],
            ..Default::default()
        };

        assert_eq!(
            routed_server_for("/JellyfinEnhanced", &federation),
            None,
            "one endpoint being routed says nothing about the plugin"
        );

        let federation = PluginFederationConfig {
            plugin_server_routes: vec![crate::config::PluginServerRoute {
                prefix: "/JellyfinEnhanced".to_string(),
                server: "Seerr Host".to_string(),
            }],
            ..Default::default()
        };

        assert_eq!(
            routed_server_for("/JellyfinEnhanced", &federation),
            Some("Seerr Host".to_string())
        );
    }
}
